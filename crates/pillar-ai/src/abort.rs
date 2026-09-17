//! Port of packages/ai/src/utils/abort.ts (pi v0.84.3) — abort-signal
//! plumbing for cancellable operations, in tokio terms.
//!
//! divergence: upstream uses the DOM AbortSignal/AbortController; the Rust
//! port uses a lightweight cloneable signal backed by a tokio watch channel
//! with the same observable semantics (aborted flag, reason, racing).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Reason attached to an abort. Upstream uses the signal's `reason` (any
/// thrown value); the Rust port models the common cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    /// Upstream default: `Error("The operation was aborted")` named AbortError.
    Aborted,
    /// Explicit reason string (e.g. timeout, superseded refresh).
    Custom(String),
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortReason::Aborted => write!(f, "The operation was aborted"),
            AbortReason::Custom(msg) => write!(f, "{msg}"),
        }
    }
}

/// A cloneable abort signal. Clones share abort state, mirroring how one
/// DOM signal fans out to many listeners.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    inner: Arc<SignalInner>,
}

#[derive(Debug)]
struct SignalInner {
    /// Own abort state (the DOM signal's aborted flag). Kept in a watch
    /// channel so waiters can subscribe; `abort` stores the flag with
    /// `send_replace` so late subscribers still observe it.
    state: tokio::sync::watch::Sender<Option<AbortReason>>,
    /// Signals this one follows (`AbortSignal.any`). Checked synchronously by
    /// `is_aborted`/`reason`/`aborted_or_pending`, so an input abort is
    /// observable immediately — no forwarding task whose scheduling could
    /// race the caller.
    inputs: Mutex<Vec<tokio::sync::watch::Receiver<Option<AbortReason>>>>,
}

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortSignal {
    /// Fresh, never-aborted signal.
    pub fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(None);
        Self {
            inner: Arc::new(SignalInner {
                state: tx,
                inputs: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Signal already in the aborted state.
    pub fn aborted(reason: Option<AbortReason>) -> Self {
        let signal = Self::new();
        signal.abort(reason);
        signal
    }

    /// Combines several signals: the composite aborts when any input aborts
    /// (mirrors upstream `AbortSignal.any`). Upstream attaches its listeners
    /// synchronously; the port stores subscriptions to the inputs so the
    /// composite observes aborts without a forwarding task.
    pub fn any(signals: &[AbortSignal]) -> Self {
        let composite = AbortSignal::new();
        {
            let mut inputs = composite.lock_inputs();
            for signal in signals {
                if signal.is_aborted() {
                    drop(inputs);
                    composite.abort(signal.reason());
                    return composite;
                }
                inputs.push(signal.inner.state.subscribe());
            }
        }
        composite
    }

    /// Timeout signal (mirrors upstream `AbortSignal.timeout`).
    pub fn timeout(duration: std::time::Duration) -> Self {
        let signal = AbortSignal::new();
        let task_signal = signal.clone();
        tokio::spawn(async move {
            crate::clock::sleep(duration).await;
            task_signal.abort(Some(AbortReason::Custom("TimeoutError".to_string())));
        });
        signal
    }

    /// Aborts the signal with an optional reason.
    pub fn abort(&self, reason: Option<AbortReason>) {
        // `send_replace`, not `send`: the aborted flag must be stored even when
        // no receiver is subscribed yet (upstream DOM signals store the flag,
        // so late listeners observe the abort). tokio watch's `send` drops the
        // value when every receiver has been dropped, which would make an
        // abort racing a queued task silently vanish.
        self.inner
            .state
            .send_replace(Some(reason.unwrap_or(AbortReason::Aborted)));
    }

    pub fn is_aborted(&self) -> bool {
        if self.inner.state.borrow().is_some() {
            return true;
        }
        self.lock_inputs().iter().any(|rx| rx.borrow().is_some())
    }

    /// Whether two handles share the same underlying signal state.
    pub fn same_as(&self, other: &AbortSignal) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns a clone of the abort reason, if any (avoids borrowing the
    /// temporary watch guard across the return boundary).
    pub fn reason(&self) -> Option<AbortReason> {
        if let Some(reason) = self.inner.state.borrow().clone() {
            return Some(reason);
        }
        self.lock_inputs().iter().find_map(|rx| rx.borrow().clone())
    }

    /// Waits until the signal aborts.
    pub async fn aborted_or_pending(&self) -> AbortReason {
        let own = self.inner.state.subscribe();
        let mut inputs: Vec<tokio::sync::watch::Receiver<Option<AbortReason>>> =
            self.lock_inputs().iter().cloned().collect();
        loop {
            if let Some(reason) = own.borrow().clone() {
                return reason;
            }
            for rx in &inputs {
                if let Some(reason) = rx.borrow().clone() {
                    return reason;
                }
            }
            // Park until one of the channels changes. Index 0 is the own
            // channel; the rest follow the inputs. A channel that closed
            // (input sender dropped without aborting) is pruned instead of
            // busy-looping on a ready `Err`.
            let mut waits: Vec<Pin<Box<dyn Future<Output = bool> + Send>>> =
                Vec::with_capacity(inputs.len() + 1);
            {
                let mut own_wait = own.clone();
                waits.push(Box::pin(async move { own_wait.changed().await.is_ok() }));
            }
            for rx in &inputs {
                let mut input_wait = rx.clone();
                waits.push(Box::pin(async move { input_wait.changed().await.is_ok() }));
            }
            let (open, index, _) = futures::future::select_all(waits).await;
            if index == 0 {
                if !open {
                    return AbortReason::Aborted;
                }
            } else if !open {
                inputs.remove(index - 1);
            }
        }
    }

    fn lock_inputs(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<tokio::sync::watch::Receiver<Option<AbortReason>>>> {
        self.inner
            .inputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Upstream `signal.throwIfAborted()`: raises the abort as an error.
    pub fn throw_if_aborted(&self) -> Result<(), crate::error::AiError> {
        match self.reason() {
            Some(reason) => Err(crate::error::AiError::Aborted(reason.to_string())),
            None => Ok(()),
        }
    }

    /// Waits for the operation, racing it against this signal. If the signal
    /// aborts first, returns `Err(Aborted)` (upstream rejects with the abort
    /// reason); the abandoned future keeps running but its result is ignored.
    pub async fn race<T, E, F>(&self, operation: F) -> Result<T, crate::error::AiError>
    where
        F: std::future::Future<Output = Result<T, E>>,
        crate::error::AiError: From<E>,
    {
        tokio::select! {
            biased;
            reason = self.aborted_or_pending() => Err(crate::error::AiError::Aborted(reason.to_string())),
            result = operation => result.map_err(crate::error::AiError::from),
        }
    }
}

/// Creates an operation-local signal for public APIs whose signal is
/// optional (upstream `operationSignal`).
pub fn operation_signal(signal: Option<&AbortSignal>) -> AbortSignal {
    signal.cloned().unwrap_or_default()
}
