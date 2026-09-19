//! Operation ids and receipts: a retry either joins the operation it repeats
//! or gets the receipt of what already happened (design §7).
//!
//! The ledger is deliberately in memory and scoped to one host generation. It
//! refuses a new operation rather than evicting a receipt, because an evicted
//! receipt would make the next retry of that id look new and re-apply it
//! (design §7.2: "ID受付枠を公開前に確保し、記録を安全に保持できなければ新操作を
//! 拒否する").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::error::{ExpError, ExpErrorCode};
use super::refs::OwnerId;
use super::store::Revision;

/// The caller's id for one logical operation. A provider retry that re-sends
/// the same tool call keeps it, so the retry is recognized (design §7.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an applied operation reports back: the new revision and a short
/// summary. The diff is *not* part of it — that is what artifacts and staged
/// fetching are for (design §6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub operation_id: OperationId,
    pub path: String,
    pub revision: Revision,
    /// Replacements applied (one per reference).
    pub edits_applied: usize,
    /// Bytes written for the whole file.
    pub bytes_written: usize,
}

type Outcome = Result<Receipt, ExpError>;

enum SlotState {
    InFlight {
        /// Fingerprint of the arguments this id was first reserved with.
        args: u64,
        done: watch::Sender<Option<Outcome>>,
    },
    Done {
        args: u64,
        outcome: Outcome,
    },
}

/// The reservation of an operation id: the holder must report an outcome, and
/// a dropped reservation becomes [`ExpErrorCode::OutcomeUnknown`] rather than
/// an implied success (design §7.1).
pub enum Reservation {
    /// This call owns the operation; it must complete or fail it.
    Fresh(FreshOperation),
    /// Another call already ran (or is running) this id with the same
    /// arguments: here is its outcome.
    Joined(Outcome),
}

/// The rights and duties of a fresh reservation.
pub struct FreshOperation {
    ledger: Arc<OperationLedger>,
    key: (String, String),
    finished: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh(_) => f.write_str("Reservation::Fresh"),
            Self::Joined(outcome) => write!(f, "Reservation::Joined({outcome:?})"),
        }
    }
}

impl FreshOperation {
    /// Record a successful publication and hand the receipt back.
    pub fn complete(mut self, receipt: Receipt) -> Receipt {
        self.finish(Ok(receipt.clone()));
        receipt
    }

    /// Record a definite failure — including a cancellation before publication,
    /// which is a definite "no change" (design §7.1).
    pub fn fail(mut self, error: ExpError) -> ExpError {
        self.finish(Err(error.clone()));
        error
    }

    fn finish(&mut self, outcome: Outcome) {
        self.finished = true;
        self.ledger.finish(&self.key, outcome);
    }
}

impl Drop for FreshOperation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.ledger.finish(
            &self.key,
            Err(ExpError::new(
                ExpErrorCode::OutcomeUnknown,
                "the operation was abandoned before it reported an outcome",
            )
            .with_repair("check the target, then retry with a new operation id")),
        );
    }
}

/// The operation ledger of one host generation.
pub struct OperationLedger {
    slots: Mutex<HashMap<(String, String), SlotState>>,
    capacity: usize,
}

impl OperationLedger {
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.slots.lock().expect("ledger lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reserve `operation` for `owner`.
    ///
    /// - unknown id: the caller gets a fresh reservation;
    /// - known id, same argument fingerprint: the caller joins the outcome
    ///   (awaiting the in-flight operation when needed) and never publishes;
    /// - known id, different fingerprint: [`ExpErrorCode::OperationIdMismatch`];
    /// - full ledger: [`ExpErrorCode::BudgetExceeded`], with nothing reserved.
    pub async fn reserve(
        self: &Arc<Self>,
        owner: &OwnerId,
        operation: &OperationId,
        args_digest: u64,
    ) -> Result<Reservation, ExpError> {
        let key = (owner.as_str().to_string(), operation.as_str().to_string());
        let waiting = {
            let mut slots = self.slots.lock().expect("ledger lock");
            match slots.get(&key) {
                Some(SlotState::Done { args, outcome }) => {
                    if *args != args_digest {
                        return Err(operation_id_mismatch());
                    }
                    return Ok(Reservation::Joined(outcome.clone()));
                }
                Some(SlotState::InFlight { args, done }) => {
                    if *args != args_digest {
                        return Err(operation_id_mismatch());
                    }
                    let rx = done.subscribe();
                    // Re-check before awaiting: the operation may have
                    // completed while we were looking up the slot.
                    if let Some(outcome) = rx.borrow().clone() {
                        return Ok(Reservation::Joined(outcome));
                    }
                    rx
                }
                None => {
                    if slots.len() >= self.capacity {
                        return Err(ExpError::budget_exceeded(
                            format!("more than {} recorded operations", self.capacity),
                            "retry with a new operation id, or raise the ledger budget",
                        ));
                    }
                    let (done, _) = watch::channel(None);
                    slots.insert(
                        key.clone(),
                        SlotState::InFlight {
                            args: args_digest,
                            done,
                        },
                    );
                    return Ok(Reservation::Fresh(FreshOperation {
                        ledger: Arc::clone(self),
                        key,
                        finished: false,
                    }));
                }
            }
        };
        let mut rx = waiting;
        let outcome = match rx.wait_for(|value| value.is_some()).await {
            Ok(guard) => guard.clone().expect("checked is_some"),
            // The sender went away without recording an outcome: that is
            // unknown, never success.
            Err(_) => Err(ExpError::new(
                ExpErrorCode::OutcomeUnknown,
                "the operation stopped without reporting an outcome",
            )
            .with_repair("check the target, then retry with a new operation id")),
        };
        Ok(Reservation::Joined(outcome))
    }

    fn finish(&self, key: &(String, String), outcome: Outcome) {
        let mut slots = self.slots.lock().expect("ledger lock");
        let Some(slot) = slots.remove(key) else {
            return;
        };
        if let SlotState::InFlight { done, .. } = &slot {
            // Publish to the joiners before the sender is dropped, so a
            // waiter observes the outcome rather than a closed channel.
            let _ = done.send(Some(outcome.clone()));
        }
        let args = match slot {
            SlotState::InFlight { args, .. } | SlotState::Done { args, .. } => args,
        };
        slots.insert(key.clone(), SlotState::Done { args, outcome });
    }
}

fn operation_id_mismatch() -> ExpError {
    ExpError::new(
        ExpErrorCode::OperationIdMismatch,
        "this operation id was already used with different arguments",
    )
    .with_repair("use a new operation id for a different edit")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(id: &str) -> Receipt {
        Receipt {
            operation_id: OperationId::new(id),
            path: "f.txt".to_string(),
            revision: Revision::new(2, b"two\n"),
            edits_applied: 1,
            bytes_written: 4,
        }
    }

    #[tokio::test]
    async fn a_duplicate_id_returns_the_first_receipt() {
        let ledger = Arc::new(OperationLedger::new(8));
        let owner = OwnerId::new("s1");
        let id = OperationId::new("op-1");

        let Reservation::Fresh(fresh) = ledger.reserve(&owner, &id, 7).await.expect("fresh") else {
            panic!("expected a fresh reservation");
        };
        fresh.complete(receipt("op-1"));

        let Reservation::Joined(outcome) = ledger.reserve(&owner, &id, 7).await.expect("joined")
        else {
            panic!("expected a join");
        };
        assert_eq!(outcome.expect("receipt"), receipt("op-1"));

        let mismatch = ledger.reserve(&owner, &id, 8).await.expect_err("mismatch");
        assert_eq!(mismatch.code, ExpErrorCode::OperationIdMismatch);
    }

    #[tokio::test]
    async fn a_dropped_reservation_is_unknown_not_success() {
        let ledger = Arc::new(OperationLedger::new(8));
        let owner = OwnerId::new("s1");
        let id = OperationId::new("op-2");

        {
            let _fresh = ledger.reserve(&owner, &id, 1).await.expect("fresh");
        }
        let Reservation::Joined(outcome) = ledger.reserve(&owner, &id, 1).await.expect("joined")
        else {
            panic!("expected a join");
        };
        assert_eq!(
            outcome.expect_err("unknown").code,
            ExpErrorCode::OutcomeUnknown
        );
    }

    #[tokio::test]
    async fn a_full_ledger_refuses_a_new_operation() {
        let ledger = Arc::new(OperationLedger::new(1));
        let owner = OwnerId::new("s1");
        let first = OperationId::new("op-3");
        let Reservation::Fresh(fresh) = ledger.reserve(&owner, &first, 1).await.expect("fresh")
        else {
            panic!("expected a fresh reservation");
        };
        fresh.complete(receipt("op-3"));

        let refused = ledger
            .reserve(&owner, &OperationId::new("op-4"), 1)
            .await
            .expect_err("full");
        assert_eq!(refused.code, ExpErrorCode::BudgetExceeded);
    }
}
