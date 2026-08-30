//! Port of packages/agent/src/harness/reducer.ts (pi v0.84.3) — pure
//! reconstruction of one lane's orchestration state from its bounded
//! recovery inputs.
//!
//! divergence: upstream throws `RecordLogCorruption`; the port returns
//! `Result<_, RecordLogCorruption>`. Upstream clones inputs defensively
//! (`structuredClone`) so reduction cannot alias them; the port borrows
//! inputs and clones on output, which gives the same guarantee
//! (docs/INSTRUCTIONS.md: no defensive clones).

use std::collections::{HashMap, HashSet};
use std::fmt;

use pillar_ai::types::{AssistantMessage, DeferredHandle, StopReason};

use crate::harness::result::TaggedErrorValue;
use crate::harness::session::types::{
    CompactionReason, Entry, EntryPayload, LaneRecord, OperationIntent, ProvisionedEntry,
    RecordPayload,
};
use crate::types::AgentToolCall;
use crate::types::thinking::AgentThinkingLevel;

/// Machine-readable category for a contradiction in a lane's durable
/// recovery slice (upstream `RecordLogCorruptionReason`). These indicate
/// states the single-writer record protocol cannot produce, not ordinary
/// operation failures or incomplete-but-recoverable intent/result
/// prefixes. Restore must reject such states rather than repair or
/// continue it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLogCorruptionReason {
    MultipleOpenOperations,
    UnknownOperation,
    RecordAfterFinish,
    NonConsecutiveAttempt,
    InvalidCompactionReason,
    QueueAfterAbort,
    InvalidQueueCancellation,
    InconsistentStep,
    ToolCallMismatch,
    DuplicateToolInvocation,
    ProvisionedEntryMismatch,
    InvalidDeferredHandle,
}

impl RecordLogCorruptionReason {
    /// Upstream snake_case reason string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MultipleOpenOperations => "multiple_open_operations",
            Self::UnknownOperation => "unknown_operation",
            Self::RecordAfterFinish => "record_after_finish",
            Self::NonConsecutiveAttempt => "non_consecutive_attempt",
            Self::InvalidCompactionReason => "invalid_compaction_reason",
            Self::QueueAfterAbort => "queue_after_abort",
            Self::InvalidQueueCancellation => "invalid_queue_cancellation",
            Self::InconsistentStep => "inconsistent_step",
            Self::ToolCallMismatch => "tool_call_mismatch",
            Self::DuplicateToolInvocation => "duplicate_tool_invocation",
            Self::ProvisionedEntryMismatch => "provisioned_entry_mismatch",
            Self::InvalidDeferredHandle => "invalid_deferred_handle",
        }
    }
}

/// Upstream `RecordLogCorruption`: a record-log contradiction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordLogCorruption {
    pub reason: RecordLogCorruptionReason,
    pub message: String,
}

impl RecordLogCorruption {
    pub fn new(reason: RecordLogCorruptionReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl fmt::Display for RecordLogCorruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RecordLogCorruption {}

impl TaggedErrorValue for RecordLogCorruption {
    fn tag(&self) -> &'static str {
        "RecordLogCorruption"
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "_tag": self.tag(),
            "message": self.message,
            "reason": self.reason.as_str(),
        })
    }
}

/// The bounded recovery slice of one lane (upstream `RecordLogSlice`).
#[derive(Debug, Clone, Default)]
pub struct RecordLogSlice {
    pub lane: String,
    pub open_operations: Vec<LaneRecord>,
    pub records: Vec<LaneRecord>,
    /// Operation-owned entries plus entries fetched directly by provisioned
    /// or referenced ids.
    pub entries: Vec<Entry>,
}

/// The resolved model reference (upstream `{ provider, modelId }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneModelRef {
    pub provider: String,
    pub model_id: String,
}

/// The model/thinking/tool configuration an operation runs under (upstream
/// `EffectiveLaneConfiguration`).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveLaneConfiguration {
    pub model: LaneModelRef,
    pub thinking_level: AgentThinkingLevel,
    pub active_tool_names: Vec<String>,
}

impl Default for EffectiveLaneConfiguration {
    fn default() -> Self {
        Self {
            model: LaneModelRef {
                provider: String::new(),
                model_id: String::new(),
            },
            thinking_level: AgentThinkingLevel::Off,
            active_tool_names: Vec::new(),
        }
    }
}

/// Where a terminal failure was produced (upstream
/// `TerminalFailureState.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFailureSource {
    Step,
    DeferredFetch,
}

/// A terminal failure observed at the operation tail (upstream
/// `TerminalFailureState`).
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFailureState {
    pub entry_id: String,
    pub source: TerminalFailureSource,
    pub message: Box<AssistantMessage>,
}

/// The tool-start record data a batch call carries (upstream `started?`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStartRecordInfo {
    pub run_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub effective_args: serde_json::Value,
    pub result_entry_id: String,
    /// `never` | `safe`.
    pub replay: String,
}

/// One tool call of the pending tool batch (upstream
/// `ToolBatchState.calls`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolBatchCall {
    pub tool_index: usize,
    pub tool_call: Box<AgentToolCall>,
    pub started: Option<Box<ToolStartRecordInfo>>,
    pub result_exists: bool,
    pub terminate: bool,
}

/// The pending tool batch of the open operation (upstream
/// `ToolBatchState`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolBatchState {
    pub assistant_entry_id: String,
    pub calls: Vec<ToolBatchCall>,
    pub truncated: bool,
    pub unresolved: bool,
}

/// The in-flight step of the open operation (upstream
/// `LaneState.operation.step`).
#[derive(Debug, Clone, PartialEq)]
pub struct OperationStep {
    /// `assistant` | `compaction` | `branch_summary`.
    pub kind: String,
    pub attempts: u32,
    pub result_entry_id: String,
    pub compaction_reason: Option<CompactionReason>,
}

/// Structural target presence (upstream `{ result?, summary? }`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationTargets {
    pub result: Option<bool>,
    pub summary: Option<bool>,
}

/// The newest operation-owned entry summary (upstream `newestOwn`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewestOwnEntry {
    pub entry_id: String,
    /// Upstream `Entry["type"]`.
    pub kind: String,
    /// Present when the entry is a message (upstream `role?`).
    pub role: Option<String>,
    /// Present for assistant messages (upstream `stopReason?`).
    pub stop_reason: Option<StopReason>,
}

/// The open operation state of one lane (upstream
/// `LaneState.operation`).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOperationState {
    pub id: String,
    /// `run` | `compaction` | `navigation`.
    pub kind: String,
    pub intent: Box<OperationIntent>,
    pub aborting: bool,
    pub step: Option<OperationStep>,
    pub tool_batch: Option<ToolBatchState>,
    pub missing_initial_messages: Vec<ProvisionedEntry>,
    pub pending_steer: Vec<ProvisionedEntry>,
    pub pending_follow_up: Vec<ProvisionedEntry>,
    pub pending_writes: Vec<ProvisionedEntry>,
    pub deferred: Option<DeferredHandle>,
    pub overflow_recovery_used: bool,
    pub newest_own: Option<NewestOwnEntry>,
    pub targets: OperationTargets,
}

/// The reduced orchestration state of one lane (upstream `LaneState`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneState {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub operation: Option<OpenOperationState>,
    pub pending_next_run: Vec<ProvisionedEntry>,
}

/// The reduction input (upstream `LaneReductionInput`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaneReductionInput {
    pub lane: String,
    pub open_operations: Vec<LaneRecord>,
    pub records: Vec<LaneRecord>,
    /// Entries fetched directly beyond the operation-owned set.
    pub entries: Vec<Entry>,
    /// Entries appended by the open operation, oldest first. Empty when
    /// idle.
    pub own_entries: Vec<Entry>,
    /// Bounded effective-state lookups at the operation anchor or idle
    /// leaf, oldest first.
    pub configuration_entries: Vec<Entry>,
    pub leaf_id: Option<String>,
    /// Harness option fallbacks used when no persisted value exists.
    pub defaults: EffectiveLaneConfiguration,
}

/// The reduction result (upstream `LaneReductionResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneReductionResult {
    pub lane_state: LaneState,
    pub effective_configuration: EffectiveLaneConfiguration,
    pub terminal_failure: Option<TerminalFailureState>,
}

fn corrupt(reason: RecordLogCorruptionReason, message: String) -> RecordLogCorruption {
    RecordLogCorruption::new(reason, message)
}

/// Upstream `hasRunId`: records that carry a run id; queue records carry it
/// optionally, and its absence skips operation-membership validation.
fn record_run_id(record: &LaneRecord) -> Option<&str> {
    match &record.payload {
        RecordPayload::AbortRequested { run_id } => Some(run_id),
        RecordPayload::OperationFinished { run_id, .. } => Some(run_id),
        RecordPayload::StepAttempt { run_id, .. } => Some(run_id),
        RecordPayload::ToolStarted { run_id, .. } => Some(run_id),
        RecordPayload::QueueEnqueued { run_id, .. } => run_id.as_deref(),
        RecordPayload::QueueCancelled { run_id, .. } => run_id.as_deref(),
        RecordPayload::WriteDeferred { run_id, .. } => Some(run_id),
        RecordPayload::UsageRecord { run_id, .. } => run_id.as_deref(),
        RecordPayload::OperationStarted { .. } => None,
    }
}

/// Upstream `matchesProvisionedEntry`: compare the entry's provisioned
/// shape (id + payload) with the recorded intent.
fn matches_provisioned_entry(entry: &Entry, target: &ProvisionedEntry) -> bool {
    entry.id == target.id && entry.payload == target.payload
}

fn validate_exact_provisioned_entry(
    entries_by_id: &HashMap<&str, &Entry>,
    target: &ProvisionedEntry,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(target.id.as_str()) {
        if !matches_provisioned_entry(entry, target) {
            return Err(corrupt(
                RecordLogCorruptionReason::ProvisionedEntryMismatch,
                format!(
                    "Provisioned entry {} exists with content different from its intent",
                    target.id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_result_entry(
    entries_by_id: &HashMap<&str, &Entry>,
    result_entry_id: &str,
    matches: impl Fn(&Entry) -> bool,
    description: &str,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(result_entry_id) {
        if !matches(entry) {
            return Err(corrupt(
                RecordLogCorruptionReason::ProvisionedEntryMismatch,
                format!(
                    "Provisioned {description} entry {result_entry_id} exists with different content"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_attempt_reason(record: &LaneRecord) -> Result<(), RecordLogCorruption> {
    let RecordPayload::StepAttempt {
        step,
        compaction_reason,
        ..
    } = &record.payload
    else {
        return Ok(());
    };
    if step == "compaction" {
        // divergence: the reason value is a closed enum in the port, so the
        // upstream membership check reduces to presence.
        if compaction_reason.is_none() {
            return Err(corrupt(
                RecordLogCorruptionReason::InvalidCompactionReason,
                format!(
                    "Compaction attempt {} has no valid compaction reason",
                    record.id
                ),
            ));
        }
    } else if compaction_reason.is_some() {
        return Err(corrupt(
            RecordLogCorruptionReason::InvalidCompactionReason,
            format!("{step} attempt {} has a compaction reason", record.id),
        ));
    }
    Ok(())
}

fn validate_attempt_sequence(
    record: &LaneRecord,
    previous: Option<&LaneRecord>,
    entries_by_id: &HashMap<&str, &Entry>,
) -> Result<(), RecordLogCorruption> {
    let RecordPayload::StepAttempt { step, attempt, .. } = &record.payload else {
        return Ok(());
    };
    let previous_record =
        previous.filter(|record| matches!(record.payload, RecordPayload::StepAttempt { .. }));
    let Some(previous_record) = previous_record else {
        if attempt != &1 {
            return Err(corrupt(
                RecordLogCorruptionReason::NonConsecutiveAttempt,
                format!("{step} attempt {} is {attempt}; expected 1", record.id),
            ));
        }
        return Ok(());
    };
    let RecordPayload::StepAttempt {
        step: previous_step,
        attempt: previous_attempt,
        result_entry_id: previous_result_entry_id,
        compaction_reason: previous_compaction_reason,
        ..
    } = &previous_record.payload
    else {
        unreachable!("filtered above");
    };
    let previous_result_seq = entries_by_id
        .get(previous_result_entry_id.as_str())
        .map(|entry| entry.seq);
    let continues_series = previous_step == step
        && previous_result_seq.is_none_or(|result_seq| result_seq >= record.seq);
    let expected_attempt = if continues_series {
        previous_attempt + 1
    } else {
        1
    };
    if attempt != &expected_attempt {
        return Err(corrupt(
            RecordLogCorruptionReason::NonConsecutiveAttempt,
            format!(
                "{step} attempt {} is {attempt}; expected {expected_attempt}",
                record.id
            ),
        ));
    }
    if !continues_series || step == "assistant" {
        return Ok(());
    }
    let RecordPayload::StepAttempt {
        result_entry_id,
        compaction_reason,
        ..
    } = &record.payload
    else {
        unreachable!("checked above");
    };
    if result_entry_id != previous_result_entry_id {
        return Err(corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!("{step} attempts disagree on their result entry id"),
        ));
    }
    if compaction_reason != previous_compaction_reason {
        return Err(corrupt(
            RecordLogCorruptionReason::InconsistentStep,
            format!("{step} attempts disagree on their compaction reason"),
        ));
    }
    Ok(())
}

/// The entry is an assistant message (upstream narrows on entry type and
/// message role).
fn as_assistant_entry(entry: &Entry) -> Option<&AssistantMessage> {
    let EntryPayload::Message { message, .. } = &entry.payload else {
        return None;
    };
    match message.as_message() {
        Some(pillar_ai::types::Message::Assistant(assistant)) => Some(assistant),
        _ => None,
    }
}

fn validate_attempt_result(
    entries_by_id: &HashMap<&str, &Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let RecordPayload::StepAttempt {
        step,
        result_entry_id,
        ..
    } = &record.payload
    else {
        return Ok(());
    };
    match step.as_str() {
        "assistant" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |entry| as_assistant_entry(entry).is_some(),
            "assistant result",
        ),
        "compaction" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |entry| entry.kind == "compaction",
            "compaction result",
        ),
        "branch_summary" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |entry| entry.kind == "branch_summary",
            "branch-summary result",
        ),
        _ => Ok(()),
    }
}

fn validate_tool_start(
    record: &LaneRecord,
    entries_by_id: &HashMap<&str, &Entry>,
    invocations: &mut HashSet<(String, u32)>,
) -> Result<(), RecordLogCorruption> {
    let RecordPayload::ToolStarted {
        assistant_entry_id,
        tool_index,
        tool_call_id,
        tool_name,
        result_entry_id,
        ..
    } = &record.payload
    else {
        return Ok(());
    };
    let invocation = (assistant_entry_id.clone(), *tool_index);
    if invocations.contains(&invocation) {
        return Err(corrupt(
            RecordLogCorruptionReason::DuplicateToolInvocation,
            format!("Tool invocation {assistant_entry_id}:{tool_index} is duplicated"),
        ));
    }
    invocations.insert(invocation);

    let mismatch = |record: &LaneRecord| {
        corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!(
                "Tool start {} does not reference an assistant entry",
                record.id
            ),
        )
    };
    let Some(assistant_entry) = entries_by_id.get(assistant_entry_id.as_str()) else {
        return Err(mismatch(record));
    };
    let Some(assistant) = as_assistant_entry(assistant_entry) else {
        return Err(mismatch(record));
    };
    let tool_calls: Vec<&pillar_ai::types::Content> = assistant
        .content
        .iter()
        .filter(|content| matches!(content, pillar_ai::types::Content::ToolCall { .. }))
        .collect();
    let ordinal_mismatch = |record: &LaneRecord| {
        corrupt(
            RecordLogCorruptionReason::ToolCallMismatch,
            format!(
                "Tool start {} does not match its assistant tool-call ordinal",
                record.id
            ),
        )
    };
    let Some(tool_call) = tool_calls.get(*tool_index as usize) else {
        return Err(ordinal_mismatch(record));
    };
    let pillar_ai::types::Content::ToolCall {
        id: call_id,
        name: call_name,
        ..
    } = tool_call
    else {
        unreachable!("filtered above");
    };
    if call_id != tool_call_id || call_name != tool_name {
        return Err(ordinal_mismatch(record));
    }

    validate_result_entry(
        entries_by_id,
        result_entry_id,
        |entry| {
            let EntryPayload::Message { message, .. } = &entry.payload else {
                return false;
            };
            matches!(
                message.as_message(),
                Some(pillar_ai::types::Message::ToolResult(result))
                    if result.tool_call_id == *tool_call_id && result.tool_name == *tool_name
            )
        },
        "tool result",
    )
}

fn validate_deferred_handles<'a>(
    entries: impl Iterator<Item = &'a Entry>,
) -> Result<(), RecordLogCorruption> {
    for entry in entries {
        let Some(assistant) = as_assistant_entry(entry) else {
            continue;
        };
        if assistant.stop_reason == StopReason::Deferred && assistant.deferred.is_none() {
            return Err(corrupt(
                RecordLogCorruptionReason::InvalidDeferredHandle,
                format!(
                    "Deferred assistant entry {} does not carry a handle",
                    entry.id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_operation_result(
    entries_by_id: &HashMap<&str, &Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let RecordPayload::OperationStarted { intent, .. } = &record.payload else {
        return Ok(());
    };
    match intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            for target in initial_messages {
                validate_exact_provisioned_entry(entries_by_id, target)?;
            }
            Ok(())
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |entry| entry.kind == "compaction",
            "manual compaction",
        ),
        OperationIntent::Navigation {
            summary_entry_id, ..
        } => {
            if let Some(summary_entry_id) = summary_entry_id {
                return validate_result_entry(
                    entries_by_id,
                    summary_entry_id,
                    |entry| entry.kind == "branch_summary",
                    "navigation summary",
                );
            }
            Ok(())
        }
    }
}

/// Validates a bounded lane recovery slice without reading or mutating
/// session state (upstream `validateRecordLog`).
pub fn validate_record_log(input: &RecordLogSlice) -> Result<(), RecordLogCorruption> {
    validate_lane_slice(
        &input.lane,
        &input.open_operations,
        &input.records,
        &input.entries,
    )
}

fn validate_lane_slice(
    lane: &str,
    open_operations: &[LaneRecord],
    records: &[LaneRecord],
    entries: &[Entry],
) -> Result<(), RecordLogCorruption> {
    if open_operations.len() > 1 {
        return Err(corrupt(
            RecordLogCorruptionReason::MultipleOpenOperations,
            format!("Lane {lane} has at least two open operations"),
        ));
    }

    let mut entries_by_id: HashMap<&str, &Entry> = HashMap::new();
    for entry in entries {
        entries_by_id.insert(entry.id.as_str(), entry);
    }
    validate_deferred_handles(entries.iter())?;

    let mut starts: HashSet<&str> = HashSet::new();
    let mut finished_at: HashMap<&str, u64> = HashMap::new();
    let mut aborted_at: HashMap<&str, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<&str, &LaneRecord> = HashMap::new();
    let mut latest_attempt: HashMap<&str, &LaneRecord> = HashMap::new();
    let mut tool_invocations: HashSet<(String, u32)> = HashSet::new();
    let mut ordered: Vec<&LaneRecord> = records.iter().collect();
    ordered.sort_by_key(|record| record.seq);

    for record in ordered {
        if let RecordPayload::OperationStarted { .. } = &record.payload {
            validate_operation_result(&entries_by_id, record)?;
            starts.insert(record.id.as_str());
            continue;
        }

        if let Some(run_id) = record_run_id(record) {
            if !starts.contains(run_id) {
                return Err(corrupt(
                    RecordLogCorruptionReason::UnknownOperation,
                    format!(
                        "Record {} references unknown operation {}",
                        record.id, run_id
                    ),
                ));
            }
            if let Some(finish_seq) = finished_at.get(run_id) {
                if record.seq > *finish_seq {
                    return Err(corrupt(
                        RecordLogCorruptionReason::RecordAfterFinish,
                        format!(
                            "Record {} follows the finish of operation {}",
                            record.id, run_id
                        ),
                    ));
                }
            }
        }

        match &record.payload {
            RecordPayload::OperationFinished { run_id, .. } => {
                finished_at.insert(run_id.as_str(), record.seq);
            }
            RecordPayload::AbortRequested { run_id } => {
                aborted_at.insert(run_id.as_str(), record.seq);
            }
            RecordPayload::StepAttempt { run_id, .. } => {
                validate_attempt_reason(record)?;
                validate_attempt_sequence(
                    record,
                    latest_attempt.get(run_id.as_str()).copied(),
                    &entries_by_id,
                )?;
                validate_attempt_result(&entries_by_id, record)?;
                latest_attempt.insert(run_id.as_str(), record);
            }
            RecordPayload::ToolStarted { .. } => {
                validate_tool_start(record, &entries_by_id, &mut tool_invocations)?;
            }
            RecordPayload::QueueEnqueued {
                queue,
                run_id,
                target,
            } => {
                if queue != "nextRun" {
                    if let Some(abort_seq) = run_id.as_deref().and_then(|r| aborted_at.get(r)) {
                        if record.seq > *abort_seq {
                            return Err(corrupt(
                                RecordLogCorruptionReason::QueueAfterAbort,
                                format!("{queue} item {} was enqueued after abort", target.id),
                            ));
                        }
                    }
                }
                queue_enqueues.insert(target.id.as_str(), record);
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            RecordPayload::QueueCancelled { run_id, entry_id } => {
                let enqueue = queue_enqueues.get(entry_id.as_str()).copied();
                let mismatch = match enqueue {
                    None => true,
                    Some(enqueue) => {
                        let RecordPayload::QueueEnqueued {
                            run_id: enqueue_run_id,
                            ..
                        } = &enqueue.payload
                        else {
                            unreachable!("stored enqueues only");
                        };
                        enqueue.seq >= record.seq
                            || enqueue_run_id.as_deref() != run_id.as_deref()
                            || entries_by_id.contains_key(entry_id.as_str())
                    }
                };
                if mismatch {
                    return Err(corrupt(
                        RecordLogCorruptionReason::InvalidQueueCancellation,
                        format!(
                            "Queue cancellation {} has no pending matching enqueue",
                            record.id
                        ),
                    ));
                }
            }
            RecordPayload::WriteDeferred { target, .. } => {
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            RecordPayload::UsageRecord { .. } => {}
            RecordPayload::OperationStarted { .. } => unreachable!("handled above"),
        }
    }
    Ok(())
}

fn by_sequence<'a, T>(values: impl IntoIterator<Item = T>) -> Vec<T>
where
    T: HasSeq + 'a,
{
    let mut values: Vec<T> = values.into_iter().collect();
    values.sort_by_key(|value| value.seq());
    values
}

/// Uniform `seq` access over records/entries by reference or value.
trait HasSeq {
    fn seq(&self) -> u64;
}

impl HasSeq for LaneRecord {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl HasSeq for Entry {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl<T: HasSeq> HasSeq for &T {
    fn seq(&self) -> u64 {
        (*self).seq()
    }
}

fn derive_effective_configuration(input: &LaneReductionInput) -> EffectiveLaneConfiguration {
    let mut configuration = input.defaults.clone();
    let mut entries_by_id: HashMap<&str, &Entry> = HashMap::new();
    for entry in input
        .configuration_entries
        .iter()
        .chain(input.own_entries.iter())
    {
        entries_by_id.insert(entry.id.as_str(), entry);
    }

    for entry in by_sequence(entries_by_id.values().copied()) {
        match &entry.payload {
            EntryPayload::ModelChange { provider, model_id } => {
                configuration.model = LaneModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                };
            }
            EntryPayload::ThinkingLevelChange { thinking_level } => {
                configuration.thinking_level = parse_thinking_level(thinking_level);
            }
            EntryPayload::ActiveToolsChange {
                active_tool_names: names,
            } => {
                configuration.active_tool_names = names.clone();
            }
            EntryPayload::Message { message, .. } => {
                if let Some(pillar_ai::types::Message::Assistant(assistant)) = message.as_message()
                {
                    configuration.model = LaneModelRef {
                        provider: assistant.provider.clone(),
                        model_id: assistant.model.clone(),
                    };
                }
            }
            _ => {}
        }
    }
    configuration
}

/// Upstream stores the level as its string; the port's closed enum cannot
/// hold unknown strings, so they fall back to "off".
fn parse_thinking_level(level: &str) -> AgentThinkingLevel {
    match level {
        "minimal" => AgentThinkingLevel::Minimal,
        "low" => AgentThinkingLevel::Low,
        "medium" => AgentThinkingLevel::Medium,
        "high" => AgentThinkingLevel::High,
        "xhigh" => AgentThinkingLevel::Xhigh,
        "max" => AgentThinkingLevel::Max,
        _ => AgentThinkingLevel::Off,
    }
}

fn derive_newest_own(entry: Option<&Entry>) -> Option<NewestOwnEntry> {
    let entry = entry?;
    let EntryPayload::Message { message, .. } = &entry.payload else {
        return Some(NewestOwnEntry {
            entry_id: entry.id.clone(),
            kind: entry.kind.clone(),
            role: None,
            stop_reason: None,
        });
    };
    let stop_reason = match message.as_message() {
        Some(pillar_ai::types::Message::Assistant(assistant)) => Some(assistant.stop_reason),
        _ => None,
    };
    Some(NewestOwnEntry {
        entry_id: entry.id.clone(),
        kind: entry.kind.clone(),
        role: Some(message.role_name().to_owned()),
        stop_reason,
    })
}

fn derive_tool_batch(
    operation_id: &str,
    operation_records: &[&LaneRecord],
    own_entries: &[&Entry],
    entries_by_id: &HashMap<&str, &Entry>,
    deferred_write_ids: &HashSet<&str>,
) -> Option<ToolBatchState> {
    let assistant_entry = own_entries.iter().rev().find(|entry| {
        as_assistant_entry(entry).is_some_and(|assistant| {
            assistant
                .content
                .iter()
                .any(|content| matches!(content, pillar_ai::types::Content::ToolCall { .. }))
        })
    })?;
    let assistant = as_assistant_entry(assistant_entry)?;

    let mut starts: HashMap<u32, &LaneRecord> = HashMap::new();
    for record in operation_records {
        if let RecordPayload::ToolStarted {
            run_id,
            assistant_entry_id,
            tool_index,
            ..
        } = &record.payload
        {
            if run_id == operation_id && assistant_entry_id == &assistant_entry.id {
                starts.insert(*tool_index, record);
            }
        }
    }

    let mut calls = Vec::new();
    for (tool_index, content) in assistant.content.iter().enumerate() {
        let pillar_ai::types::Content::ToolCall {
            id: call_id,
            name: call_name,
            arguments,
            ..
        } = content
        else {
            continue;
        };
        let started = starts.get(&(tool_index as u32)).copied();
        let started_result = started.and_then(|record| match &record.payload {
            RecordPayload::ToolStarted {
                result_entry_id, ..
            } => entries_by_id.get(result_entry_id.as_str()).copied(),
            _ => unreachable!("filtered above"),
        });
        let blocked_result = own_entries.iter().copied().find(|entry| {
            entry.seq > assistant_entry.seq
                && !deferred_write_ids.contains(entry.id.as_str())
                && matches!(
                    &entry.payload,
                    EntryPayload::Message { message, .. }
                        if matches!(
                            message.as_message(),
                            Some(pillar_ai::types::Message::ToolResult(result))
                                if result.tool_call_id == *call_id
                        )
                )
        });
        let result = started_result.or(blocked_result);
        let terminate = matches!(
            result.map(|entry| &entry.payload),
            Some(EntryPayload::Message {
                terminate: true,
                ..
            })
        );
        calls.push(ToolBatchCall {
            tool_index,
            tool_call: Box::new(AgentToolCall {
                id: call_id.clone(),
                name: call_name.clone(),
                arguments: arguments.clone(),
            }),
            started: started.map(|record| match &record.payload {
                RecordPayload::ToolStarted {
                    run_id,
                    assistant_entry_id,
                    tool_index,
                    tool_call_id,
                    tool_name,
                    effective_args,
                    result_entry_id,
                    replay,
                } => Box::new(ToolStartRecordInfo {
                    run_id: run_id.clone(),
                    assistant_entry_id: assistant_entry_id.clone(),
                    tool_index: *tool_index,
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    effective_args: effective_args.clone(),
                    result_entry_id: result_entry_id.clone(),
                    replay: replay.clone(),
                }),
                _ => unreachable!("filtered above"),
            }),
            result_exists: result.is_some(),
            terminate,
        });
    }

    let unresolved = calls.iter().any(|call| !call.result_exists);
    Some(ToolBatchState {
        assistant_entry_id: assistant_entry.id.clone(),
        calls,
        truncated: assistant.stop_reason == StopReason::Length,
        unresolved,
    })
}

/// Purely reconstructs one lane's orchestration state from its bounded
/// recovery inputs (upstream `reduceLaneState`).
pub fn reduce_lane_state(
    input: &LaneReductionInput,
) -> Result<LaneReductionResult, RecordLogCorruption> {
    let LaneReductionInput {
        lane,
        open_operations,
        records,
        entries,
        own_entries,
        configuration_entries: _,
        leaf_id,
        defaults: _,
    } = input;
    validate_lane_slice(lane, open_operations, records, entries)?;

    let ordered_records = by_sequence(records.iter());
    let ordered_own_entries = by_sequence(own_entries.iter());
    let mut entries_by_id: HashMap<&str, &Entry> = HashMap::new();
    for entry in entries.iter().chain(own_entries.iter()) {
        entries_by_id.insert(entry.id.as_str(), entry);
    }
    let cancelled_queue_ids: HashSet<&str> = ordered_records
        .iter()
        .filter_map(|record| match &record.payload {
            RecordPayload::QueueCancelled { entry_id, .. } => Some(entry_id.as_str()),
            _ => None,
        })
        .collect();
    let pending_queue_records: Vec<&LaneRecord> = ordered_records
        .iter()
        .filter(|record| match &record.payload {
            RecordPayload::QueueEnqueued { target, .. } => {
                !entries_by_id.contains_key(target.id.as_str())
                    && !cancelled_queue_ids.contains(target.id.as_str())
            }
            _ => false,
        })
        .copied()
        .collect();
    let started = open_operations.first();
    let captured_initial_message_ids: HashSet<&str> = started
        .and_then(|record| match &record.payload {
            RecordPayload::OperationStarted {
                intent:
                    OperationIntent::Run {
                        initial_messages, ..
                    },
                ..
            } => Some(
                initial_messages
                    .iter()
                    .map(|target| target.id.as_str())
                    .collect::<HashSet<&str>>(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    let pending_next_run: Vec<ProvisionedEntry> = pending_queue_records
        .iter()
        .filter_map(|record| match &record.payload {
            RecordPayload::QueueEnqueued { queue, target, .. }
                if queue == "nextRun"
                    && !captured_initial_message_ids.contains(target.id.as_str()) =>
            {
                Some(target.clone())
            }
            _ => None,
        })
        .collect();
    let effective_configuration = derive_effective_configuration(input);

    let Some(started) = started else {
        return Ok(LaneReductionResult {
            lane_state: LaneState {
                lane: lane.clone(),
                leaf_id: leaf_id.clone(),
                operation: None,
                pending_next_run,
            },
            effective_configuration,
            terminal_failure: None,
        });
    };

    let RecordPayload::OperationStarted {
        intent: started_intent,
        ..
    } = &started.payload
    else {
        unreachable!("open operations are operation_started");
    };
    let started_id = started.id.as_str();
    let operation_records: Vec<&LaneRecord> = ordered_records
        .iter()
        .filter(|record| match &record.payload {
            RecordPayload::OperationStarted { .. } => record.id == started_id,
            _ => record_run_id(record) == Some(started_id),
        })
        .copied()
        .collect();
    let aborting = operation_records
        .iter()
        .any(|record| matches!(record.payload, RecordPayload::AbortRequested { .. }));
    let pending_steer: Vec<ProvisionedEntry> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter_map(|record| match &record.payload {
                RecordPayload::QueueEnqueued {
                    queue,
                    target,
                    run_id,
                } if queue == "steer" && run_id.as_deref() == Some(started_id) => {
                    Some(target.clone())
                }
                _ => None,
            })
            .collect()
    };
    let pending_follow_up: Vec<ProvisionedEntry> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter_map(|record| match &record.payload {
                RecordPayload::QueueEnqueued {
                    queue,
                    target,
                    run_id,
                } if queue == "followUp" && run_id.as_deref() == Some(started_id) => {
                    Some(target.clone())
                }
                _ => None,
            })
            .collect()
    };
    let pending_writes: Vec<ProvisionedEntry> = operation_records
        .iter()
        .filter_map(|record| match &record.payload {
            RecordPayload::WriteDeferred { target, .. }
                if !entries_by_id.contains_key(target.id.as_str()) =>
            {
                Some(target.clone())
            }
            _ => None,
        })
        .collect();
    let missing_initial_messages: Vec<ProvisionedEntry> = match started_intent {
        OperationIntent::Run {
            initial_messages, ..
        } => initial_messages
            .iter()
            .filter(|target| !entries_by_id.contains_key(target.id.as_str()))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };

    let newest_attempt = operation_records
        .iter()
        .filter(|record| matches!(record.payload, RecordPayload::StepAttempt { .. }))
        .next_back();
    let step = newest_attempt.and_then(|record| {
        let RecordPayload::StepAttempt {
            step,
            attempt,
            result_entry_id,
            compaction_reason,
            ..
        } = &record.payload
        else {
            unreachable!("filtered above");
        };
        if entries_by_id.contains_key(result_entry_id.as_str()) {
            return None;
        }
        Some(OperationStep {
            kind: step.clone(),
            attempts: *attempt,
            result_entry_id: result_entry_id.clone(),
            compaction_reason: *compaction_reason,
        })
    });

    let mut consumed_input_ids: HashSet<&str> = HashSet::new();
    if let OperationIntent::Run {
        initial_messages, ..
    } = started_intent
    {
        for target in initial_messages {
            consumed_input_ids.insert(target.id.as_str());
        }
    }
    for record in &operation_records {
        if let RecordPayload::QueueEnqueued { queue, target, .. } = &record.payload {
            if queue != "nextRun" {
                consumed_input_ids.insert(target.id.as_str());
            }
        }
    }
    // Upstream walks from NEGATIVE_INFINITY, so "no consumed message" means
    // any overflow attempt counts as recovery; `None` models that here.
    let newest_consumed_input_sequence: Option<u64> = consumed_input_ids
        .iter()
        .filter_map(|id| entries_by_id.get(*id))
        .filter(|entry| matches!(&entry.payload, EntryPayload::Message { .. }))
        .map(|entry| entry.seq)
        .max();
    let overflow_recovery_used = operation_records.iter().any(|record| {
        matches!(
            &record.payload,
            RecordPayload::StepAttempt {
                step,
                compaction_reason: Some(CompactionReason::Overflow),
                ..
            } if step == "compaction"
        ) && newest_consumed_input_sequence.is_none_or(|seq| record.seq > seq)
    });

    let newest_own_entry = ordered_own_entries.last().copied();
    let newest_own = derive_newest_own(newest_own_entry);
    let deferred: Option<DeferredHandle> = newest_own_entry.and_then(|entry| {
        as_assistant_entry(entry)
            .filter(|assistant| assistant.stop_reason == StopReason::Deferred)
            .and_then(|assistant| assistant.deferred.clone())
    });
    let mut targets = OperationTargets::default();
    match started_intent {
        OperationIntent::Compaction {
            result_entry_id, ..
        } => {
            targets.result = Some(entries_by_id.contains_key(result_entry_id.as_str()));
        }
        OperationIntent::Navigation {
            summary_entry_id: Some(summary_entry_id),
            ..
        } => {
            targets.summary = Some(entries_by_id.contains_key(summary_entry_id.as_str()));
        }
        _ => {}
    }

    let deferred_write_ids: HashSet<&str> = operation_records
        .iter()
        .filter_map(|record| match &record.payload {
            RecordPayload::WriteDeferred { target, .. } => Some(target.id.as_str()),
            _ => None,
        })
        .collect();
    let mut terminal_failure: Option<TerminalFailureState> = None;
    if let Some(newest_own_entry) = newest_own_entry {
        if let Some(assistant) = as_assistant_entry(newest_own_entry) {
            if assistant.stop_reason == StopReason::Error
                && !deferred_write_ids.contains(newest_own_entry.id.as_str())
            {
                let produced_by_step = operation_records.iter().any(|record| {
                    matches!(
                        &record.payload,
                        RecordPayload::StepAttempt { result_entry_id, .. }
                            if result_entry_id == &newest_own_entry.id
                    )
                });
                let previous_own_entry = ordered_own_entries
                    .len()
                    .checked_sub(2)
                    .map(|index| ordered_own_entries[index]);
                let produced_by_deferred_fetch = operation_records.iter().any(|record| {
                    matches!(
                        &record.payload,
                        RecordPayload::UsageRecord { cause, entry_id: Some(entry_id), .. }
                            if cause == "deferred_fetch" && entry_id == &newest_own_entry.id
                    )
                }) || matches!(
                    previous_own_entry.map(|entry| &entry.payload),
                    Some(EntryPayload::Message { message, .. })
                        if matches!(
                            message.as_message(),
                            Some(pillar_ai::types::Message::Assistant(a))
                                if a.stop_reason == StopReason::Deferred
                        )
                );
                if produced_by_step || produced_by_deferred_fetch {
                    terminal_failure = Some(TerminalFailureState {
                        entry_id: newest_own_entry.id.clone(),
                        source: if produced_by_step {
                            TerminalFailureSource::Step
                        } else {
                            TerminalFailureSource::DeferredFetch
                        },
                        message: Box::new(assistant.clone()),
                    });
                }
            }
        }
    }

    let tool_batch = derive_tool_batch(
        started_id,
        &operation_records,
        &ordered_own_entries,
        &entries_by_id,
        &deferred_write_ids,
    );
    let kind = match started_intent {
        OperationIntent::Run { .. } => "run",
        OperationIntent::Compaction { .. } => "compaction",
        OperationIntent::Navigation { .. } => "navigation",
    };

    Ok(LaneReductionResult {
        lane_state: LaneState {
            lane: lane.clone(),
            leaf_id: leaf_id.clone(),
            operation: Some(OpenOperationState {
                id: started.id.clone(),
                kind: kind.to_owned(),
                intent: Box::new(started_intent.clone()),
                aborting,
                step,
                tool_batch,
                missing_initial_messages,
                pending_steer,
                pending_follow_up,
                pending_writes,
                deferred,
                overflow_recovery_used,
                newest_own,
                targets,
            }),
            pending_next_run,
        },
        effective_configuration,
        terminal_failure,
    })
}
