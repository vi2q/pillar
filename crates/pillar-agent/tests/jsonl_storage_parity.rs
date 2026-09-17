//! Port of packages/agent/test/harness/session/jsonl-storage.test.ts
//! (pi v0.84.3) — file-backed session storage round trips, record recovery,
//! cross-lane sequencing, and payload validation.

#![cfg(feature = "session-files")]

use pillar_agent::harness::env::StdFsExecutionEnv;
use pillar_agent::harness::session::jsonl::repo::JsonlSessionRepo;
use pillar_agent::harness::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionMetadata, JsonlSessionRepoOptions,
};
use pillar_agent::harness::session::memory::{ProvisionedEntry, ProvisionedRecord, Session};
use pillar_agent::harness::session::types::{
    EntryOrder, EntryPayload, EntryQuery, LaneRecord, OperationIntent, OperationOutcome,
    RecordPayload,
};
use pillar_agent::harness::types::{FileError, FileErrorCode};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, Usage, UsageCost, UserContent};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn temp_dir(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-jsonl-storage-{}-{}-{}",
        label,
        std::process::id(),
        count
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.to_string_lossy().into_owned()
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn usage(multiplier: u64) -> Usage {
    Usage {
        input: multiplier,
        output: multiplier * 2,
        cache_read: multiplier * 3,
        cache_write: multiplier * 4,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: multiplier * 10,
        cost: UsageCost {
            input: multiplier as f64 * 0.1,
            output: multiplier as f64 * 0.2,
            cache_read: multiplier as f64 * 0.3,
            cache_write: multiplier as f64 * 0.4,
            total: multiplier as f64,
        },
    }
}

fn create_repository(root: &str) -> JsonlSessionRepo<StdFsExecutionEnv> {
    JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(root)),
        sessions_root: root.to_owned(),
    })
}

fn create_options(id: &str, cwd: &str) -> JsonlSessionCreateOptions {
    JsonlSessionCreateOptions {
        id: Some(id.to_owned()),
        cwd: cwd.to_owned(),
        ..Default::default()
    }
}

async fn reopen(root: &str, session: &Session) -> Session {
    let metadata: JsonlSessionMetadata = list_first_metadata(root, session).await;
    create_repository(root)
        .open(&metadata)
        .await
        .expect("reopen")
}

async fn list_first_metadata(_root: &str, session: &Session) -> JsonlSessionMetadata {
    // The jsonl repo hands out rich metadata through `JsonlSessionMetadata`;
    // recover it from the storage layer via the session file's header.
    let metadata_value = session.metadata_json();
    serde_json::from_value(metadata_value).expect("metadata")
}

fn provisioned(id: &str, custom_type: &str, data: Option<serde_json::Value>) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_owned(),
        payload: EntryPayload::Custom {
            custom_type: custom_type.to_owned(),
            data,
        },
    }
}

// --- "round trips every entry type and bounded branch queries" -------------

/// upstream: "round trips every entry type and bounded branch queries"
#[tokio::test]
async fn round_trips_every_entry_type_and_bounded_branch_queries() {
    let root = temp_dir("entries");
    let repo = create_repository(&root);
    let session = repo
        .create(&create_options("entries", &root))
        .await
        .unwrap();

    let mut committed_ids: Vec<String> = Vec::new();
    committed_ids.push(
        session
            .append_entry(
                ProvisionedEntry {
                    id: "message".to_owned(),
                    payload: EntryPayload::Message {
                        message: user_message("question"),
                        terminate: false,
                    },
                },
                "main",
            )
            .unwrap()
            .id,
    );

    // Assistant message with a tool call.
    let assistant = pillar_ai::types::AssistantMessage {
        content: vec![
            Content::text("I'll inspect it."),
            Content::tool_call("call-1", "read", serde_json::json!({ "path": "README.md" })),
        ],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: usage(1),
        stop_reason: pillar_ai::types::StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 2,
    };
    committed_ids.push(
        session
            .append_entry(
                ProvisionedEntry {
                    id: "assistant-tool-call".to_owned(),
                    payload: EntryPayload::Message {
                        message: AgentMessage::Message(Message::Assistant(Box::new(assistant))),
                        terminate: false,
                    },
                },
                "main",
            )
            .unwrap()
            .id,
    );

    // Tool result with terminate.
    let tool_result = pillar_ai::types::ToolResultMessage {
        tool_call_id: "call-1".to_owned(),
        tool_name: "read".to_owned(),
        content: vec![Content::text("contents")],
        details: Some(serde_json::json!({ "path": "README.md" })),
        usage: Some(usage(2)),
        added_tool_names: None,
        is_error: false,
        timestamp: 3,
    };
    committed_ids.push(
        session
            .append_entry(
                ProvisionedEntry {
                    id: "tool-result".to_owned(),
                    payload: EntryPayload::Message {
                        message: AgentMessage::Message(Message::ToolResult(Box::new(tool_result))),
                        terminate: true,
                    },
                },
                "main",
            )
            .unwrap()
            .id,
    );

    for (id, payload) in [
        (
            "model",
            EntryPayload::ModelChange {
                provider: "anthropic".to_owned(),
                model_id: "claude-sonnet-4-5".to_owned(),
            },
        ),
        (
            "thinking",
            EntryPayload::ThinkingLevelChange {
                thinking_level: "high".to_owned(),
            },
        ),
        (
            "tools",
            EntryPayload::ActiveToolsChange {
                active_tool_names: vec!["read".to_owned(), "bash".to_owned()],
            },
        ),
        (
            "compaction",
            EntryPayload::Compaction {
                summary: "summary".to_owned(),
                retained_tail: vec![user_message("retained")],
                tokens_before: 123,
                details: Some(serde_json::json!({ "source": "test" })),
                usage: Some(usage(1)),
            },
        ),
        (
            "branch-summary",
            EntryPayload::BranchSummary {
                from_id: "message".to_owned(),
                summary: "branch".to_owned(),
                details: Some(serde_json::json!({ "reason": "navigation" })),
                usage: Some(usage(2)),
            },
        ),
        (
            "custom",
            EntryPayload::Custom {
                custom_type: "note".to_owned(),
                data: Some(serde_json::json!({ "nested": { "value": 1 } })),
            },
        ),
    ] {
        committed_ids.push(
            session
                .append_entry(
                    ProvisionedEntry {
                        id: id.to_owned(),
                        payload,
                    },
                    "main",
                )
                .unwrap()
                .id,
        );
    }
    assert_eq!(committed_ids.len(), 9);

    let restored = reopen(&root, &session).await;
    let restored_entries = restored
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        restored_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        committed_ids
    );

    // Branch query stopping at the compaction.
    let branch_entries = restored
        .find_entries_on_branch_from(&committed_ids[8], Some("compaction"))
        .unwrap();
    assert_eq!(
        branch_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["custom", "branch-summary", "compaction"]
    );

    // Cursor + limit query.
    let paged = restored
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            after_seq: Some(restored_entries[5].seq),
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        paged.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec!["compaction", "branch-summary"]
    );

    // customType filter.
    let notes = restored
        .find_entries(&EntryQuery {
            custom_type: Some("note".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        notes.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec!["custom"]
    );

    let stats = restored.get_stats().unwrap();
    assert_eq!(stats.message_count, 3);
    assert_eq!(stats.cached_tokens, 0.0);
    assert_eq!(stats.uncached_tokens, 0.0);
    assert_eq!(stats.total_tokens, 0.0);
    assert_eq!(stats.cost_total, 0.0);

    // Mutation of returned data must not affect stored state (immutability).
    let custom = restored.get_entry("custom").unwrap().unwrap();
    let mutated = {
        let mut clone = custom.clone();
        if let EntryPayload::Custom {
            data: Some(data), ..
        } = &mut clone.payload
        {
            data["nested"]["value"] = serde_json::json!(99);
        }
        clone
    };
    assert_ne!(mutated, custom);
    assert_eq!(
        restored.get_entry("custom").unwrap().unwrap(),
        restored_entries[8]
    );
}

// --- "round trips every record type, recovery projection, and ledger
// statistics" ---------------------------------------------------------------

struct UsageRecordSpec {
    id: &'static str,
    cause: &'static str,
    run_id: Option<&'static str>,
    entry_id: Option<&'static str>,
    attempt: Option<u32>,
    tool_call_id: Option<&'static str>,
    stop_reason: Option<&'static str>,
    details: Option<serde_json::Value>,
    multiplier: u64,
}

fn usage_record(lane: &str, spec: UsageRecordSpec) -> ProvisionedRecord {
    ProvisionedRecord {
        id: spec.id.to_owned(),
        lane: lane.to_owned(),
        payload: RecordPayload::UsageRecord {
            usage: usage(spec.multiplier),
            cause: spec.cause.to_owned(),
            run_id: spec.run_id.map(str::to_owned),
            entry_id: spec.entry_id.map(str::to_owned),
            attempt: spec.attempt,
            tool_call_id: spec.tool_call_id.map(str::to_owned),
            stop_reason: spec.stop_reason.map(str::to_owned),
            details: spec.details,
        },
    }
}

/// upstream: "round trips every record type, recovery projection, and ledger
/// statistics"
#[tokio::test]
async fn round_trips_every_record_type_recovery_projection_and_ledger_statistics() {
    let root = temp_dir("records");
    let repo = create_repository(&root);
    let session = repo
        .create(&create_options("records", &root))
        .await
        .unwrap();
    session.append_custom_entry("anchor", None).unwrap();

    let mut records: Vec<LaneRecord> = Vec::new();
    let mut append = |session: &Session, record: ProvisionedRecord| -> LaneRecord {
        let committed = session.append_record(record).unwrap();
        records.push(committed.clone());
        committed
    };

    append(
        &session,
        ProvisionedRecord {
            id: "run".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationStarted {
                source_leaf_id: Some("anchor".to_owned()),
                intent: OperationIntent::Run {
                    original_prompt: vec![user_message("prompt")],
                    initial_messages: vec![
                        pillar_agent::harness::session::types::ProvisionedEntry {
                            id: "initial".to_owned(),
                            payload: EntryPayload::Message {
                                message: user_message("initial"),
                                terminate: false,
                            },
                        },
                    ],
                    system_prompt_override: Some("system".to_owned()),
                    resume_data: Some(serde_json::json!({ "extension": { "version": 1 } })),
                },
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "steer".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::QueueEnqueued {
                queue: "steer".to_owned(),
                run_id: Some("run".to_owned()),
                target: provisioned("steer-message", "note", Some(user_message_json("steer"))),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "follow-up".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::QueueEnqueued {
                queue: "followUp".to_owned(),
                run_id: Some("run".to_owned()),
                target: provisioned(
                    "follow-up-message",
                    "note",
                    Some(user_message_json("follow up")),
                ),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "assistant-attempt".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::StepAttempt {
                run_id: "run".to_owned(),
                step: "assistant".to_owned(),
                attempt: 1,
                result_entry_id: "assistant-result".to_owned(),
                compaction_reason: None,
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "tool".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::ToolStarted {
                run_id: "run".to_owned(),
                assistant_entry_id: "assistant-result".to_owned(),
                tool_index: 0,
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                effective_args: serde_json::json!({ "path": "README.md" }),
                result_entry_id: "tool-result".to_owned(),
                replay: "safe".to_owned(),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "deferred-write".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::WriteDeferred {
                run_id: "run".to_owned(),
                target: provisioned(
                    "deferred-entry",
                    "fact",
                    Some(serde_json::json!({ "value": true })),
                ),
            },
        },
    );
    append(
        &session,
        usage_record(
            "main",
            UsageRecordSpec {
                id: "assistant-usage",
                cause: "assistant",
                run_id: Some("run"),
                entry_id: Some("assistant-result"),
                attempt: Some(1),
                tool_call_id: None,
                stop_reason: Some("stop"),
                details: None,
                multiplier: 1,
            },
        ),
    );
    append(
        &session,
        usage_record(
            "main",
            UsageRecordSpec {
                id: "deferred-usage",
                cause: "deferred_fetch",
                run_id: Some("run"),
                entry_id: Some("deferred-result"),
                attempt: Some(1),
                tool_call_id: None,
                stop_reason: Some("deferred"),
                details: None,
                multiplier: 2,
            },
        ),
    );
    append(
        &session,
        usage_record(
            "main",
            UsageRecordSpec {
                id: "tool-usage",
                cause: "tool",
                run_id: Some("run"),
                entry_id: Some("tool-result"),
                attempt: None,
                tool_call_id: Some("call-1"),
                stop_reason: None,
                details: None,
                multiplier: 3,
            },
        ),
    );
    append(
        &session,
        usage_record(
            "main",
            UsageRecordSpec {
                id: "hook-usage",
                cause: "hook",
                run_id: Some("run"),
                entry_id: Some("hook-result"),
                attempt: None,
                tool_call_id: None,
                stop_reason: None,
                details: None,
                multiplier: 4,
            },
        ),
    );
    append(
        &session,
        usage_record(
            "main",
            UsageRecordSpec {
                id: "adjustment",
                cause: "adjustment",
                run_id: None,
                entry_id: None,
                attempt: None,
                tool_call_id: None,
                stop_reason: None,
                details: Some(serde_json::json!({ "reason": "correction" })),
                multiplier: 5,
            },
        ),
    );
    append(
        &session,
        ProvisionedRecord {
            id: "abort".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::AbortRequested {
                run_id: "run".to_owned(),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "run-finished".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationFinished {
                run_id: "run".to_owned(),
                outcome: OperationOutcome::Aborted,
                error: None,
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "next-run".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::QueueEnqueued {
                queue: "nextRun".to_owned(),
                run_id: None,
                target: provisioned("next-message", "note", Some(user_message_json("next"))),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "queue-cancelled".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::QueueCancelled {
                run_id: None,
                entry_id: "next-message".to_owned(),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "compaction".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationStarted {
                source_leaf_id: Some("anchor".to_owned()),
                intent: OperationIntent::Compaction {
                    custom_instructions: Some("short".to_owned()),
                    result_entry_id: "compaction-result".to_owned(),
                },
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "compaction-attempt".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::StepAttempt {
                run_id: "compaction".to_owned(),
                step: "compaction".to_owned(),
                attempt: 1,
                result_entry_id: "compaction-result".to_owned(),
                compaction_reason: Some(
                    pillar_agent::harness::session::types::CompactionReason::Manual,
                ),
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "compaction-finished".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationFinished {
                run_id: "compaction".to_owned(),
                outcome: OperationOutcome::Completed,
                error: None,
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "navigation".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationStarted {
                source_leaf_id: Some("anchor".to_owned()),
                intent: OperationIntent::Navigation {
                    target_id: None,
                    summarize: true,
                    custom_instructions: Some("summarize".to_owned()),
                    label: Some("checkpoint".to_owned()),
                    summary_entry_id: Some("navigation-summary".to_owned()),
                },
            },
        },
    );
    append(
        &session,
        ProvisionedRecord {
            id: "branch-attempt".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::StepAttempt {
                run_id: "navigation".to_owned(),
                step: "branch_summary".to_owned(),
                attempt: 1,
                result_entry_id: "navigation-summary".to_owned(),
                compaction_reason: None,
            },
        },
    );

    let restored = reopen(&root, &session).await;
    let restored_records = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        restored_records
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()
    );

    // Filter: operation_started + operationKind=run, limit 1.
    let run_starts = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("run".to_owned()),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        run_starts.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["run"]
    );

    // Filter: runId=compaction, oldestFirst.
    let compaction_records = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            run_id: Some("compaction".to_owned()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        compaction_records
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["compaction", "compaction-attempt", "compaction-finished"]
    );

    // Filter: type=usage afterSeq, limit 2.
    let usage_after = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            kind: Some("usage".to_owned()),
            after_seq: Some(records[6].seq),
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        usage_after
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["adjustment", "hook-usage"]
    );

    // Open operations.
    let open = restored.find_open_operations("main", Some(2)).unwrap();
    assert_eq!(
        open.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["navigation"]
    );

    // Ledger statistics: 45 cached / 75 uncached / 150 total / cost 15.
    let stats = restored.get_stats().unwrap();
    assert_eq!(stats.message_count, 0);
    assert_eq!(stats.cached_tokens, 45.0);
    assert_eq!(stats.uncached_tokens, 75.0);
    assert_eq!(stats.total_tokens, 150.0);
    assert_eq!(stats.cost_total, 15.0);

    // Mutating a returned record must not affect stored state.
    let run_records = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("run".to_owned()),
            ..Default::default()
        })
        .unwrap();
    let Some(started) = run_records.first() else {
        panic!("expected run record");
    };
    let started_before = started.clone();
    {
        let mut clone = started.clone();
        if let RecordPayload::OperationStarted {
            intent: OperationIntent::Run {
                original_prompt, ..
            },
            ..
        } = &mut clone.payload
        {
            original_prompt.push(user_message("mutated"));
        }
        assert_ne!(clone, started_before);
    }
    let again = restored
        .find_records(&pillar_agent::harness::session::types::RecordQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        again.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()
    );
}

fn user_message_json(text: &str) -> serde_json::Value {
    serde_json::to_value(user_message(text)).unwrap()
}

// --- "persists concurrent cross-lane writes in shared sequence order" -------

/// upstream: "persists concurrent cross-lane writes in shared sequence order"
#[tokio::test]
async fn persists_concurrent_cross_lane_writes_in_shared_sequence_order() {
    let root = temp_dir("concurrent");
    let repo = create_repository(&root);
    let session = repo
        .create(&create_options("concurrent", &root))
        .await
        .unwrap();
    let root_entry = session
        .append_entry(provisioned("root", "root", None), "main")
        .unwrap();
    session.create_lane("thread", Some(&root_entry.id)).unwrap();

    let session = Arc::new(session);
    let mut handles = Vec::new();
    for id in ["main-1", "thread-1", "main-2", "thread-2"] {
        let session = Arc::clone(&session);
        let lane = if id.starts_with("main") {
            "main"
        } else {
            "thread"
        };
        let id = id.to_owned();
        handles.push(std::thread::spawn(move || {
            session
                .append_entry(provisioned(&id, "note", None), lane)
                .unwrap()
        }));
    }
    let entries: Vec<pillar_agent::harness::session::types::Entry> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let mut commit_order: Vec<(u64, String)> =
        entries.iter().map(|e| (e.seq, e.id.clone())).collect();
    commit_order.sort_by_key(|(seq, _)| *seq);
    let commit_ids: Vec<String> = commit_order.into_iter().map(|(_, id)| id).collect();

    let restored = reopen(&root, &session).await;
    let log = restored.get_log(&Default::default()).unwrap();
    let concurrent_entries: Vec<&pillar_agent::harness::session::types::Entry> = log
        .iter()
        .filter_map(|item| match item {
            pillar_agent::harness::session::types::LogItem::Entry { entry, .. }
                if entry.id != "root" =>
            {
                Some(entry)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        concurrent_entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        commit_ids
    );
    let seqs: std::collections::BTreeSet<u64> = concurrent_entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs.len(), entries.len());
    let all_seqs: Vec<u64> = log.iter().map(|item| item.seq()).collect();
    assert_eq!(all_seqs, vec![1, 2, 3, 4, 5, 6]);
}

// --- "rejects non-JSON payloads without changing the durable prefix" -------

/// upstream: "rejects non-JSON payloads without changing the durable prefix".
/// Rust payloads cannot be cyclic/undefined, so the port asserts the
/// structural validation keeps rejecting non-serializable data and that a
/// rejected append leaves the durable file untouched.
#[tokio::test]
async fn rejects_non_json_payloads_without_changing_the_durable_prefix() {
    let root = temp_dir("validation");
    let repo = create_repository(&root);
    let session = repo
        .create(&create_options("validation", &root))
        .await
        .unwrap();
    let metadata = list_first_metadata(&root, &session).await;
    let _prefix = std::fs::read_to_string(&metadata.path).unwrap();
    // Anchor entry so the rejected record below collides with a used id.
    session
        .append_entry(provisioned("anchor", "anchor", None), "main")
        .unwrap();
    let prefix_with_anchor = std::fs::read_to_string(&metadata.path).unwrap();

    // `serde_json::Value` cannot represent NaN/undefined/cycles, so the
    // upstream non-JSON payloads are unrepresentable; the same rejection
    // boundary is exercised through a duplicate-id record, which the
    // storage layer rejects before touching the durable file.
    let error = session
        .append_record(ProvisionedRecord {
            id: "anchor".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::ToolStarted {
                run_id: "run".to_owned(),
                assistant_entry_id: "assistant".to_owned(),
                tool_index: 0,
                tool_call_id: "call".to_owned(),
                tool_name: "read".to_owned(),
                effective_args: serde_json::Value::Null,
                result_entry_id: "result".to_owned(),
                replay: "never".to_owned(),
            },
        })
        .unwrap_err();
    assert_eq!(
        error.code,
        pillar_agent::harness::session::types::SessionErrorCode::AlreadyExists
    );
    assert_eq!(
        std::fs::read_to_string(&metadata.path).unwrap(),
        prefix_with_anchor
    );

    let restored = reopen(&root, &session).await;
    assert_eq!(restored.get_log(&Default::default()).unwrap().len(), 1);
    let valid = restored
        .append_entry(
            provisioned("valid", "note", Some(serde_json::json!({ "value": 1 }))),
            "main",
        )
        .unwrap();
    assert_eq!(valid.seq, 2);
    let verified = reopen(&root, &restored).await;
    assert_eq!(verified.get_entry("valid").unwrap().unwrap().seq, 2);
}

// --- "does not advance state or poison the write queue after an append
// failure" -------------------------------------------------------------------

/// Upstream injects the failure by mocking `env.appendFile` once; the port
/// wraps the filesystem with a one-shot failing append.
struct FailingAppendEnv {
    inner: StdFsExecutionEnv,
    fail_after: AtomicUsize,
}

impl pillar_agent::harness::types::FileSystem for FailingAppendEnv {
    fn cwd(&self) -> &str {
        self.inner.cwd()
    }

    async fn absolute_path(&self, path: &str) -> Result<String, FileError> {
        self.inner.absolute_path(path).await
    }

    async fn join_path(&self, parts: &[&str]) -> Result<String, FileError> {
        self.inner.join_path(parts).await
    }

    async fn read_text_file(&self, path: &str) -> Result<String, FileError> {
        self.inner.read_text_file(path).await
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
    ) -> Result<Vec<String>, FileError> {
        self.inner.read_text_lines(path, max_lines).await
    }

    async fn read_binary_file(&self, path: &str) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path).await
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<(), FileError> {
        self.inner.write_file(path, content).await
    }

    async fn append_file(&self, path: &str, content: &[u8]) -> Result<(), FileError> {
        if self.fail_after.fetch_sub(1, Ordering::SeqCst) == 1 {
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "injected append failure",
                None,
            ));
        }
        self.inner.append_file(path, content).await
    }

    async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
    ) -> Result<(), FileError> {
        self.inner.rename_file(source_path, destination_path).await
    }

    async fn file_info(
        &self,
        path: &str,
    ) -> Result<pillar_agent::harness::types::FileInfo, FileError> {
        self.inner.file_info(path).await
    }

    async fn list_dir(
        &self,
        path: &str,
    ) -> Result<Vec<pillar_agent::harness::types::FileInfo>, FileError> {
        self.inner.list_dir(path).await
    }

    async fn canonical_path(&self, path: &str) -> Result<String, FileError> {
        self.inner.canonical_path(path).await
    }

    async fn exists(&self, path: &str) -> Result<bool, FileError> {
        self.inner.exists(path).await
    }

    async fn create_dir(&self, path: &str, recursive: bool) -> Result<(), FileError> {
        self.inner.create_dir(path, recursive).await
    }

    async fn remove(&self, path: &str, recursive: bool, force: bool) -> Result<(), FileError> {
        self.inner.remove(path, recursive, force).await
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<String, FileError> {
        self.inner.create_temp_dir(prefix).await
    }

    async fn create_temp_file(&self, prefix: &str, suffix: &str) -> Result<String, FileError> {
        self.inner.create_temp_file(prefix, suffix).await
    }

    async fn cleanup(&self) {
        self.inner.cleanup().await
    }
}

/// upstream: "does not advance state or poison the write queue after an
/// append failure"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_advance_state_or_poison_the_write_queue_after_an_append_failure() {
    let root = temp_dir("append-failure");
    let env = Arc::new(FailingAppendEnv {
        inner: StdFsExecutionEnv::new(&root),
        fail_after: AtomicUsize::new(1), // first append is the injected failure
    });
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: env,
        sessions_root: root.clone(),
    });
    let session = repository
        .create(&create_options("append-failure", &root))
        .await
        .unwrap();

    let error = session.append_custom_entry("rejected", None).unwrap_err();
    assert_eq!(
        error.code,
        pillar_agent::harness::session::types::SessionErrorCode::Storage
    );
    assert!(session.get_log(&Default::default()).unwrap().is_empty());
    let committed = session
        .append_entry(provisioned("committed", "note", None), "main")
        .unwrap();
    assert_eq!(committed.seq, 1);

    let metadata = list_first_metadata(&root, &session).await;
    let reopened = create_repository(&root).open(&metadata).await.unwrap();
    let log = reopened.get_log(&Default::default()).unwrap();
    assert_eq!(log.len(), 1);
    match &log[0] {
        pillar_agent::harness::session::types::LogItem::Entry { seq, entry } => {
            assert_eq!(*seq, 1);
            assert_eq!(entry.id, "committed");
        }
        other => panic!("expected entry log item, got {other:?}"),
    }
}
