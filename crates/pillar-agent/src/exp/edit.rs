//! `exp_edit`: replace referenced byte ranges in one conditional publication
//! (design §4.2–§4.4, §7).
//!
//! The shape of the operation is what makes it safe:
//!
//! 1. the operation id is reserved **before** anything can be published, so a
//!    retry joins the in-flight operation or gets its receipt, and a
//!    reservation that is dropped reports `outcome_unknown` (§7.1);
//! 2. every reference is resolved and checked (owner, generation, lifetime,
//!    editability, path identity, one file, one revision, no overlap) before
//!    any content is computed — a rejected call has no side effect at all;
//! 3. all replacements are positions against the *original* snapshot (§4.2),
//!    so nothing drifts as earlier ranges are rewritten;
//! 4. publication is a conditional compare-and-swap on the revision, so an
//!    external writer (or an ABA write) is a conflict, never a silent
//!    overwrite.
//!
//! Replacements are literal: line endings, BOM and trailing newlines outside
//! the referenced range are untouched, and nothing is normalized (§4.2).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ExpLimits;
use super::error::{ExpError, ExpErrorCode};
use super::ledger::{OperationId, OperationLedger, Receipt, Reservation};
use super::refs::{ByteRange, OwnerId, RefId, RefRecord, RefStore};
use super::store::{ConditionalStore, Guarantee, digest64};

/// One replacement: the referenced bytes become `replacement`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpEditRequestItem {
    #[serde(rename = "ref")]
    pub reference: RefId,
    pub replacement: String,
}

/// An `exp_edit` call: one operation, one file, a set of disjoint references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpEditRequest {
    pub operation_id: OperationId,
    pub edits: Vec<ExpEditRequestItem>,
}

/// The result of an `exp_edit`: the receipt of the publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpEditResponse {
    pub receipt: Receipt,
}

/// Apply the requested replacements, or explain why nothing was applied.
pub async fn exp_edit(
    host: &dyn ConditionalStore,
    refs: &RefStore,
    ledger: &Arc<OperationLedger>,
    limits: &ExpLimits,
    owner: &OwnerId,
    request: &ExpEditRequest,
) -> Result<ExpEditResponse, ExpError> {
    if request.edits.is_empty() {
        return Err(ExpError::invalid_request(
            "edits must contain at least one replacement",
        )
        .with_repair("send one or more {ref, replacement} pairs"));
    }
    if request.edits.len() > limits.max_edits_per_call {
        return Err(ExpError::budget_exceeded(
            format!(
                "{} replacements in one call (limit {})",
                request.edits.len(),
                limits.max_edits_per_call
            ),
            "split the edit into several calls",
        ));
    }
    let replacement_bytes: usize = request
        .edits
        .iter()
        .map(|edit| edit.replacement.len())
        .sum();
    if replacement_bytes > limits.max_edit_bytes {
        return Err(ExpError::budget_exceeded(
            format!(
                "{replacement_bytes} replacement bytes in one call (limit {})",
                limits.max_edit_bytes
            ),
            "split the edit into several calls",
        ));
    }

    let reservation = ledger
        .reserve(owner, &request.operation_id, args_digest(request))
        .await?;
    let fresh = match reservation {
        Reservation::Joined(outcome) => {
            return Ok(ExpEditResponse { receipt: outcome? });
        }
        Reservation::Fresh(fresh) => fresh,
    };

    match apply(host, refs, owner, request).await {
        Ok(receipt) => Ok(ExpEditResponse {
            receipt: fresh.complete(receipt),
        }),
        Err(error) => Err(fresh.fail(error)),
    }
}

/// Validate everything, then publish once.
async fn apply(
    host: &dyn ConditionalStore,
    refs: &RefStore,
    owner: &OwnerId,
    request: &ExpEditRequest,
) -> Result<Receipt, ExpError> {
    if host.guarantee() != Guarantee::Strict {
        return Err(ExpError::new(
            ExpErrorCode::UnsupportedGuarantee,
            "this host cannot check a revision and publish atomically",
        )
        .with_repair(
            "edit through the host's own conditional-update path, or opt into a weak mode explicitly",
        ));
    }

    // Every gate comes before any computation of new content.
    let records: Vec<RefRecord> = request
        .edits
        .iter()
        .map(|edit| refs.lookup(owner, &edit.reference))
        .collect::<Result<_, _>>()?;

    for record in &records {
        if !record.editable {
            return Err(ExpError::new(
                ExpErrorCode::InvalidRef,
                "this reference was not issued for editing",
            )
            .with_repair("read the range again; only fully delivered text is editable"));
        }
        match host.resolve(&record.path).await {
            Ok(current) if current == record.resource => {}
            Ok(_) => {
                return Err(ExpError::revision_conflict()
                    .with_repair("the path now names a different file; read it again"));
            }
            Err(error) if error.code == ExpErrorCode::PermissionDenied => return Err(error),
            Err(_) => {
                return Err(ExpError::revision_conflict()
                    .with_repair("the referenced file is gone; read the target again"));
            }
        }
    }

    let resource = records[0].resource.clone();
    let revision = records[0].revision;
    for record in &records {
        if record.resource != resource {
            return Err(ExpError::invalid_request(
                "every reference in one call must belong to the same file",
            ));
        }
        if record.revision != revision {
            return Err(ExpError::revision_conflict()
                .with_repair("read the file again so every reference comes from one revision"));
        }
    }

    let mut ranges: Vec<ByteRange> = records.iter().map(|record| record.range).collect();
    ranges.sort_by_key(|range| range.start);
    for pair in ranges.windows(2) {
        if pair[0].overlaps(&pair[1]) {
            return Err(ExpError::invalid_request(
                "referenced ranges overlap",
            )
            .with_repair("merge the overlapping ranges into one replacement"));
        }
    }

    let snapshot = host.snapshot(&resource).await?;
    if snapshot.revision != revision {
        return Err(ExpError::revision_conflict());
    }
    for record in &records {
        let bytes = snapshot
            .bytes
            .get(record.range.start..record.range.end)
            .ok_or_else(ExpError::revision_conflict)?;
        if digest64(bytes) != record.range_digest {
            return Err(ExpError::revision_conflict());
        }
    }

    // One pass over the original bytes: splice from the end so that the
    // positions of the remaining ranges stay valid.
    let mut new_bytes = snapshot.bytes.clone();
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse(records[*index].range.start));
    for index in order {
        let range = records[index].range;
        let replacement = request.edits[index].replacement.as_bytes();
        new_bytes.splice(range.start..range.end, replacement.iter().copied());
    }

    let new_revision = host.compare_and_swap(&resource, revision, &new_bytes).await?;
    refs.invalidate_resource(&resource);

    Ok(Receipt {
        operation_id: request.operation_id.clone(),
        path: records[0].path.clone(),
        revision: new_revision,
        edits_applied: records.len(),
        bytes_written: new_bytes.len(),
    })
}

/// Fingerprint of the arguments a retry must repeat to be recognized as the
/// same operation (design §7.1). Length-prefixed so two different edit sets
/// cannot render identically.
fn args_digest(request: &ExpEditRequest) -> u64 {
    let mut buffer: Vec<u8> = Vec::new();
    for edit in &request.edits {
        buffer.extend_from_slice(edit.reference.as_str().as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&(edit.replacement.len() as u64).to_le_bytes());
        buffer.extend_from_slice(edit.replacement.as_bytes());
    }
    digest64(&buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::ledger::OperationId;

    fn edit(reference: &str, replacement: &str) -> ExpEditRequestItem {
        ExpEditRequestItem {
            reference: RefId::new(reference),
            replacement: replacement.to_string(),
        }
    }

    #[test]
    fn the_argument_fingerprint_separates_adjacent_edits() {
        let one = ExpEditRequest {
            operation_id: OperationId::new("op"),
            edits: vec![edit("a", "b"), edit("c", "d")],
        };
        let two = ExpEditRequest {
            operation_id: OperationId::new("op"),
            edits: vec![edit("a", "bc"), edit("", "d")],
        };
        assert_ne!(args_digest(&one), args_digest(&two));

        let repeat = ExpEditRequest {
            operation_id: OperationId::new("op"),
            edits: vec![edit("a", "b"), edit("c", "d")],
        };
        assert_eq!(args_digest(&one), args_digest(&repeat));
    }
}
