//! Compiler suggestion application (design §8, stage R3).
//!
//! A rustc suggestion is registered as a *proposal group*: every replacement
//! the compiler offered together must be applied together, and a suggestion is
//! only a candidate when the whole group is `MachineApplicable`. The initial
//! conditions are enforced here, not in prose (design §8.2):
//!
//! - a single workspace file, valid UTF-8, non-overlapping byte spans;
//! - no zero-length span (an insertion); the base adapter does not have an
//!   insertion contract yet, so it is refused rather than forced;
//! - a strict host. A weak host (a plain filesystem) still previews but
//!   application is refused, never silently downgraded (design §8.3);
//! - the expected revision is captured when the suggestion is registered, and
//!   re-checked at publication. A live diagnostic whose snapshot cannot be
//!   proven does not become an editable target.
//!
//! Registration and application reuse the experimental adapter's
//! [`ConditionalStore`], [`OperationLedger`] and [`Receipt`]
//! (docs/TOOL-EFFICIENCY-DESIGN.md §4, §7): a retry with the same operation id
//! joins or receives the original receipt, and an abandoned operation is
//! `outcome_unknown`, never success.
//!
//! What "applied" means here: the patch was published to the target. It is
//! **not** a validation that the change compiles or tests pass; that is a
//! separate `rs_run` step (design §8.3).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::exp::error::{ExpError, ExpErrorCode};
use crate::exp::ledger::{OperationId, OperationLedger, Receipt, Reservation};
use crate::exp::refs::OwnerId as ExpOwnerId;
use crate::exp::store::{ConditionalStore, Guarantee, ResourceId, Revision, digest64};

use super::diagnostic::{Applicability, CollectedRun, Diagnostic, SuggestionGroup};
use super::error::{RustToolError, RustToolErrorCode};
use super::host::OwnerId;

/// The model-facing reference to a registered proposal group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionNotice {
    pub suggestion_id: String,
    pub diagnostic_id: String,
    pub applicable: bool,
    pub applicability: Applicability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One replacement in a preview, with the new text the model must be able to
/// judge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewReplacement {
    pub byte_start: u64,
    pub byte_end: u64,
    pub new_text: String,
    pub zero_length: bool,
}

/// A registered suggestion as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionPreview {
    pub suggestion_id: String,
    pub diagnostic_id: String,
    pub run_id: String,
    pub applicable: bool,
    pub applicability: Applicability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub guarantee: Guarantee,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Revision>,
    pub replacements: Vec<PreviewReplacement>,
    /// Optimistic-concurrency token for [`ApplyRequest::expected_preview_digest`].
    pub preview_digest: String,
}

/// `rs_apply_suggestion` arguments (design §8.1, example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyRequest {
    pub suggestion_id: String,
    pub operation_id: String,
    #[serde(default)]
    pub expected_preview_digest: Option<String>,
}

/// What an application reports: the published revision and counts. `validated`
/// stays false — publication is not compilation or test success (design §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionReceipt {
    pub suggestion_id: String,
    pub operation_id: String,
    pub path: String,
    pub revision: Revision,
    pub edits_applied: usize,
    pub bytes_written: usize,
    pub applied: bool,
    pub validated: bool,
}

#[derive(Debug, Clone)]
struct RegisteredReplacement {
    byte_start: usize,
    byte_end: usize,
    new_text: String,
}

#[derive(Debug, Clone)]
struct RegisteredSuggestion {
    id: String,
    diagnostic_id: String,
    run_id: String,
    owner: String,
    path: Option<String>,
    resource: Option<ResourceId>,
    expected_revision: Option<Revision>,
    replacements: Vec<RegisteredReplacement>,
    applicability: Applicability,
    applicable: bool,
    reason: Option<String>,
    guarantee: Guarantee,
    preview_digest: String,
    args_digest: u64,
    new_bytes: Vec<u8>,
}

/// Per-file snapshot cache for one registration pass: the resolution, the
/// revision and the bytes, or the refusal that stopped it.
type SnapshotCache = HashMap<String, Result<(ResourceId, Revision, Vec<u8>), RustToolError>>;

#[derive(Debug, Default)]
struct SuggestionRegistry {
    by_id: HashMap<String, RegisteredSuggestion>,
    order: VecDeque<String>,
    counter: u64,
    capacity: usize,
}

impl SuggestionRegistry {
    fn new(capacity: usize) -> Self {
        Self {
            by_id: HashMap::new(),
            order: VecDeque::new(),
            counter: 0,
            capacity: capacity.max(1),
        }
    }

    fn insert(&mut self, mut suggestion: RegisteredSuggestion) -> String {
        self.counter += 1;
        let id = format!("sg{}", self.counter);
        suggestion.id = id.clone();
        while self.by_id.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.by_id.remove(&evicted);
            } else {
                break;
            }
        }
        self.order.push_back(id.clone());
        self.by_id.insert(id.clone(), suggestion);
        id
    }

    fn get(&self, owner: &OwnerId, id: &str) -> Result<RegisteredSuggestion, RustToolError> {
        match self.by_id.get(id) {
            Some(suggestion) if suggestion.owner == owner.as_str() => Ok(suggestion.clone()),
            // Unknown and foreign are the same refusal (design §3).
            _ => Err(RustToolError::invalid_request(
                "unknown suggestion for this session",
            )),
        }
    }
}

/// Registers proposal groups from normalized diagnostics and applies them
/// under the initial conditions (design §8).
pub struct SuggestionService {
    host: Arc<dyn ConditionalStore>,
    ledger: Arc<OperationLedger>,
    registry: Mutex<SuggestionRegistry>,
    registered_runs: Mutex<HashMap<String, Vec<SuggestionNotice>>>,
}

impl SuggestionService {
    pub fn new(
        host: Arc<dyn ConditionalStore>,
        ledger: Arc<OperationLedger>,
        capacity: usize,
    ) -> Self {
        Self {
            host,
            ledger,
            registry: Mutex::new(SuggestionRegistry::new(capacity)),
            registered_runs: Mutex::new(HashMap::new()),
        }
    }

    /// Register every proposal group in a run's diagnostics once. A repeated
    /// call returns the same ids (idempotent by run id).
    pub async fn register_run(
        &self,
        owner: &OwnerId,
        run: &CollectedRun,
    ) -> Result<Vec<SuggestionNotice>, RustToolError> {
        if let Some(cached) = self
            .registered_runs
            .lock()
            .expect("registered runs lock")
            .get(&run.run_id)
            .cloned()
        {
            return Ok(cached);
        }

        let mut snapshots: SnapshotCache = HashMap::new();
        let mut notices = Vec::new();
        for diagnostic in &run.diagnostics {
            self.register_diagnostic(owner, run, diagnostic, &mut snapshots, &mut notices)
                .await;
        }
        self.registered_runs
            .lock()
            .expect("registered runs lock")
            .insert(run.run_id.clone(), notices.clone());
        Ok(notices)
    }

    async fn register_diagnostic(
        &self,
        owner: &OwnerId,
        run: &CollectedRun,
        diagnostic: &Diagnostic,
        snapshots: &mut SnapshotCache,
        notices: &mut Vec<SuggestionNotice>,
    ) {
        for group in &diagnostic.suggestions {
            let notice = self
                .register_group(owner, run, diagnostic, group, snapshots)
                .await;
            notices.push(notice);
        }
        for child in &diagnostic.children {
            // A child's suggestion is a distinct proposal (design §8.1); box
            // the recursion so a deep diagnostic tree cannot blow the stack.
            Box::pin(self.register_diagnostic(owner, run, child, snapshots, notices)).await;
        }
    }

    async fn register_group(
        &self,
        owner: &OwnerId,
        run: &CollectedRun,
        diagnostic: &Diagnostic,
        group: &SuggestionGroup,
        snapshots: &mut SnapshotCache,
    ) -> SuggestionNotice {
        let guarantee = self.host.guarantee();
        let mut path: Option<String> = None;
        let mut resource = None;
        let mut revision = None;
        let mut replacements: Vec<RegisteredReplacement> = Vec::new();
        let mut new_bytes = Vec::new();

        let (applicable, reason) = match self.classify(group, guarantee) {
            Ok(()) => {
                let file = first_workspace_path(group).expect("classify checked the file");
                path = Some(file.clone());
                match self.load(file.clone(), &file, snapshots).await {
                    Ok((found_resource, found_revision, bytes)) => {
                        match build_new_bytes(&bytes, group) {
                            Ok(built) => {
                                resource = Some(found_resource);
                                revision = Some(found_revision);
                                replacements = group
                                    .replacements
                                    .iter()
                                    .map(|item| RegisteredReplacement {
                                        byte_start: item.byte_start as usize,
                                        byte_end: item.byte_end as usize,
                                        new_text: item.replacement.clone(),
                                    })
                                    .collect();
                                new_bytes = built;
                                (true, None)
                            }
                            Err(reason) => (false, Some(reason)),
                        }
                    }
                    Err(error) => (false, Some(error.message)),
                }
            }
            Err(reason) => (false, Some(reason)),
        };

        let preview_digest = preview_digest(run, diagnostic, group, path.as_deref(), revision);
        let args_digest = digest64(preview_digest.as_bytes());
        let registered = RegisteredSuggestion {
            id: String::new(),
            diagnostic_id: diagnostic.id.clone(),
            run_id: run.run_id.clone(),
            owner: owner.as_str().to_string(),
            path,
            resource,
            expected_revision: revision,
            replacements,
            applicability: group.applicability.clone(),
            applicable,
            reason: reason.clone(),
            guarantee,
            preview_digest,
            args_digest,
            new_bytes,
        };
        let suggestion_id = self
            .registry
            .lock()
            .expect("registry lock")
            .insert(registered);
        SuggestionNotice {
            suggestion_id,
            diagnostic_id: diagnostic.id.clone(),
            applicable,
            applicability: group.applicability.clone(),
            reason,
        }
    }

    /// The design's initial conditions, checked before any source is read
    /// (design §8.2).
    fn classify(&self, group: &SuggestionGroup, guarantee: Guarantee) -> Result<(), String> {
        if group.replacements.is_empty() {
            return Err("the proposal has no replacements".to_string());
        }
        if first_workspace_path(group).is_none() {
            return Err(
                "the proposal is not a single workspace file (macro, registry or sysroot span)"
                    .to_string(),
            );
        }
        if group.has_insertions() {
            return Err("zero-length insertion is not supported yet".to_string());
        }
        if !group.is_machine_applicable() {
            return Err(format!(
                "the compiler marked this {:?}, not MachineApplicable",
                group.applicability
            ));
        }
        if guarantee == Guarantee::Weak {
            return Err(
                "the host cannot promise a conditional publication (weak guarantee)".to_string(),
            );
        }
        let mut spans: Vec<(u64, u64)> = group
            .replacements
            .iter()
            .map(|item| (item.byte_start, item.byte_end))
            .collect();
        spans.sort_unstable();
        for window in spans.windows(2) {
            if window[1].0 < window[0].1 {
                return Err("the replacement spans overlap".to_string());
            }
        }
        Ok(())
    }

    async fn load(
        &self,
        cache_key: String,
        path: &str,
        snapshots: &mut SnapshotCache,
    ) -> Result<(ResourceId, Revision, Vec<u8>), RustToolError> {
        if let Some(cached) = snapshots.get(&cache_key) {
            return cached.clone();
        }
        let result = async {
            let resource = self.host.resolve(path).await.map_err(map_exp)?;
            let snapshot = self.host.snapshot(&resource).await.map_err(map_exp)?;
            Ok((snapshot.resource, snapshot.revision, snapshot.bytes))
        }
        .await;
        snapshots.insert(cache_key, result.clone());
        result
    }

    /// The preview for one registered suggestion.
    pub fn preview(
        &self,
        owner: &OwnerId,
        suggestion_id: &str,
    ) -> Result<SuggestionPreview, RustToolError> {
        let suggestion = self
            .registry
            .lock()
            .expect("registry lock")
            .get(owner, suggestion_id)?;
        Ok(SuggestionPreview {
            suggestion_id: suggestion.id.clone(),
            diagnostic_id: suggestion.diagnostic_id.clone(),
            run_id: suggestion.run_id.clone(),
            applicable: suggestion.applicable,
            applicability: suggestion.applicability.clone(),
            reason: suggestion.reason.clone(),
            guarantee: suggestion.guarantee,
            path: suggestion.path.clone(),
            expected_revision: suggestion.expected_revision,
            replacements: suggestion
                .replacements
                .iter()
                .map(|item| PreviewReplacement {
                    byte_start: item.byte_start as u64,
                    byte_end: item.byte_end as u64,
                    new_text: item.new_text.clone(),
                    zero_length: item.byte_start == item.byte_end,
                })
                .collect(),
            preview_digest: suggestion.preview_digest.clone(),
        })
    }

    /// Apply a registered suggestion once, with operation-id retransmit
    /// suppression (design §8.3, base design §7).
    pub async fn apply(
        &self,
        owner: &OwnerId,
        request: &ApplyRequest,
    ) -> Result<SuggestionReceipt, RustToolError> {
        let suggestion = self
            .registry
            .lock()
            .expect("registry lock")
            .get(owner, &request.suggestion_id)?;
        if !suggestion.applicable {
            return Err(RustToolError::unsupported_suggestion(
                suggestion
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the suggestion is preview-only".to_string()),
            ));
        }
        let (Some(resource), Some(expected_revision), Some(_path)) = (
            suggestion.resource.clone(),
            suggestion.expected_revision,
            suggestion.path.clone(),
        ) else {
            return Err(RustToolError::invalid_request(
                "the suggestion has no target revision",
            ));
        };
        if let Some(expected) = &request.expected_preview_digest
            && *expected != suggestion.preview_digest
        {
            return Err(RustToolError::revision_conflict(
                "the suggestion preview changed since it was read",
            ));
        }

        let exp_owner = ExpOwnerId::new(owner.as_str());
        let operation = OperationId::new(request.operation_id.clone());
        let reservation = self
            .ledger
            .reserve(&exp_owner, &operation, suggestion.args_digest)
            .await
            .map_err(map_exp)?;

        match reservation {
            Reservation::Joined(outcome) => match outcome {
                Ok(receipt) => Ok(self.receipt(&suggestion, receipt)),
                Err(error) => Err(map_exp(error)),
            },
            Reservation::Fresh(fresh) => {
                let snapshot = match self.host.snapshot(&resource).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => return Err(map_exp(fresh.fail(error))),
                };
                if snapshot.revision != expected_revision {
                    let error = ExpError::new(
                        ExpErrorCode::RevisionConflict,
                        "the target changed since the suggestion was registered",
                    );
                    return Err(map_exp(fresh.fail(error)));
                }
                match self
                    .host
                    .compare_and_swap(&resource, expected_revision, &suggestion.new_bytes)
                    .await
                {
                    Ok(revision) => {
                        let receipt = fresh.complete(Receipt {
                            operation_id: operation,
                            path: suggestion.path.clone().unwrap_or_default(),
                            revision,
                            edits_applied: suggestion.replacements.len(),
                            bytes_written: suggestion.new_bytes.len(),
                        });
                        Ok(self.receipt(&suggestion, receipt))
                    }
                    Err(error) => Err(map_exp(fresh.fail(error))),
                }
            }
        }
    }

    fn receipt(&self, suggestion: &RegisteredSuggestion, receipt: Receipt) -> SuggestionReceipt {
        SuggestionReceipt {
            suggestion_id: suggestion.id.clone(),
            operation_id: receipt.operation_id.as_str().to_string(),
            path: receipt.path,
            revision: receipt.revision,
            edits_applied: receipt.edits_applied,
            bytes_written: receipt.bytes_written,
            applied: true,
            validated: false,
        }
    }
}

fn first_workspace_path(group: &SuggestionGroup) -> Option<String> {
    let mut path: Option<String> = None;
    for replacement in &group.replacements {
        let super::diagnostic::SourceBinding::Workspace { path: candidate } = &replacement.binding
        else {
            return None;
        };
        match &path {
            Some(existing) if existing != candidate => return None,
            _ => path = Some(candidate.clone()),
        }
    }
    path
}

fn build_new_bytes(bytes: &[u8], group: &SuggestionGroup) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "the target is not valid UTF-8".to_string())?;
    let mut spans: Vec<&super::diagnostic::SuggestionReplacement> =
        group.replacements.iter().collect();
    spans.sort_by_key(|item| item.byte_start);
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0usize;
    for item in spans {
        let start = item.byte_start as usize;
        let end = item.byte_end as usize;
        if start > end || end > bytes.len() {
            return Err("a replacement span is outside the current file".to_string());
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err("a replacement span is not on a UTF-8 boundary".to_string());
        }
        out.extend_from_slice(&bytes[cursor..start]);
        out.extend_from_slice(item.replacement.as_bytes());
        cursor = end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    Ok(out)
}

fn preview_digest(
    run: &CollectedRun,
    diagnostic: &Diagnostic,
    group: &SuggestionGroup,
    path: Option<&str>,
    revision: Option<Revision>,
) -> String {
    let mut canonical = String::new();
    canonical.push_str(&run.run_id);
    canonical.push('\u{1f}');
    canonical.push_str(&diagnostic.id);
    canonical.push('\u{1f}');
    canonical.push_str(path.unwrap_or(""));
    if let Some(revision) = revision {
        canonical.push('\u{1f}');
        canonical.push_str(&format!("{}:{}", revision.generation, revision.digest));
    }
    for item in &group.replacements {
        canonical.push('\u{1f}');
        canonical.push_str(&format!("{}:{}:", item.byte_start, item.byte_end));
        canonical.push_str(&item.replacement);
    }
    format!("{:016x}", digest64(canonical.as_bytes()))
}

fn map_exp(error: ExpError) -> RustToolError {
    let code = match error.code {
        ExpErrorCode::PermissionDenied => RustToolErrorCode::PermissionDenied,
        ExpErrorCode::RevisionConflict => RustToolErrorCode::RevisionConflict,
        ExpErrorCode::UnsupportedGuarantee => RustToolErrorCode::UnsupportedSuggestion,
        ExpErrorCode::BudgetExceeded => RustToolErrorCode::BudgetExceeded,
        ExpErrorCode::OperationIdMismatch => RustToolErrorCode::OperationIdMismatch,
        ExpErrorCode::OutcomeUnknown => RustToolErrorCode::OutcomeUnknown,
        ExpErrorCode::InvalidRef | ExpErrorCode::ExpiredRef => RustToolErrorCode::SourceUnbound,
        ExpErrorCode::InvalidRequest => RustToolErrorCode::InvalidRequest,
        ExpErrorCode::HostFailure => RustToolErrorCode::HostFailure,
    };
    let mut mapped = RustToolError::new(code, error.message);
    mapped.repair = error.repair;
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_new_bytes_applies_a_sorted_non_overlapping_set() {
        let group = SuggestionGroup {
            diagnostic_id: "d1".to_string(),
            applicability: Applicability::MachineApplicable,
            replacements: vec![
                super::super::diagnostic::SuggestionReplacement {
                    file_name: "f.txt".to_string(),
                    binding: super::super::diagnostic::SourceBinding::Workspace {
                        path: "f.txt".to_string(),
                    },
                    byte_start: 0,
                    byte_end: 3,
                    replacement: "ONE".to_string(),
                    applicability: Applicability::MachineApplicable,
                    zero_length: false,
                },
                super::super::diagnostic::SuggestionReplacement {
                    file_name: "f.txt".to_string(),
                    binding: super::super::diagnostic::SourceBinding::Workspace {
                        path: "f.txt".to_string(),
                    },
                    byte_start: 4,
                    byte_end: 7,
                    replacement: "TWO".to_string(),
                    applicability: Applicability::MachineApplicable,
                    zero_length: false,
                },
            ],
        };
        assert_eq!(
            build_new_bytes(b"one two", &group).expect("built"),
            b"ONE TWO"
        );
    }

    #[test]
    fn build_new_bytes_rejects_a_span_past_the_end() {
        let group = SuggestionGroup {
            diagnostic_id: "d1".to_string(),
            applicability: Applicability::MachineApplicable,
            replacements: vec![super::super::diagnostic::SuggestionReplacement {
                file_name: "f.txt".to_string(),
                binding: super::super::diagnostic::SourceBinding::Workspace {
                    path: "f.txt".to_string(),
                },
                byte_start: 10,
                byte_end: 12,
                replacement: "x".to_string(),
                applicability: Applicability::MachineApplicable,
                zero_length: false,
            }],
        };
        assert!(build_new_bytes(b"short", &group).is_err());
    }
}
