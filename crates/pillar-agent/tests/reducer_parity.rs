//! Port of packages/agent/test/harness/reducer.test.ts (pi v0.84.3):
//! record-log validity (12 corruption cases + prefix acceptance), lane-state
//! reduction, effective configuration, tool batch state, deferred handles,
//! terminal-failure provenance, and determinism/no-mutation guarantees.

use std::collections::HashSet;

use pillar_agent::harness::reducer::{
    EffectiveLaneConfiguration, LaneModelRef, LaneReductionInput, LaneReductionResult, LaneState,
    OperationStep, OperationTargets, RecordLogCorruptionReason, TerminalFailureSource,
    reduce_lane_state, validate_record_log,
};
use pillar_agent::harness::session::types::{
    CompactionReason, Entry, EntryPayload, LaneRecord, OperationIntent, ProvisionedEntry,
    RecordPayload,
};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, StopReason, Usage, UsageCost, UserContent};

const USAGE: Usage = Usage {
    input: 1,
    output: 1,
    cache_read: 0,
    cache_write: 0,
    cache_write_1h: None,
    reasoning: None,
    total_tokens: 2,
    cost: UsageCost {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        total: 0.0,
    },
};

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn assistant_message(content: Vec<Content>, stop_reason: StopReason) -> AgentMessage {
    AgentMessage::Message(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content,
            api: "openai-responses".into(),
            provider: "openai".into(),
            model: "test-model".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: USAGE,
            stop_reason,
            deferred: (stop_reason == StopReason::Deferred).then(|| {
                pillar_ai::types::DeferredHandle {
                    provider: "openai".into(),
                    model_id: "test-model".to_owned(),
                    api: "openai-responses".into(),
                    id: "deferred-1".to_owned(),
                    expires_at: None,
                    poll_after_ms: None,
                    data: None,
                }
            }),
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    )))
}

fn tool_result_message(tool_call_id: &str, tool_name: &str) -> AgentMessage {
    AgentMessage::Message(Message::ToolResult(Box::new(
        pillar_ai::types::ToolResultMessage {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            content: vec![Content::text("result")],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 1,
        },
    )))
}

fn text_content(text: &str) -> Content {
    Content::text(text)
}

/// A deferred assistant message without its handle (upstream
/// `{ ...assistantMessage([], "deferred"), deferred: undefined }`).
fn assistant_message_no_handle() -> AgentMessage {
    let AgentMessage::Message(Message::Assistant(base)) =
        assistant_message(Vec::new(), StopReason::Deferred)
    else {
        unreachable!()
    };
    AgentMessage::Message(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            deferred: None,
            ..*base
        },
    )))
}

fn tool_call_content(id: &str, name: &str) -> Content {
    Content::tool_call(id, name, serde_json::json!({}))
}

fn message_target(id: &str, message: AgentMessage) -> ProvisionedEntry {
    provisioned_from_payload(
        id,
        EntryPayload::Message {
            message,
            terminate: false,
        },
    )
}

fn provisioned_from_payload(id: &str, payload: EntryPayload) -> ProvisionedEntry {
    ProvisionedEntry {
        kind: payload.kind().to_owned(),
        id: id.to_owned(),
        payload,
    }
}

fn persisted_entry(target: &ProvisionedEntry, seq: u64, parent_id: Option<&str>) -> Entry {
    Entry {
        kind: match &target.payload {
            EntryPayload::Message { .. } => "message",
            EntryPayload::ModelChange { .. } => "model_change",
            EntryPayload::ThinkingLevelChange { .. } => "thinking_level_change",
            EntryPayload::ActiveToolsChange { .. } => "active_tools_change",
            EntryPayload::Compaction { .. } => "compaction",
            EntryPayload::BranchSummary { .. } => "branch_summary",
            EntryPayload::Custom { .. } => "custom",
        }
        .to_owned(),
        id: target.id.clone(),
        seq,
        parent_id: parent_id.map(str::to_owned),
        timestamp: seq,
        payload: target.payload.clone(),
    }
}

fn persisted_message(id: &str, message: AgentMessage, seq: u64, parent_id: Option<&str>) -> Entry {
    let target = message_target(id, message);
    persisted_entry(&target, seq, parent_id)
}

fn record(kind: &str, id: &str, seq: u64, payload: RecordPayload) -> LaneRecord {
    LaneRecord {
        kind: kind.to_owned(),
        id: id.to_owned(),
        seq,
        lane: "main".to_owned(),
        timestamp: seq,
        payload,
    }
}

fn run_started(seq: u64, id: &str, initial_messages: Vec<ProvisionedEntry>) -> LaneRecord {
    record(
        "operation_started",
        id,
        seq,
        RecordPayload::OperationStarted {
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages,
                system_prompt_override: None,
                resume_data: None,
            },
        },
    )
}

fn compaction_started(seq: u64, result_entry_id: &str) -> LaneRecord {
    record(
        "operation_started",
        "compact-1",
        seq,
        RecordPayload::OperationStarted {
            source_leaf_id: Some("source".to_owned()),
            intent: OperationIntent::Compaction {
                custom_instructions: None,
                result_entry_id: result_entry_id.to_owned(),
            },
        },
    )
}

fn navigation_started(seq: u64, summary_entry_id: &str) -> LaneRecord {
    record(
        "operation_started",
        "navigate-1",
        seq,
        RecordPayload::OperationStarted {
            source_leaf_id: Some("source".to_owned()),
            intent: OperationIntent::Navigation {
                target_id: Some("target".to_owned()),
                summarize: true,
                custom_instructions: None,
                label: None,
                summary_entry_id: Some(summary_entry_id.to_owned()),
            },
        },
    )
}

fn attempt(
    seq: u64,
    run_id: &str,
    step: &str,
    attempt_number: u32,
    result_entry_id: &str,
    compaction_reason: Option<CompactionReason>,
) -> LaneRecord {
    record(
        "step_attempt",
        &format!("attempt-{seq}"),
        seq,
        RecordPayload::StepAttempt {
            run_id: run_id.to_owned(),
            step: step.to_owned(),
            attempt: attempt_number,
            result_entry_id: result_entry_id.to_owned(),
            compaction_reason,
        },
    )
}

fn abort_requested(seq: u64, run_id: &str) -> LaneRecord {
    record(
        "abort_requested",
        &format!("abort-{seq}"),
        seq,
        RecordPayload::AbortRequested {
            run_id: run_id.to_owned(),
        },
    )
}

fn operation_finished(seq: u64, run_id: &str) -> LaneRecord {
    record(
        "operation_finished",
        &format!("finish-{seq}"),
        seq,
        RecordPayload::OperationFinished {
            run_id: run_id.to_owned(),
            outcome: pillar_agent::harness::session::types::OperationOutcome::Completed,
            error: None,
        },
    )
}

fn tool_started(seq: u64, overrides: ToolStartOverrides) -> LaneRecord {
    record(
        "tool_started",
        &overrides.id.unwrap_or_else(|| format!("tool-start-{seq}")),
        seq,
        RecordPayload::ToolStarted {
            run_id: "run-1".to_owned(),
            assistant_entry_id: overrides
                .assistant_entry_id
                .unwrap_or_else(|| "assistant-tools".to_owned()),
            tool_index: overrides.tool_index.unwrap_or(0),
            tool_call_id: overrides
                .tool_call_id
                .unwrap_or_else(|| "call-1".to_owned()),
            tool_name: overrides.tool_name.unwrap_or_else(|| "tool-1".to_owned()),
            effective_args: serde_json::json!({}),
            result_entry_id: overrides
                .result_entry_id
                .unwrap_or_else(|| "tool-result-1".to_owned()),
            replay: "never".to_owned(),
        },
    )
}

#[derive(Default)]
struct ToolStartOverrides {
    id: Option<String>,
    assistant_entry_id: Option<String>,
    tool_index: Option<u32>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    result_entry_id: Option<String>,
}

fn queue_enqueued(seq: u64, target: ProvisionedEntry, queue: &str) -> LaneRecord {
    record(
        "queue_enqueued",
        &format!("queue-{seq}"),
        seq,
        RecordPayload::QueueEnqueued {
            queue: queue.to_owned(),
            run_id: (queue != "nextRun").then(|| "run-1".to_owned()),
            target,
        },
    )
}

fn queue_cancelled(seq: u64, entry_id: &str, run_id: Option<&str>) -> LaneRecord {
    record(
        "queue_cancelled",
        &format!("cancel-{seq}"),
        seq,
        RecordPayload::QueueCancelled {
            run_id: run_id.map(str::to_owned),
            entry_id: entry_id.to_owned(),
        },
    )
}

fn write_deferred(seq: u64, target: ProvisionedEntry) -> LaneRecord {
    record(
        "write_deferred",
        &format!("write-{seq}"),
        seq,
        RecordPayload::WriteDeferred {
            run_id: "run-1".to_owned(),
            target,
        },
    )
}

fn usage_record(
    seq: u64,
    entry_id: &str,
    cause: &str,
    stop_reason: Option<StopReason>,
) -> LaneRecord {
    record(
        "usage",
        &format!("usage-{seq}"),
        seq,
        RecordPayload::UsageRecord {
            usage: USAGE,
            cause: cause.to_owned(),
            run_id: Some("run-1".to_owned()),
            entry_id: Some(entry_id.to_owned()),
            attempt: Some(1),
            tool_call_id: None,
            stop_reason: stop_reason.map(|reason| match reason {
                StopReason::Stop => "stop".to_owned(),
                StopReason::Length => "length".to_owned(),
                StopReason::ToolUse => "toolUse".to_owned(),
                StopReason::Error => "error".to_owned(),
                StopReason::Aborted => "aborted".to_owned(),
                StopReason::Deferred => "deferred".to_owned(),
                StopReason::Pending => unreachable!("test stop reasons exclude pending"),
            }),
            details: None,
        },
    )
}

fn compaction_entry(id: &str, seq: u64) -> Entry {
    Entry {
        kind: "compaction".to_owned(),
        id: id.to_owned(),
        seq,
        parent_id: None,
        timestamp: seq,
        payload: EntryPayload::Compaction {
            summary: "summary".to_owned(),
            retained_tail: Vec::new(),
            tokens_before: 10,
            details: None,
            usage: None,
        },
    }
}

fn branch_summary_entry(id: &str, seq: u64) -> Entry {
    Entry {
        kind: "branch_summary".to_owned(),
        id: id.to_owned(),
        seq,
        parent_id: Some("target".to_owned()),
        timestamp: seq,
        payload: EntryPayload::BranchSummary {
            from_id: "source".to_owned(),
            summary: "summary".to_owned(),
            details: None,
            usage: None,
        },
    }
}

fn recovery_slice(
    records: Vec<LaneRecord>,
    entries: Vec<Entry>,
) -> pillar_agent::harness::reducer::RecordLogSlice {
    let finished: HashSet<&str> = records
        .iter()
        .filter_map(|record| match &record.payload {
            RecordPayload::OperationFinished { run_id, .. } => Some(run_id.as_str()),
            _ => None,
        })
        .collect();
    let mut open_operations: Vec<LaneRecord> = records
        .iter()
        .filter(|record| {
            matches!(record.payload, RecordPayload::OperationStarted { .. })
                && !finished.contains(record.id.as_str())
        })
        .cloned()
        .collect();
    open_operations.sort_by(|left, right| right.seq.cmp(&left.seq));
    pillar_agent::harness::reducer::RecordLogSlice {
        lane: "main".to_owned(),
        open_operations,
        records,
        entries,
    }
}

fn defaults() -> EffectiveLaneConfiguration {
    EffectiveLaneConfiguration {
        model: LaneModelRef {
            provider: "default-provider".to_owned(),
            model_id: "default-model".to_owned(),
        },
        thinking_level: pillar_agent::types::thinking::AgentThinkingLevel::Off,
        active_tool_names: vec!["default-tool".to_owned()],
    }
}

fn reduction_input(
    records: Vec<LaneRecord>,
    own_entries: Vec<Entry>,
    options: ReductionOptions,
) -> LaneReductionInput {
    let leaf_id = options
        .leaf_id
        .or_else(|| own_entries.last().map(|entry| entry.id.clone()));
    let slice = recovery_slice(
        records,
        own_entries
            .iter()
            .cloned()
            .chain(options.entries.iter().cloned())
            .collect(),
    );
    LaneReductionInput {
        lane: slice.lane,
        open_operations: slice.open_operations,
        records: slice.records,
        entries: slice.entries,
        own_entries,
        configuration_entries: options.configuration_entries,
        leaf_id,
        defaults: options.defaults.unwrap_or_else(defaults),
    }
}

#[derive(Default)]
struct ReductionOptions {
    entries: Vec<Entry>,
    configuration_entries: Vec<Entry>,
    leaf_id: Option<String>,
    defaults: Option<EffectiveLaneConfiguration>,
}

fn expect_corruption(
    slice: &pillar_agent::harness::reducer::RecordLogSlice,
    reason: RecordLogCorruptionReason,
) {
    let error = validate_record_log(slice).expect_err(&format!("expected corruption {reason:?}"));
    assert_eq!(
        error.reason, reason,
        "expected {reason:?}, got {error:?} ({})",
        error.message
    );
}

fn assistant_tools_entry() -> Entry {
    let target = message_target(
        "assistant-tools",
        assistant_message(
            vec![tool_call_content("call-1", "tool-1")],
            StopReason::ToolUse,
        ),
    );
    persisted_entry(&target, 3, None)
}

struct CorruptionCase {
    name: &'static str,
    reason: RecordLogCorruptionReason,
    slice: pillar_agent::harness::reducer::RecordLogSlice,
}

fn corruption_cases() -> Vec<CorruptionCase> {
    vec![
        CorruptionCase {
            name: "multiple operations are open",
            reason: RecordLogCorruptionReason::MultipleOpenOperations,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    run_started(2, "run-2", Vec::new()),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "a record references an operation that does not exist",
            reason: RecordLogCorruptionReason::UnknownOperation,
            slice: recovery_slice(vec![abort_requested(1, "missing")], Vec::new()),
        },
        CorruptionCase {
            name: "a record follows its operation finish",
            reason: RecordLogCorruptionReason::RecordAfterFinish,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    operation_finished(2, "run-1"),
                    abort_requested(3, "run-1"),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "attempt numbers skip within one assistant step",
            reason: RecordLogCorruptionReason::NonConsecutiveAttempt,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    attempt(2, "run-1", "assistant", 1, "assistant-1", None),
                    attempt(3, "run-1", "assistant", 3, "assistant-2", None),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "a non-compaction attempt carries compactionReason",
            reason: RecordLogCorruptionReason::InvalidCompactionReason,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    record(
                        "step_attempt",
                        "attempt-2",
                        2,
                        RecordPayload::StepAttempt {
                            run_id: "run-1".to_owned(),
                            step: "assistant".to_owned(),
                            attempt: 1,
                            result_entry_id: "assistant-1".to_owned(),
                            // divergence: the port's reason enum is closed;
                            // the upstream "assistant + compactionReason"
                            // case is represented by a compaction reason on
                            // an assistant step, which the closed field
                            // cannot express, so the non-compaction guard
                            // is exercised via a compaction attempt that
                            // omits the reason below and this case asserts
                            // the reason-presence guard only.
                            compaction_reason: Some(CompactionReason::Manual),
                        },
                    ),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "a compaction attempt omits compactionReason",
            reason: RecordLogCorruptionReason::InvalidCompactionReason,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    attempt(2, "run-1", "compaction", 1, "compaction-1", None),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "steering is enqueued after abort",
            reason: RecordLogCorruptionReason::QueueAfterAbort,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    abort_requested(2, "run-1"),
                    queue_enqueued(
                        3,
                        message_target("queue-1", user_message("queued")),
                        "steer",
                    ),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "a queue cancellation has no enqueue",
            reason: RecordLogCorruptionReason::InvalidQueueCancellation,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    queue_cancelled(2, "queue-1", Some("run-1")),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "a queue cancellation targets an entry that exists",
            reason: RecordLogCorruptionReason::InvalidQueueCancellation,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    queue_enqueued(
                        2,
                        message_target("queue-1", user_message("queued")),
                        "steer",
                    ),
                    queue_cancelled(4, "queue-1", Some("run-1")),
                ],
                vec![persisted_entry(
                    &message_target("queue-1", user_message("queued")),
                    3,
                    None,
                )],
            ),
        },
        CorruptionCase {
            name: "structural attempts disagree on resultEntryId",
            reason: RecordLogCorruptionReason::InconsistentStep,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    attempt(
                        2,
                        "run-1",
                        "compaction",
                        1,
                        "compaction-1",
                        Some(CompactionReason::Threshold),
                    ),
                    attempt(
                        3,
                        "run-1",
                        "compaction",
                        2,
                        "compaction-2",
                        Some(CompactionReason::Threshold),
                    ),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "structural attempts disagree on compactionReason",
            reason: RecordLogCorruptionReason::InconsistentStep,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    attempt(
                        2,
                        "run-1",
                        "compaction",
                        1,
                        "compaction-1",
                        Some(CompactionReason::Threshold),
                    ),
                    attempt(
                        3,
                        "run-1",
                        "compaction",
                        2,
                        "compaction-1",
                        Some(CompactionReason::Overflow),
                    ),
                ],
                Vec::new(),
            ),
        },
        CorruptionCase {
            name: "tool_started does not match the assistant tool call",
            reason: RecordLogCorruptionReason::ToolCallMismatch,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    tool_started(
                        4,
                        ToolStartOverrides {
                            tool_call_id: Some("different-call".to_owned()),
                            ..Default::default()
                        },
                    ),
                ],
                vec![assistant_tools_entry()],
            ),
        },
        CorruptionCase {
            name: "two tool_started records share an invocation identity",
            reason: RecordLogCorruptionReason::DuplicateToolInvocation,
            slice: recovery_slice(
                vec![
                    run_started(1, "run-1", Vec::new()),
                    tool_started(4, ToolStartOverrides::default()),
                    tool_started(
                        5,
                        ToolStartOverrides {
                            id: Some("tool-start-duplicate".to_owned()),
                            result_entry_id: Some("tool-result-2".to_owned()),
                            ..Default::default()
                        },
                    ),
                ],
                vec![assistant_tools_entry()],
            ),
        },
        CorruptionCase {
            name: "a provisioned id exists with different content",
            reason: RecordLogCorruptionReason::ProvisionedEntryMismatch,
            slice: recovery_slice(
                vec![run_started(
                    1,
                    "run-1",
                    vec![message_target("prompt-1", user_message("expected"))],
                )],
                vec![persisted_entry(
                    &message_target("prompt-1", user_message("different")),
                    2,
                    None,
                )],
            ),
        },
        CorruptionCase {
            name: "a deferred assistant message has no handle",
            reason: RecordLogCorruptionReason::InvalidDeferredHandle,
            slice: recovery_slice(
                vec![run_started(1, "run-1", Vec::new())],
                vec![persisted_entry(
                    // Upstream spreads `deferred: undefined` onto the
                    // deferred assistant message; the port builds the
                    // message without the handle directly.
                    &provisioned_from_payload(
                        "assistant-deferred",
                        EntryPayload::Message {
                            message: assistant_message_no_handle(),
                            terminate: false,
                        },
                    ),
                    2,
                    None,
                )],
            ),
        },
    ]
}

#[test]
fn rejects_each_corruption_case() {
    for case in corruption_cases() {
        expect_corruption(&case.slice, case.reason);
    }
}

#[test]
fn names_each_corruption_case() {
    // Keep case names in sync with upstream for review parity.
    let names: Vec<&str> = corruption_cases()
        .into_iter()
        .map(|case| case.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "multiple operations are open",
            "a record references an operation that does not exist",
            "a record follows its operation finish",
            "attempt numbers skip within one assistant step",
            "a non-compaction attempt carries compactionReason",
            "a compaction attempt omits compactionReason",
            "steering is enqueued after abort",
            "a queue cancellation has no enqueue",
            "a queue cancellation targets an entry that exists",
            "structural attempts disagree on resultEntryId",
            "structural attempts disagree on compactionReason",
            "tool_started does not match the assistant tool call",
            "two tool_started records share an invocation identity",
            "a provisioned id exists with different content",
            "a deferred assistant message has no handle",
        ]
    );
}

#[test]
fn does_not_mutate_its_bounded_recovery_inputs() {
    let target = message_target("prompt-1", user_message("hello"));
    let start = run_started(1, "run-1", vec![target.clone()]);
    let entry = persisted_entry(&target, 2, None);
    let slice = recovery_slice(vec![start.clone()], vec![entry.clone()]);

    validate_record_log(&slice).expect("slice must be valid");
    assert_eq!(slice.records, vec![start]);
    assert_eq!(slice.entries, vec![entry]);
}

/// conformance: every prefix of each accepted trace must validate.
struct TraceBuilder {
    records: Vec<LaneRecord>,
    entries: Vec<Entry>,
}

enum DurableAction {
    Record(LaneRecord),
    Entry(Entry),
}

fn valid_prefixes(
    trace: &str,
    actions: Vec<DurableAction>,
) -> Vec<(String, pillar_agent::harness::reducer::RecordLogSlice)> {
    actions
        .iter()
        .scan(
            TraceBuilder {
                records: Vec::new(),
                entries: Vec::new(),
            },
            |builder, action| {
                match action {
                    DurableAction::Record(record) => builder.records.push(record.clone()),
                    DurableAction::Entry(entry) => builder.entries.push(entry.clone()),
                }
                Some((
                    format!(
                        "{trace} after action {}",
                        builder.records.len() + builder.entries.len()
                    ),
                    recovery_slice(builder.records.clone(), builder.entries.clone()),
                ))
            },
        )
        .collect()
}

fn prompt_target() -> ProvisionedEntry {
    message_target("prompt-1", user_message("fix the bug"))
}

fn assistant_tool_target() -> ProvisionedEntry {
    message_target(
        "assistant-tools",
        assistant_message(
            vec![tool_call_content("call-1", "tool-1")],
            StopReason::ToolUse,
        ),
    )
}

fn tool_result_target() -> ProvisionedEntry {
    message_target("tool-result-1", tool_result_message("call-1", "tool-1"))
}

fn assistant_final_target() -> ProvisionedEntry {
    message_target(
        "assistant-final",
        assistant_message(vec![text_content("done")], StopReason::Stop),
    )
}

fn valid_prefix_cases() -> Vec<(String, pillar_agent::harness::reducer::RecordLogSlice)> {
    let mut cases = Vec::new();
    cases.extend(valid_prefixes(
        "one-tool run X1-X5",
        vec![
            DurableAction::Record(run_started(1, "run-1", vec![prompt_target()])),
            DurableAction::Entry(persisted_entry(&prompt_target(), 2, None)),
            DurableAction::Record(attempt(3, "run-1", "assistant", 1, "assistant-tools", None)),
            DurableAction::Entry(persisted_entry(
                &assistant_tool_target(),
                4,
                Some("prompt-1"),
            )),
            DurableAction::Record(tool_started(5, ToolStartOverrides::default())),
            DurableAction::Entry(persisted_entry(
                &tool_result_target(),
                6,
                Some("assistant-tools"),
            )),
            DurableAction::Record(attempt(7, "run-1", "assistant", 1, "assistant-final", None)),
            DurableAction::Entry(persisted_entry(
                &assistant_final_target(),
                8,
                Some("tool-result-1"),
            )),
            DurableAction::Record(operation_finished(9, "run-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "assistant retry",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "assistant",
                1,
                "assistant-attempt-1",
                None,
            )),
            DurableAction::Record(usage_record(
                3,
                "assistant-attempt-1",
                "assistant",
                Some(StopReason::Error),
            )),
            DurableAction::Record(attempt(
                4,
                "run-1",
                "assistant",
                2,
                "assistant-attempt-2",
                None,
            )),
            DurableAction::Record(usage_record(
                5,
                "assistant-attempt-2",
                "assistant",
                Some(StopReason::Stop),
            )),
            DurableAction::Entry(persisted_message(
                "assistant-attempt-2",
                assistant_message(vec![text_content("ok")], StopReason::Stop),
                6,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "terminal assistant failure",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(2, "run-1", "assistant", 1, "assistant-error", None)),
            DurableAction::Entry(persisted_message(
                "assistant-error",
                assistant_message(Vec::new(), StopReason::Error),
                3,
                None,
            )),
            DurableAction::Record(operation_finished(4, "run-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "overflow compaction and retry",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "assistant",
                1,
                "discarded-overflow",
                None,
            )),
            DurableAction::Record(usage_record(
                3,
                "discarded-overflow",
                "assistant",
                Some(StopReason::Length),
            )),
            DurableAction::Record(attempt(
                4,
                "run-1",
                "compaction",
                1,
                "overflow-compaction",
                Some(CompactionReason::Overflow),
            )),
            DurableAction::Entry(compaction_entry("overflow-compaction", 5)),
            DurableAction::Record(attempt(
                6,
                "run-1",
                "assistant",
                1,
                "assistant-after-compaction",
                None,
            )),
            DurableAction::Entry(persisted_message(
                "assistant-after-compaction",
                assistant_message(vec![text_content("fits")], StopReason::Stop),
                7,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "steering acceptance and consumption",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(queue_enqueued(
                2,
                message_target("queue-1", user_message("queued")),
                "steer",
            )),
            DurableAction::Entry(persisted_entry(
                &message_target("queue-1", user_message("queued")),
                3,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "queue cancellation",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(queue_enqueued(
                2,
                message_target("queue-1", user_message("queued")),
                "steer",
            )),
            DurableAction::Record(queue_cancelled(3, "queue-1", Some("run-1"))),
        ],
    ));
    cases.extend(valid_prefixes(
        "deferred write acceptance and application",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(write_deferred(
                2,
                message_target("write-1", user_message("deferred write")),
            )),
            DurableAction::Entry(persisted_entry(
                &message_target("write-1", user_message("deferred write")),
                3,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "abort during a tool",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(2, "run-1", "assistant", 1, "assistant-tools", None)),
            DurableAction::Entry(persisted_entry(&assistant_tool_target(), 3, None)),
            DurableAction::Record(tool_started(4, ToolStartOverrides::default())),
            DurableAction::Record(abort_requested(5, "run-1")),
            DurableAction::Entry(persisted_message(
                "tool-result-1",
                tool_result_message("call-1", "tool-1"),
                6,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "threshold auto-compaction",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "compaction",
                1,
                "threshold-compaction",
                Some(CompactionReason::Threshold),
            )),
            DurableAction::Entry(compaction_entry("threshold-compaction", 3)),
            DurableAction::Record(attempt(
                4,
                "run-1",
                "assistant",
                1,
                "assistant-after-threshold",
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "manual compaction",
        vec![
            DurableAction::Record(compaction_started(1, "compaction-1")),
            DurableAction::Record(attempt(
                2,
                "compact-1",
                "compaction",
                1,
                "compaction-1",
                Some(CompactionReason::Manual),
            )),
            DurableAction::Entry(compaction_entry("compaction-1", 3)),
            DurableAction::Record(operation_finished(4, "compact-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "move-first navigation summary",
        vec![
            DurableAction::Record(navigation_started(1, "summary-1")),
            DurableAction::Record(attempt(
                2,
                "navigate-1",
                "branch_summary",
                1,
                "summary-1",
                None,
            )),
            DurableAction::Entry(branch_summary_entry("summary-1", 3)),
            DurableAction::Record(operation_finished(4, "navigate-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "blocked tool without an intent record",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(2, "run-1", "assistant", 1, "assistant-tools", None)),
            DurableAction::Entry(persisted_entry(&assistant_tool_target(), 3, None)),
            DurableAction::Entry(persisted_message(
                "blocked-result",
                tool_result_message("call-1", "tool-1"),
                4,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "idle next-run cancellation",
        vec![
            DurableAction::Record(queue_enqueued(
                1,
                message_target("next-1", user_message("later")),
                "nextRun",
            )),
            DurableAction::Record(queue_cancelled(2, "next-1", None)),
        ],
    ));
    cases.extend(valid_prefixes(
        "next-run enqueue after abort",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(abort_requested(2, "run-1")),
            DurableAction::Record(queue_enqueued(
                3,
                message_target("next-1", user_message("later")),
                "nextRun",
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "deferred write applied during abort reconciliation",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(write_deferred(
                2,
                message_target("write-1", user_message("deferred write")),
            )),
            DurableAction::Record(abort_requested(3, "run-1")),
            DurableAction::Entry(persisted_entry(
                &message_target("write-1", user_message("deferred write")),
                4,
                None,
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "accepted steering killed by abort",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(queue_enqueued(
                2,
                message_target("queue-1", user_message("queued")),
                "steer",
            )),
            DurableAction::Record(abort_requested(3, "run-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "compaction retry",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "compaction",
                1,
                "threshold-compaction",
                Some(CompactionReason::Threshold),
            )),
            DurableAction::Record(attempt(
                3,
                "run-1",
                "compaction",
                2,
                "threshold-compaction",
                Some(CompactionReason::Threshold),
            )),
            DurableAction::Entry(compaction_entry("threshold-compaction", 4)),
        ],
    ));
    cases.extend(valid_prefixes(
        "hook-supplied manual compaction",
        vec![
            DurableAction::Record(compaction_started(1, "compaction-1")),
            DurableAction::Entry(compaction_entry("compaction-1", 2)),
            DurableAction::Record(operation_finished(3, "compact-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "hook-supplied navigation summary",
        vec![
            DurableAction::Record(navigation_started(1, "summary-1")),
            DurableAction::Entry(branch_summary_entry("summary-1", 2)),
            DurableAction::Record(operation_finished(3, "navigate-1")),
        ],
    ));
    cases.extend(valid_prefixes(
        "deferred provider suspension and redemption",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "assistant",
                1,
                "assistant-deferred",
                None,
            )),
            DurableAction::Entry(persisted_entry(
                &message_target(
                    "assistant-deferred",
                    assistant_message(Vec::new(), StopReason::Deferred),
                ),
                3,
                None,
            )),
            DurableAction::Entry(persisted_message(
                "assistant-ready",
                assistant_message(vec![text_content("ready")], StopReason::Stop),
                4,
                Some("assistant-deferred"),
            )),
        ],
    ));
    cases.extend(valid_prefixes(
        "abort of a deferred provider request",
        vec![
            DurableAction::Record(run_started(1, "run-1", Vec::new())),
            DurableAction::Record(attempt(
                2,
                "run-1",
                "assistant",
                1,
                "assistant-deferred",
                None,
            )),
            DurableAction::Entry(persisted_entry(
                &message_target(
                    "assistant-deferred",
                    assistant_message(Vec::new(), StopReason::Deferred),
                ),
                3,
                None,
            )),
            DurableAction::Record(abort_requested(4, "run-1")),
        ],
    ));
    cases
}

#[test]
fn accepts_valid_durable_prefixes() {
    for (name, slice) in valid_prefix_cases() {
        let result = validate_record_log(&slice);
        assert!(result.is_ok(), "{name}: expected valid, got {result:?}");
    }
}

#[test]
fn reduces_an_idle_lane_to_pending_next_run_input_and_default_configuration() {
    let pending = message_target("next-pending", user_message("pending"));
    let cancelled = message_target("next-cancelled", user_message("cancelled"));
    let consumed = message_target("next-consumed", user_message("consumed"));
    let input = reduction_input(
        vec![
            queue_enqueued(1, pending.clone(), "nextRun"),
            queue_enqueued(2, cancelled.clone(), "nextRun"),
            queue_cancelled(3, "next-cancelled", None),
            queue_enqueued(4, consumed.clone(), "nextRun"),
        ],
        Vec::new(),
        ReductionOptions {
            entries: vec![persisted_entry(&consumed, 5, None)],
            leaf_id: Some("idle-leaf".to_owned()),
            ..Default::default()
        },
    );

    let result = reduce_lane_state(&input).expect("valid input");
    assert_eq!(
        result.lane_state,
        LaneState {
            lane: "main".to_owned(),
            leaf_id: Some("idle-leaf".to_owned()),
            operation: None,
            pending_next_run: vec![pending],
        }
    );
    assert_eq!(result.effective_configuration, defaults());
    assert_eq!(result.terminal_failure, None);
}

#[test]
fn folds_persisted_configuration_over_copied_defaults_in_sequence() {
    let configuration_entries = vec![
        Entry {
            kind: "model_change".to_owned(),
            id: "model-change".to_owned(),
            seq: 1,
            parent_id: None,
            timestamp: 1,
            payload: EntryPayload::ModelChange {
                provider: "persisted-provider".to_owned(),
                model_id: "persisted-model".to_owned(),
            },
        },
        Entry {
            kind: "thinking_level_change".to_owned(),
            id: "thinking-change".to_owned(),
            seq: 2,
            parent_id: Some("model-change".to_owned()),
            timestamp: 2,
            payload: EntryPayload::ThinkingLevelChange {
                thinking_level: "high".to_owned(),
            },
        },
        Entry {
            kind: "active_tools_change".to_owned(),
            id: "tools-change".to_owned(),
            seq: 3,
            parent_id: Some("thinking-change".to_owned()),
            timestamp: 3,
            payload: EntryPayload::ActiveToolsChange {
                active_tool_names: vec!["persisted-tool".to_owned()],
            },
        },
    ];
    let input = reduction_input(
        Vec::new(),
        Vec::new(),
        ReductionOptions {
            configuration_entries,
            ..Default::default()
        },
    );

    let result = reduce_lane_state(&input).expect("valid input");
    assert_eq!(
        result.effective_configuration,
        EffectiveLaneConfiguration {
            model: LaneModelRef {
                provider: "persisted-provider".to_owned(),
                model_id: "persisted-model".to_owned(),
            },
            thinking_level: pillar_agent::types::thinking::AgentThinkingLevel::High,
            active_tool_names: vec!["persisted-tool".to_owned()],
        }
    );
    assert_eq!(input.defaults, defaults());
}

#[test]
fn applies_committed_operation_owned_configuration_after_the_anchor() {
    let assistant = persisted_message(
        "assistant-config",
        assistant_message(vec![text_content("response")], StopReason::Stop),
        2,
        None,
    );
    // Override provider/model on the assistant entry to match upstream.
    let assistant = {
        let mut message = assistant.clone();
        if let EntryPayload::Message {
            message: AgentMessage::Message(Message::Assistant(a)),
            ..
        } = &mut message.payload
        {
            a.provider = "response-provider".to_owned();
            a.model = "response-model".to_owned();
        }
        message
    };
    let tools = Entry {
        kind: "active_tools_change".to_owned(),
        id: "operation-tools".to_owned(),
        seq: 3,
        parent_id: Some("assistant-config".to_owned()),
        timestamp: 3,
        payload: EntryPayload::ActiveToolsChange {
            active_tool_names: vec!["operation-tool".to_owned()],
        },
    };
    let result = reduce_lane_state(&reduction_input(
        vec![run_started(1, "run-1", Vec::new())],
        vec![assistant, tools],
        ReductionOptions::default(),
    ))
    .expect("valid input");

    assert_eq!(
        result.effective_configuration,
        EffectiveLaneConfiguration {
            model: LaneModelRef {
                provider: "response-provider".to_owned(),
                model_id: "response-model".to_owned(),
            },
            thinking_level: pillar_agent::types::thinking::AgentThinkingLevel::Off,
            active_tool_names: vec!["operation-tool".to_owned()],
        }
    );
}

#[test]
fn keeps_captured_next_run_input_with_the_open_run_instead_of_pending_next_run() {
    let captured = message_target("next-captured", user_message("captured"));
    let later = message_target("next-later", user_message("later"));
    let start = run_started(2, "run-1", vec![captured.clone()]);

    let result = reduce_lane_state(&reduction_input(
        vec![
            queue_enqueued(1, captured.clone(), "nextRun"),
            start,
            queue_enqueued(3, later.clone(), "nextRun"),
        ],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");

    assert_eq!(result.lane_state.pending_next_run, vec![later]);
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert_eq!(operation.missing_initial_messages, vec![captured]);
}

#[test]
fn derives_missing_input_queues_deferred_writes_and_the_unfinished_attempt() {
    let missing_prompt = message_target("prompt-missing", user_message("missing"));
    let committed_prompt = message_target("prompt-committed", user_message("committed"));
    let steer = message_target("steer-pending", user_message("steer"));
    let consumed_follow_up = message_target("follow-consumed", user_message("follow"));
    let next_run = message_target("next-run", user_message("next"));
    let pending_write = message_target("write-pending", user_message("write"));
    let applied_write = message_target("write-applied", user_message("applied"));
    let start = run_started(
        1,
        "run-1",
        vec![missing_prompt.clone(), committed_prompt.clone()],
    );
    let committed_prompt_entry = persisted_entry(&committed_prompt, 2, None);
    let consumed_follow_up_entry =
        persisted_entry(&consumed_follow_up, 6, Some("prompt-committed"));
    let applied_write_entry = persisted_entry(&applied_write, 9, Some("follow-consumed"));
    let input = reduction_input(
        vec![
            start,
            queue_enqueued(3, steer.clone(), "steer"),
            queue_enqueued(4, consumed_follow_up.clone(), "followUp"),
            queue_enqueued(5, next_run.clone(), "nextRun"),
            write_deferred(7, pending_write.clone()),
            write_deferred(8, applied_write.clone()),
            attempt(10, "run-1", "assistant", 1, "assistant-pending", None),
        ],
        vec![
            committed_prompt_entry,
            consumed_follow_up_entry,
            applied_write_entry,
        ],
        ReductionOptions::default(),
    );

    let result = reduce_lane_state(&input).expect("valid input");
    assert_eq!(result.lane_state.pending_next_run, vec![next_run]);
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert_eq!(operation.id, "run-1");
    assert!(!operation.aborting);
    assert_eq!(operation.missing_initial_messages, vec![missing_prompt]);
    assert_eq!(operation.pending_steer, vec![steer]);
    assert!(operation.pending_follow_up.is_empty());
    assert_eq!(operation.pending_writes, vec![pending_write]);
    let step = operation.step.as_ref().expect("unfinished attempt");
    assert_eq!(step.kind, "assistant");
    assert_eq!(step.attempts, 1);
    assert_eq!(step.result_entry_id, "assistant-pending");
    let newest_own = operation.newest_own.as_ref().expect("own entries");
    assert_eq!(newest_own.entry_id, "write-applied");
    assert_eq!(newest_own.role.as_deref(), Some("user"));
}

#[test]
fn kills_steer_and_follow_up_queues_on_abort_while_preserving_writes_and_next_run_input() {
    let steer = message_target("steer-aborted", user_message("steer"));
    let follow_up = message_target("follow-aborted", user_message("follow"));
    let next_run = message_target("next-after-abort", user_message("next"));
    let pending_write = message_target("write-after-abort", user_message("write"));
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            queue_enqueued(2, steer, "steer"),
            queue_enqueued(3, follow_up, "followUp"),
            queue_enqueued(4, next_run.clone(), "nextRun"),
            write_deferred(5, pending_write.clone()),
            abort_requested(6, "run-1"),
        ],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");

    assert_eq!(result.lane_state.pending_next_run, vec![next_run]);
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert!(operation.aborting);
    assert!(operation.pending_steer.is_empty());
    assert!(operation.pending_follow_up.is_empty());
    assert_eq!(operation.pending_writes, vec![pending_write]);
}

#[test]
fn reduces_an_unfinished_assistant_step() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "result", None),
        ],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert_eq!(
        operation.step,
        Some(OperationStep {
            kind: "assistant".to_owned(),
            attempts: 1,
            result_entry_id: "result".to_owned(),
            compaction_reason: None,
        })
    );
}

#[test]
fn reduces_an_unfinished_compaction_step() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(
                2,
                "run-1",
                "compaction",
                1,
                "result",
                Some(CompactionReason::Overflow),
            ),
        ],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert_eq!(
        operation.step,
        Some(OperationStep {
            kind: "compaction".to_owned(),
            attempts: 1,
            result_entry_id: "result".to_owned(),
            compaction_reason: Some(CompactionReason::Overflow),
        })
    );
}

#[test]
fn reduces_an_unfinished_branch_summary_step() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "branch_summary", 1, "result", None),
        ],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let operation = result.lane_state.operation.as_ref().expect("open run");
    assert_eq!(
        operation.step,
        Some(OperationStep {
            kind: "branch_summary".to_owned(),
            attempts: 1,
            result_entry_id: "result".to_owned(),
            compaction_reason: None,
        })
    );
}

#[test]
fn closes_the_newest_attempt_only_when_its_provisioned_result_exists() {
    let target = message_target(
        "result",
        assistant_message(vec![text_content("done")], StopReason::Stop),
    );
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "result", None),
        ],
        vec![persisted_entry(&target, 3, None)],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(
        result
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .step
            .is_none()
    );
}

#[test]
fn ignores_unfulfilled_result_ids_from_earlier_attempts() {
    let target = message_target(
        "attempt-2-result",
        assistant_message(vec![text_content("done")], StopReason::Stop),
    );
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "attempt-1-result", None),
            attempt(3, "run-1", "assistant", 2, "attempt-2-result", None),
        ],
        vec![persisted_entry(&target, 4, None)],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(
        result
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .step
            .is_none()
    );
}

struct ToolBatchCase {
    name: &'static str,
    records: Vec<LaneRecord>,
    result_entry: Option<Entry>,
}

fn tool_batch_cases() -> Vec<ToolBatchCase> {
    vec![
        ToolBatchCase {
            name: "X1",
            records: vec![
                run_started(1, "run-1", Vec::new()),
                attempt(2, "run-1", "assistant", 1, "assistant-tools", None),
            ],
            result_entry: None,
        },
        ToolBatchCase {
            name: "X3",
            records: vec![
                run_started(1, "run-1", Vec::new()),
                attempt(2, "run-1", "assistant", 1, "assistant-tools", None),
                tool_started(4, ToolStartOverrides::default()),
            ],
            result_entry: None,
        },
        ToolBatchCase {
            name: "X5",
            records: vec![
                run_started(1, "run-1", Vec::new()),
                attempt(2, "run-1", "assistant", 1, "assistant-tools", None),
                tool_started(4, ToolStartOverrides::default()),
            ],
            result_entry: Some({
                let mut entry = persisted_entry(&tool_result_target(), 5, Some("assistant-tools"));
                if let EntryPayload::Message { terminate, .. } = &mut entry.payload {
                    *terminate = true;
                }
                entry
            }),
        },
    ]
}

#[test]
fn reduces_tool_batch_state_at_each_stage() {
    for case in tool_batch_cases() {
        let mut own_entries = vec![assistant_tools_entry()];
        if let Some(result_entry) = case.result_entry {
            own_entries.push(result_entry);
        }
        let reduction = reduce_lane_state(&reduction_input(
            case.records,
            own_entries,
            ReductionOptions::default(),
        ))
        .expect("valid input");
        let operation = reduction.lane_state.operation.as_ref().expect("open run");
        let batch = operation.tool_batch.as_ref().expect("tool batch");
        assert_eq!(batch.assistant_entry_id, "assistant-tools", "{}", case.name);
        assert!(!batch.truncated, "{}", case.name);
        let has_result = case.name == "X5";
        assert_eq!(batch.unresolved, !has_result, "{}", case.name);
        assert_eq!(batch.calls.len(), 1, "{}", case.name);
        let call = &batch.calls[0];
        assert_eq!(call.tool_index, 0, "{}", case.name);
        assert_eq!(call.tool_call.id, "call-1", "{}", case.name);
        assert_eq!(call.tool_call.name, "tool-1", "{}", case.name);
        assert_eq!(call.result_exists, has_result, "{}", case.name);
        assert_eq!(call.terminate, has_result, "{}", case.name);
        assert_eq!(call.started.is_some(), case.name != "X1", "{}", case.name);
    }
}

#[test]
fn does_not_resolve_a_tool_batch_from_a_deferred_write_tool_result() {
    let assistant = persisted_entry(&assistant_tool_target(), 3, None);
    let written_result = message_target(
        "written-tool-result",
        tool_result_message("call-1", "tool-1"),
    );
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-tools", None),
            write_deferred(4, written_result.clone()),
        ],
        vec![
            assistant,
            persisted_entry(&written_result, 5, Some("assistant-tools")),
        ],
        ReductionOptions::default(),
    ))
    .expect("valid input");

    let operation = result.lane_state.operation.as_ref().expect("open run");
    let batch = operation.tool_batch.as_ref().expect("tool batch");
    assert!(!batch.calls[0].result_exists);
    assert!(batch.unresolved);
}

#[test]
fn matches_blocked_results_without_tool_start_records_and_preserves_source_order() {
    let assistant_target = message_target(
        "assistant-two-tools",
        assistant_message(
            vec![
                tool_call_content("call-1", "tool-1"),
                tool_call_content("call-2", "tool-2"),
            ],
            StopReason::ToolUse,
        ),
    );
    let assistant = persisted_entry(&assistant_target, 3, None);
    let mut blocked = provisioned_from_payload(
        "blocked-result",
        EntryPayload::Message {
            message: tool_result_message("call-1", "tool-1"),
            terminate: false,
        },
    );
    if let EntryPayload::Message {
        message: AgentMessage::Message(Message::ToolResult(result)),
        ..
    } = &mut blocked.payload
    {
        result.content = vec![Content::text("blocked")];
        result.is_error = true;
    }
    let blocked = persisted_entry(&blocked, 4, Some("assistant-two-tools"));
    let second_start = tool_started(
        5,
        ToolStartOverrides {
            assistant_entry_id: Some("assistant-two-tools".to_owned()),
            tool_index: Some(1),
            tool_call_id: Some("call-2".to_owned()),
            tool_name: Some("tool-2".to_owned()),
            result_entry_id: Some("call-2-result".to_owned()),
            ..Default::default()
        },
    );
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-two-tools", None),
            second_start,
        ],
        vec![assistant, blocked],
        ReductionOptions::default(),
    ))
    .expect("valid input");

    let operation = result.lane_state.operation.as_ref().expect("open run");
    let batch = operation.tool_batch.as_ref().expect("tool batch");
    assert_eq!(batch.calls.len(), 2);
    assert_eq!(batch.calls[0].tool_index, 0);
    assert_eq!(batch.calls[0].tool_call.id, "call-1");
    assert!(batch.calls[0].result_exists);
    assert_eq!(batch.calls[1].tool_index, 1);
    assert_eq!(batch.calls[1].tool_call.id, "call-2");
    assert!(batch.calls[1].started.is_some());
    assert!(!batch.calls[1].result_exists);
}

#[test]
fn marks_a_length_stopped_tool_batch_as_truncated_without_resolving_it() {
    let truncated = persisted_entry(
        &message_target(
            "assistant-truncated",
            assistant_message(
                vec![tool_call_content("call-1", "tool-1")],
                StopReason::Length,
            ),
        ),
        3,
        None,
    );
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-truncated", None),
        ],
        vec![truncated],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let operation = result.lane_state.operation.as_ref().expect("open run");
    let batch = operation.tool_batch.as_ref().expect("tool batch");
    assert!(batch.truncated);
    assert!(batch.unresolved);
}

#[test]
fn detects_an_unredeemed_deferred_handle_only_at_the_operation_tail() {
    let deferred_entry = persisted_entry(
        &message_target(
            "assistant-deferred",
            assistant_message(Vec::new(), StopReason::Deferred),
        ),
        3,
        None,
    );
    let pending = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-deferred", None),
        ],
        vec![deferred_entry.clone()],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let pending_operation = pending.lane_state.operation.as_ref().expect("open run");
    let expected_handle = match as_assistant_of_entry(&deferred_entry) {
        Some(handle) => handle,
        None => panic!("deferred entry must be an assistant"),
    };
    assert_eq!(pending_operation.deferred, Some(expected_handle));

    let successor = persisted_message(
        "assistant-ready",
        assistant_message(vec![text_content("ready")], StopReason::Stop),
        4,
        Some("assistant-deferred"),
    );
    let redeemed = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-deferred", None),
        ],
        vec![deferred_entry, successor],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(
        redeemed
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .deferred
            .is_none()
    );
}

fn as_assistant_of_entry(entry: &Entry) -> Option<pillar_ai::types::DeferredHandle> {
    match &entry.payload {
        EntryPayload::Message { message, .. } => match message.as_message() {
            Some(Message::Assistant(assistant)) => assistant.deferred.clone(),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn derives_step_terminal_failure_provenance() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-error", None),
        ],
        vec![persisted_message(
            "assistant-error",
            assistant_message(Vec::new(), StopReason::Error),
            3,
            None,
        )],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let failure = result.terminal_failure.expect("terminal failure");
    assert_eq!(failure.source, TerminalFailureSource::Step);
}

#[test]
fn derives_deferred_fetch_terminal_failure_provenance() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            attempt(2, "run-1", "assistant", 1, "assistant-deferred", None),
        ],
        vec![
            persisted_entry(
                &message_target(
                    "assistant-deferred",
                    assistant_message(Vec::new(), StopReason::Deferred),
                ),
                3,
                None,
            ),
            persisted_message(
                "deferred-error",
                assistant_message(Vec::new(), StopReason::Error),
                4,
                Some("assistant-deferred"),
            ),
        ],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let failure = result.terminal_failure.expect("terminal failure");
    assert_eq!(failure.source, TerminalFailureSource::DeferredFetch);
}

#[test]
fn derives_deferred_fetch_usage_record_terminal_failure_provenance() {
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            usage_record(
                3,
                "deferred-error",
                "deferred_fetch",
                Some(StopReason::Error),
            ),
        ],
        vec![persisted_message(
            "deferred-error",
            assistant_message(Vec::new(), StopReason::Error),
            2,
            None,
        )],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    let failure = result.terminal_failure.expect("terminal failure");
    assert_eq!(failure.source, TerminalFailureSource::DeferredFetch);
}

#[test]
fn does_not_classify_an_error_shaped_deferred_write_as_terminal_failure() {
    let target = message_target(
        "written-error",
        assistant_message(Vec::new(), StopReason::Error),
    );
    let entry = persisted_entry(&target, 3, None);
    let result = reduce_lane_state(&reduction_input(
        vec![
            run_started(1, "run-1", Vec::new()),
            write_deferred(2, target),
        ],
        vec![entry],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(result.terminal_failure.is_none());
}

#[test]
fn derives_structural_target_state_for_manual_compaction() {
    let missing = reduce_lane_state(&reduction_input(
        vec![compaction_started(1, "compaction-1")],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert_eq!(
        missing
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .targets,
        OperationTargets {
            result: Some(false),
            summary: None,
        }
    );

    let completed = reduce_lane_state(&reduction_input(
        vec![compaction_started(1, "compaction-1")],
        Vec::new(),
        ReductionOptions {
            entries: vec![compaction_entry("compaction-1", 2)],
            ..Default::default()
        },
    ))
    .expect("valid input");
    assert_eq!(
        completed
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .targets,
        OperationTargets {
            result: Some(true),
            summary: None,
        }
    );
}

#[test]
fn derives_structural_target_state_for_navigation_summary() {
    let missing = reduce_lane_state(&reduction_input(
        vec![navigation_started(1, "summary-1")],
        Vec::new(),
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert_eq!(
        missing
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .targets,
        OperationTargets {
            result: None,
            summary: Some(false),
        }
    );

    let present = reduce_lane_state(&reduction_input(
        vec![navigation_started(1, "summary-1")],
        Vec::new(),
        ReductionOptions {
            entries: vec![branch_summary_entry("summary-1", 2)],
            ..Default::default()
        },
    ))
    .expect("valid input");
    assert_eq!(
        present
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .targets,
        OperationTargets {
            result: None,
            summary: Some(true),
        }
    );
}

#[test]
fn resets_the_overflow_guard_only_after_newer_conversational_input_is_consumed() {
    let initial = message_target("initial", user_message("initial"));
    let steer = message_target("steer", user_message("steer"));
    let start = run_started(1, "run-1", vec![initial.clone()]);
    let initial_entry = persisted_entry(&initial, 2, None);
    let records = vec![
        start,
        attempt(
            3,
            "run-1",
            "compaction",
            1,
            "overflow-summary",
            Some(CompactionReason::Overflow),
        ),
        queue_enqueued(5, steer.clone(), "steer"),
    ];

    let used = reduce_lane_state(&reduction_input(
        records.clone(),
        vec![initial_entry.clone()],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(
        used.lane_state
            .operation
            .as_ref()
            .expect("open run")
            .overflow_recovery_used
    );

    let reset = reduce_lane_state(&reduction_input(
        records,
        vec![initial_entry, persisted_entry(&steer, 6, Some("initial"))],
        ReductionOptions::default(),
    ))
    .expect("valid input");
    assert!(
        !reset
            .lane_state
            .operation
            .as_ref()
            .expect("open run")
            .overflow_recovery_used
    );
}

#[test]
fn is_deterministic_and_does_not_mutate_or_alias_its_inputs() {
    let pending = message_target("next", user_message("next"));
    let input = reduction_input(
        vec![queue_enqueued(1, pending.clone(), "nextRun")],
        Vec::new(),
        ReductionOptions::default(),
    );
    let before = input.clone();
    let first: LaneReductionResult = reduce_lane_state(&input).expect("valid input");
    let second = reduce_lane_state(&input).expect("valid input");

    assert_eq!(first, second);
    assert_eq!(input, before);
    // The output owns its data: mutating it cannot touch the input.
    let mut output = first;
    output.lane_state.pending_next_run[0].id = "mutated-output".to_owned();
    assert_eq!(input, before);
}
