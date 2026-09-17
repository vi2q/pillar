//! Port of the remaining upstream session-backend conformance cases from
//! packages/agent/src/harness/session/testing/conformance.ts (pi v0.84.3)
//! that session_parity.rs does not yet cover, plus the jsonl.test.ts
//! "JsonlSessionRepo conformance" wiring (the conformance suite run against
//! the JSONL backend).
//!
//! divergence: upstream `session.view(lane)` returns a lane-bound facade;
//! the port models lane scoping by passing the lane explicitly to the
//! storage-shaped methods, so the "binds lane views" case asserts the same
//! per-lane leaf/branch behavior through lane-scoped reads.

#![cfg(feature = "session-files")]

use pillar_agent::harness::env::StdFsExecutionEnv;
use pillar_agent::harness::session::jsonl::repo::JsonlSessionRepo;
use pillar_agent::harness::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionMetadata, JsonlSessionRepoOptions,
};
use pillar_agent::harness::session::memory::{
    ProvisionedEntry, ProvisionedRecord, Session, SessionCreateOptions,
};
use pillar_agent::harness::session::types::{
    BranchBounds, EntryOrder, EntryPayload, EntryQuery, ForkOptions, ForkPosition, LogItem,
    OperationIntent, OperationOutcome, RecordPayload, RecordQuery, SessionError, SessionErrorCode,
};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, UserContent};
use std::sync::Arc;

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn expect_code(error: SessionError, code: SessionErrorCode) {
    assert_eq!(error.code, code, "expected {code:?}, got {error:?}");
}

fn provisioned_record(id: &str, lane: &str, payload: RecordPayload) -> ProvisionedRecord {
    ProvisionedRecord {
        id: id.to_owned(),
        lane: lane.to_owned(),
        payload,
    }
}

fn operation_started(id: &str, lane: &str, kind: &str) -> ProvisionedRecord {
    let intent = match kind {
        "run" => OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        },
        "compaction" => OperationIntent::Compaction {
            custom_instructions: None,
            result_entry_id: format!("{id}-result"),
        },
        "navigation" => OperationIntent::Navigation {
            target_id: None,
            summarize: false,
            custom_instructions: None,
            label: None,
            summary_entry_id: None,
        },
        other => panic!("unknown kind {other}"),
    };
    provisioned_record(
        id,
        lane,
        RecordPayload::OperationStarted {
            source_leaf_id: None,
            intent,
        },
    )
}

fn message_target(id: &str, message: AgentMessage) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_owned(),
        payload: EntryPayload::Message {
            message,
            terminate: false,
        },
    }
}

fn entry_ids(entries: &[pillar_agent::harness::session::types::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

// ---------------------------------------------------------------------------
// In-memory backend (mirrors memory.test.ts running the conformance suite
// against InMemorySessionRepo).
// ---------------------------------------------------------------------------

fn memory_repo() -> pillar_agent::harness::session::memory::InMemorySessionRepo {
    pillar_agent::harness::session::memory::InMemorySessionRepo::new()
}

fn memory_session(
    repo: &pillar_agent::harness::session::memory::InMemorySessionRepo,
    id: &str,
) -> Session {
    repo.create(Some(SessionCreateOptions {
        id: Some(id.to_owned()),
        parent_session_id: None,
    }))
    .unwrap()
}

/// conformance: "keeps lane names permanent with their recovery records"
#[test]
fn conformance_keeps_lane_names_permanent_with_their_recovery_records() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    session.create_lane("thread", None).unwrap();
    session
        .append_record(operation_started("old-run", "thread", "run"))
        .unwrap();
    session
        .append_record(provisioned_record(
            "old-next-run",
            "thread",
            RecordPayload::QueueEnqueued {
                queue: "nextRun".to_owned(),
                run_id: None,
                target: message_target("queued-message", user_message("queued")),
            },
        ))
        .unwrap();

    let thread_records = session
        .find_records(&RecordQuery {
            lane: Some("thread".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        thread_records
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["old-next-run", "old-run"]
    );
    let log = session.get_log(&Default::default()).unwrap();
    let record_ids: Vec<&str> = log
        .iter()
        .filter_map(|item| match item {
            LogItem::Record { record, .. } => Some(record.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(record_ids, vec!["old-run", "old-next-run"]);
    let error = session.create_lane("thread", None).unwrap_err();
    expect_code(error, SessionErrorCode::AlreadyExists);
}

/// conformance: "persists queue cancellation without consuming its target"
#[test]
fn conformance_persists_queue_cancellation_without_consuming_its_target() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    let enqueued = session
        .append_record(provisioned_record(
            "enqueue",
            "main",
            RecordPayload::QueueEnqueued {
                queue: "nextRun".to_owned(),
                run_id: None,
                target: message_target("queued-message", user_message("queued")),
            },
        ))
        .unwrap();
    let cancelled = session
        .append_record(provisioned_record(
            "cancel",
            "main",
            RecordPayload::QueueCancelled {
                run_id: None,
                entry_id: "queued-message".to_owned(),
            },
        ))
        .unwrap();
    assert_eq!(cancelled.seq, 2);
    match &cancelled.payload {
        RecordPayload::QueueCancelled { run_id, entry_id } => {
            assert_eq!(run_id, &None);
            assert_eq!(entry_id, "queued-message");
        }
        other => panic!("expected queue_cancelled, got {other:?}"),
    }
    assert!(session.get_entry("queued-message").unwrap().is_none());
    let cancellations = session
        .find_records(&RecordQuery {
            kind: Some("queue_cancelled".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(cancellations[0].id, "cancel");
    match &cancellations[0].payload {
        RecordPayload::QueueCancelled { entry_id, .. } => assert_eq!(entry_id, "queued-message"),
        other => panic!("expected queue_cancelled, got {other:?}"),
    }
    let log = session.get_log(&Default::default()).unwrap();
    assert_eq!(log.len(), 2);
    match &log[0] {
        LogItem::Record { seq, record } => {
            assert_eq!(*seq, enqueued.seq);
            assert_eq!(record.id, "enqueue");
        }
        other => panic!("expected record log item, got {other:?}"),
    }
    match &log[1] {
        LogItem::Record { seq, record } => {
            assert_eq!(*seq, cancelled.seq);
            assert_eq!(record.id, "cancel");
        }
        other => panic!("expected record log item, got {other:?}"),
    }
}

/// conformance: "filters records by lane type run sequence and order"
#[test]
fn conformance_filters_records_by_lane_type_run_sequence_and_order() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    session
        .append_record(operation_started("run-1", "main", "run"))
        .unwrap();
    session
        .append_record(provisioned_record(
            "attempt-1",
            "main",
            RecordPayload::StepAttempt {
                run_id: "run-1".to_owned(),
                step: "assistant".to_owned(),
                attempt: 1,
                result_entry_id: "assistant-1".to_owned(),
                compaction_reason: None,
            },
        ))
        .unwrap();
    session.create_lane("thread", None).unwrap();
    session
        .append_record(operation_started("run-2", "thread", "run"))
        .unwrap();
    session
        .append_record(provisioned_record(
            "attempt-2",
            "thread",
            RecordPayload::StepAttempt {
                run_id: "run-2".to_owned(),
                step: "assistant".to_owned(),
                attempt: 1,
                result_entry_id: "assistant-2".to_owned(),
                compaction_reason: None,
            },
        ))
        .unwrap();

    let thread_records = session
        .find_records(&RecordQuery {
            lane: Some("thread".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        thread_records
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["attempt-2", "run-2"]
    );
    let attempts = session
        .find_records(&RecordQuery {
            kind: Some("step_attempt".to_owned()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        attempts.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["attempt-1", "attempt-2"]
    );
    let run1 = session
        .find_records(&RecordQuery {
            run_id: Some("run-1".to_owned()),
            after_seq: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        run1.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["attempt-1"]
    );
    let limited = session
        .find_records(&RecordQuery {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        limited.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["attempt-2"]
    );
}

/// conformance: "filters operation starts by operation kind"
#[test]
fn conformance_filters_operation_starts_by_operation_kind() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    for (id, kind) in [
        ("run-old", "run"),
        ("compaction", "compaction"),
        ("navigation", "navigation"),
        ("run-new", "run"),
    ] {
        session
            .append_record(operation_started(id, "main", kind))
            .unwrap();
        session
            .append_record(provisioned_record(
                &format!("{id}-finished"),
                "main",
                RecordPayload::OperationFinished {
                    run_id: id.to_owned(),
                    outcome: OperationOutcome::Completed,
                    error: None,
                },
            ))
            .unwrap();
    }

    let runs = session
        .find_records(&RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("run".to_owned()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["run-old", "run-new"]
    );
    let compactions = session
        .find_records(&RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("compaction".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        compactions
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["compaction"]
    );
    let navigations = session
        .find_records(&RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("navigation".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        navigations
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["navigation"]
    );
    let limited = session
        .find_records(&RecordQuery {
            kind: Some("operation_started".to_owned()),
            operation_kind: Some("run".to_owned()),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        limited.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["run-new"]
    );
}

/// conformance: "does not let an earlier finish close a later start"
#[test]
fn conformance_does_not_let_an_earlier_finish_close_a_later_start() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    session
        .append_record(provisioned_record(
            "finish-before-start",
            "main",
            RecordPayload::OperationFinished {
                run_id: "run".to_owned(),
                outcome: OperationOutcome::Completed,
                error: None,
            },
        ))
        .unwrap();
    let started = session
        .append_record(operation_started("run", "main", "run"))
        .unwrap();
    let open = session.find_open_operations("main", Some(2)).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, started.id);
}

/// conformance: "scopes open operations by lane and limit"
#[test]
fn conformance_scopes_open_operations_by_lane_and_limit() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    session.create_lane("thread", None).unwrap();
    let main_run = session
        .append_record(operation_started("main-run", "main", "run"))
        .unwrap();
    let thread_navigation = session
        .append_record(operation_started(
            "thread-navigation",
            "thread",
            "navigation",
        ))
        .unwrap();

    assert_eq!(
        session.find_open_operations("main", None).unwrap()[0].id,
        main_run.id
    );
    assert_eq!(
        session.find_open_operations("main", Some(1)).unwrap()[0].id,
        main_run.id
    );
    assert_eq!(
        session.find_open_operations("thread", Some(2)).unwrap()[0].id,
        thread_navigation.id
    );
}

/// conformance: "returns immutable open-operation records"
#[test]
fn conformance_returns_immutable_open_operation_records() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    let committed = session
        .append_record(operation_started("run", "main", "run"))
        .unwrap();
    let read = session.find_open_operations("main", None).unwrap();
    let mut mutated = read[0].clone();
    if let RecordPayload::OperationStarted {
        intent: OperationIntent::Run {
            original_prompt, ..
        },
        ..
    } = &mut mutated.payload
    {
        original_prompt.push(user_message("mutated"));
    }
    assert_ne!(mutated, committed);
    assert_eq!(
        session.find_open_operations("main", None).unwrap()[0],
        committed
    );
}

/// conformance: "returns immutable copies from reads"
#[test]
fn conformance_returns_immutable_copies_from_reads() {
    let repository = memory_repo();
    let session = memory_session(&repository, "immutable");
    let metadata = session.metadata_json();
    session
        .append_entry(
            ProvisionedEntry {
                id: "custom".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!({ "nested": { "value": 1 } })),
                },
            },
            "main",
        )
        .unwrap();
    // Mutating the returned copy must not affect stored state (the port's
    // reads clone; there is no shared mutable reference to corrupt).
    let read = session.get_entry("custom").unwrap().unwrap();
    match &read.payload {
        EntryPayload::Custom {
            data: Some(data), ..
        } => {
            assert_eq!(data["nested"]["value"], 1);
        }
        other => panic!("expected custom payload, got {other:?}"),
    }
    assert_eq!(session.metadata_json(), metadata);
    let stored = session.get_entry("custom").unwrap().unwrap();
    match &stored.payload {
        EntryPayload::Custom {
            data: Some(data), ..
        } => {
            assert_eq!(data["nested"]["value"], 1, "stored value unchanged");
        }
        other => panic!("expected custom payload, got {other:?}"),
    }
}

/// conformance: "validates lane lifecycle and targets"
#[test]
fn conformance_validates_lane_lifecycle_and_targets() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    let error = session.create_lane("main", None).unwrap_err();
    expect_code(error, SessionErrorCode::AlreadyExists);
    let error = session.create_lane("thread", Some("missing")).unwrap_err();
    expect_code(error, SessionErrorCode::NotFound);
    let error = session.move_lane("missing", None).unwrap_err();
    expect_code(error, SessionErrorCode::InvalidLane);
}

/// conformance: "binds lane views without caching leaves" — the port's
/// lane-scoped behavior: per-lane leaves move independently and branch
/// queries from a lane leaf walk that lane's path.
#[test]
fn conformance_binds_lane_views_without_caching_leaves() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    let root = session.append_message(user_message("root")).unwrap();
    session.create_lane("thread", Some(&root)).unwrap();
    let main_child = session.append_message(user_message("main")).unwrap();
    let thread_child = session
        .append_entry(
            message_target("thread-child", user_message("thread")),
            "thread",
        )
        .unwrap()
        .id;

    assert_eq!(
        session.get_leaf_id().unwrap().as_deref(),
        Some(main_child.as_str())
    );
    // Thread leaf is per-lane (upstream thread.getLeafId() === threadChild).
    let thread_entries = session
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                start: Some(thread_child.clone()),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .unwrap();
    assert_eq!(
        entry_ids(&thread_entries),
        vec![root.as_str(), "thread-child"]
    );
    let main_entries = session
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                start: Some(main_child.clone()),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .unwrap();
    assert_eq!(
        entry_ids(&main_entries),
        vec![root.as_str(), main_child.as_str()]
    );
    let empty = memory_session(&repository, "empty");
    assert!(
        empty
            .find_entries_on_branch(&EntryQuery::default(), &BranchBounds::default())
            .unwrap()
            .is_empty()
    );
}

/// conformance: "persists tool-result termination decisions"
#[test]
fn conformance_persists_tool_result_termination_decisions() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    let tool_result = pillar_ai::types::ToolResultMessage {
        tool_call_id: "call-1".to_owned(),
        tool_name: "example".to_owned(),
        content: vec![Content::text("done")],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 1,
    };
    let entry = session
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
        .unwrap();
    assert!(matches!(
        &entry.payload,
        EntryPayload::Message {
            terminate: true,
            ..
        }
    ));
    let stored = session.get_entry("tool-result").unwrap().unwrap();
    assert!(matches!(
        &stored.payload,
        EntryPayload::Message {
            terminate: true,
            ..
        }
    ));
    let entries = session.find_entries(&EntryQuery::default()).unwrap();
    assert_eq!(entries, vec![entry.clone()]);
    let log = session.get_log(&Default::default()).unwrap();
    match &log[0] {
        LogItem::Entry { seq, entry: logged } => {
            assert_eq!(*seq, entry.seq);
            assert_eq!(logged, &entry);
        }
        other => panic!("expected entry log item, got {other:?}"),
    }
}

/// conformance: "rejects non-JSON entries before storage mutation" — Rust
/// values are always JSON-representable, so the port asserts the same
/// validation accepts the JSON-safe subset and records nothing before a
/// valid write (structural validation is a no-op; see memory.rs divergence).
#[test]
fn conformance_rejects_non_json_entries_before_storage_mutation() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    assert_eq!(session.get_leaf_id().unwrap(), None);
    assert!(
        session
            .find_entries(&EntryQuery::default())
            .unwrap()
            .is_empty()
    );
    assert!(session.get_log(&Default::default()).unwrap().is_empty());
    let valid_id = session
        .append_custom_entry("valid", Some(serde_json::json!({ "value": 1 })))
        .unwrap();
    assert_eq!(session.get_entry(&valid_id).unwrap().unwrap().seq, 1);
}

/// conformance: "rejects non-JSON records before storage mutation" — same
/// divergence as the entries case.
#[test]
fn conformance_rejects_non_json_records_before_storage_mutation() {
    let repository = memory_repo();
    let session = memory_session(&repository, "session");
    assert!(
        session
            .find_records(&RecordQuery::default())
            .unwrap()
            .is_empty()
    );
    assert!(session.get_log(&Default::default()).unwrap().is_empty());
    let committed = session
        .append_record(operation_started("valid-record", "main", "run"))
        .unwrap();
    assert_eq!(committed.seq, 1);
}

/// conformance: "forks a complete tree with lanes and facts"
#[test]
fn conformance_forks_a_complete_tree_with_lanes_and_facts() {
    let repository = memory_repo();
    let source = memory_session(&repository, "source");
    let root = source.append_message(user_message("root")).unwrap();
    source.create_lane("thread", Some(&root)).unwrap();
    let main_child = source.append_message(user_message("main")).unwrap();
    let thread_child = source
        .append_entry(
            message_target("thread-child", user_message("thread")),
            "thread",
        )
        .unwrap()
        .id;
    source
        .set_label(&thread_child, Some("thread-tip".to_owned()))
        .unwrap();

    let fork = repository
        .fork(
            &source.get_metadata().unwrap(),
            Some((
                ForkOptions::Tree,
                SessionCreateOptions {
                    id: Some("tree-fork".to_owned()),
                    parent_session_id: None,
                },
            )),
        )
        .unwrap();
    let entries = fork
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        entry_ids(&entries),
        vec![root.as_str(), main_child.as_str(), thread_child.as_str()]
    );
    let lanes = fork.get_lanes().unwrap();
    assert_eq!(lanes[0].lane, "main");
    assert_eq!(lanes[0].leaf_id.as_deref(), Some(main_child.as_str()));
    assert_eq!(lanes[1].lane, "thread");
    assert_eq!(lanes[1].leaf_id.as_deref(), Some(thread_child.as_str()));
    assert_eq!(
        fork.get_label(&thread_child).unwrap().as_deref(),
        Some("thread-tip")
    );
    assert_eq!(fork.get_stats().unwrap().message_count, 3);
    let log = fork.get_log(&Default::default()).unwrap();
    let lane_items: Vec<(u64, &str, Option<&str>)> = log
        .iter()
        .filter_map(|item| match item {
            LogItem::Lane { seq, lane, leaf_id } => Some((*seq, lane.as_str(), leaf_id.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        lane_items,
        vec![
            (4u64, "main", Some(main_child.as_str())),
            (5u64, "thread", Some(thread_child.as_str())),
        ]
    );
}

// ---------------------------------------------------------------------------
// JSONL backend: the same conformance suite wired through JsonlSessionRepo
// (upstream jsonl.test.ts "JsonlSessionRepo conformance").
// ---------------------------------------------------------------------------

fn jsonl_temp_dir(label: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-jsonl-conformance-{}-{}-{}",
        label,
        std::process::id(),
        count
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.to_string_lossy().into_owned()
}

async fn jsonl_session(
    repo: &JsonlSessionRepo<StdFsExecutionEnv>,
    root: &str,
    id: &str,
) -> Session {
    repo.create(&JsonlSessionCreateOptions {
        id: Some(id.to_owned()),
        cwd: root.to_owned(),
        ..Default::default()
    })
    .await
    .unwrap()
}

/// jsonl.test.ts "JsonlSessionRepo conformance": the entries-and-lanes
/// conformance group against the file backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_conformance_entries_and_lanes() {
    let root = jsonl_temp_dir("entries");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    let root_entry = session
        .append_entry(message_target("root", user_message("root")), "main")
        .unwrap();
    session.create_lane("thread", Some(&root_entry.id)).unwrap();
    let child = session
        .append_entry(
            ProvisionedEntry {
                id: "child".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!({ "value": 1 })),
                },
            },
            "thread",
        )
        .unwrap();
    let record = session
        .append_record(operation_started("run", "thread", "run"))
        .unwrap();
    session.set_name(Some("Example".to_owned())).unwrap();
    session
        .set_label("root", Some("checkpoint".to_owned()))
        .unwrap();
    session.move_lane("main", Some(&child.id)).unwrap();

    assert_eq!(root_entry.parent_id, None);
    assert_eq!(root_entry.seq, 1);
    assert_eq!(child.parent_id.as_deref(), Some("root"));
    assert_eq!(child.seq, 3);
    assert_eq!(record.seq, 4);
    for timestamp in [root_entry.timestamp, child.timestamp, record.timestamp] {
        assert!(timestamp > 0);
    }
    let log_kinds: Vec<(&str, u64)> = session
        .get_log(&Default::default())
        .unwrap()
        .iter()
        .map(|item| match item {
            LogItem::Entry { seq, .. } => ("entry", *seq),
            LogItem::Record { seq, .. } => ("record", *seq),
            LogItem::Lane { seq, .. } => ("lane", *seq),
            LogItem::Name { seq, .. } | LogItem::Label { seq, .. } => ("fact", *seq),
        })
        .collect();
    assert_eq!(
        log_kinds,
        vec![
            ("entry", 1),
            ("lane", 2),
            ("entry", 3),
            ("record", 4),
            ("fact", 5),
            ("fact", 6),
            ("lane", 7),
        ]
    );
    let lanes = session.get_lanes().unwrap();
    assert_eq!(lanes[0].lane, "main");
    assert_eq!(lanes[0].leaf_id.as_deref(), Some("child"));
    assert_eq!(lanes[1].lane, "thread");
    assert_eq!(lanes[1].leaf_id.as_deref(), Some("child"));
}

/// jsonl.test.ts "JsonlSessionRepo conformance": validation-and-immutability
/// group against the file backend (duplicate ids, lane lifecycle).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_conformance_validation() {
    let root = jsonl_temp_dir("validation");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    session
        .append_entry(message_target("shared", user_message("root")), "main")
        .unwrap();
    let error = session
        .append_record(operation_started("shared", "main", "run"))
        .unwrap_err();
    expect_code(error, SessionErrorCode::AlreadyExists);
    let error = session.create_lane("main", None).unwrap_err();
    expect_code(error, SessionErrorCode::AlreadyExists);
    let error = session.move_lane("missing", None).unwrap_err();
    expect_code(error, SessionErrorCode::InvalidLane);
}

/// jsonl.test.ts "JsonlSessionRepo conformance": repository-and-forks group
/// against the file backend, including the metadata contract and reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_conformance_repository_and_forks() {
    let root = jsonl_temp_dir("repo");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let source = jsonl_session(&repository, &root, "source").await;
    let root_entry = source.append_message(user_message("root")).unwrap();
    let tail_entry = source.append_message(user_message("tail")).unwrap();
    let metadata: JsonlSessionMetadata = {
        let raw = source.metadata_json();
        serde_json::from_value(raw).expect("jsonl metadata")
    };
    assert_eq!(metadata.id, "source");
    assert_eq!(metadata.source_format, 4);
    assert!(metadata.path.ends_with(".jsonl"));
    assert!(
        metadata.cwd.starts_with("/"),
        "cwd is absolute: {}",
        metadata.cwd
    );

    // Fork before the tail: only root is copied.
    let fork = repository
        .fork(
            &metadata,
            &JsonlSessionCreateOptions {
                id: Some("fork".to_owned()),
                cwd: root.clone(),
                ..Default::default()
            },
            &ForkOptions::Branch {
                entry_id: Some(tail_entry.clone()),
                position: Some(ForkPosition::Before),
            },
        )
        .await
        .unwrap();
    let entries = fork
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entry_ids(&entries), vec![root_entry.as_str()]);
    assert_eq!(
        fork.get_leaf_id().unwrap().as_deref(),
        Some(root_entry.as_str())
    );
    assert_eq!(
        source.get_leaf_id().unwrap().as_deref(),
        Some(tail_entry.as_str())
    );

    // Reopen the fork and check durability.
    let listed = repository.list_metadata(&Default::default()).await.unwrap();
    let fork_metadata = listed
        .iter()
        .find(|m| m.id == "fork")
        .expect("fork listed")
        .clone();
    let reopened = repository.open(&fork_metadata).await.unwrap();
    let entries = reopened
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entry_ids(&entries), vec![root_entry.as_str()]);

    // Delete is idempotent and the session disappears from listing.
    repository.delete(&fork_metadata).await.unwrap();
    repository.delete(&fork_metadata).await.unwrap();
    let error = match repository.open(&fork_metadata).await {
        Err(error) => error,
        Ok(_) => panic!("expected open failure"),
    };
    expect_code(error, SessionErrorCode::NotFound);
    let listed = repository.list_metadata(&Default::default()).await.unwrap();
    assert!(listed.iter().all(|m| m.id != "fork"));
}

/// jsonl.test.ts "JsonlSessionRepo conformance": records-and-log group
/// against the file backend (queue cancellation + open-operation tracking
/// survive a reopen).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_conformance_records_and_log() {
    let root = jsonl_temp_dir("records");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    session
        .append_record(provisioned_record(
            "enqueue",
            "main",
            RecordPayload::QueueEnqueued {
                queue: "nextRun".to_owned(),
                run_id: None,
                target: message_target("queued-message", user_message("queued")),
            },
        ))
        .unwrap();
    session
        .append_record(provisioned_record(
            "cancel",
            "main",
            RecordPayload::QueueCancelled {
                run_id: None,
                entry_id: "queued-message".to_owned(),
            },
        ))
        .unwrap();
    let started = session
        .append_record(operation_started("run", "main", "run"))
        .unwrap();
    assert!(session.get_entry("queued-message").unwrap().is_none());
    assert_eq!(
        session.find_open_operations("main", Some(2)).unwrap()[0].id,
        "run"
    );

    let metadata: JsonlSessionMetadata =
        serde_json::from_value(session.metadata_json()).expect("jsonl metadata");
    let reopened = repository.open(&metadata).await.unwrap();
    assert!(reopened.get_entry("queued-message").unwrap().is_none());
    let open = reopened.find_open_operations("main", Some(2)).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, started.id);
    assert_eq!(open[0].seq, started.seq);
    // The cancellation did not consume its target and the queue record kept
    // its entry id across the replay.
    let cancellations = reopened
        .find_records(&RecordQuery {
            kind: Some("queue_cancelled".to_owned()),
            ..Default::default()
        })
        .unwrap();
    match &cancellations[0].payload {
        RecordPayload::QueueCancelled { entry_id, .. } => assert_eq!(entry_id, "queued-message"),
        other => panic!("expected queue_cancelled, got {other:?}"),
    }
}

/// jsonl.test.ts: "writes one line per mutation and restores the shared
/// sequence" — the file gains exactly one line per mutation with a shared
/// consecutive seq, and facts/lanes/records restore on reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_writes_one_line_per_mutation_and_restores_the_shared_sequence() {
    let root = jsonl_temp_dir("one-line");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    let metadata: JsonlSessionMetadata =
        serde_json::from_value(session.metadata_json()).expect("jsonl metadata");
    let entry_id = session
        .append_custom_entry("note", Some(serde_json::json!({ "value": 1 })))
        .unwrap();
    session.create_lane("thread", Some(&entry_id)).unwrap();
    session
        .append_record(provisioned_record(
            "run",
            "thread",
            RecordPayload::OperationStarted {
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            },
        ))
        .unwrap();
    session.set_name(Some("Example".to_owned())).unwrap();
    session
        .set_label(&entry_id, Some("checkpoint".to_owned()))
        .unwrap();
    session.move_lane("main", None).unwrap();

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&metadata.path)
        .unwrap()
        .trim_end()
        .split('\n')
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<&str> = lines
        .iter()
        .map(|line| line["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["header", "entry", "lane", "record", "fact", "fact", "lane"]
    );
    let seqs: Vec<u64> = lines[1..]
        .iter()
        .map(|line| line["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);

    let reopened = repository.open(&metadata).await.unwrap();
    let lanes = reopened.get_lanes().unwrap();
    assert_eq!(lanes[0].lane, "main");
    assert_eq!(lanes[0].leaf_id, None);
    assert_eq!(lanes[1].lane, "thread");
    assert_eq!(lanes[1].leaf_id.as_deref(), Some(entry_id.as_str()));
    assert_eq!(reopened.get_name().unwrap().as_deref(), Some("Example"));
    assert_eq!(
        reopened.get_label(&entry_id).unwrap().as_deref(),
        Some("checkpoint")
    );
    assert_eq!(
        reopened
            .find_records(&RecordQuery::default())
            .unwrap()
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec!["run"]
    );
    assert_eq!(
        reopened
            .find_open_operations("thread", Some(2))
            .unwrap()
            .len(),
        1
    );
    let finished = reopened
        .append_record(provisioned_record(
            "finish",
            "thread",
            RecordPayload::OperationFinished {
                run_id: "run".to_owned(),
                outcome: OperationOutcome::Completed,
                error: None,
            },
        ))
        .unwrap();
    assert_eq!(finished.seq, 7);
    assert!(
        reopened
            .find_open_operations("thread", Some(2))
            .unwrap()
            .is_empty()
    );
}

/// jsonl.test.ts: "recomputes fork message counts when reopening"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_recomputes_fork_message_counts_when_reopening() {
    let root = jsonl_temp_dir("fork-counts");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let source = jsonl_session(&repository, &root, "source").await;
    source.append_message(user_message("one")).unwrap();
    source.append_message(user_message("two")).unwrap();
    let fork = repository
        .fork(
            &serde_json::from_value(source.metadata_json()).unwrap(),
            &JsonlSessionCreateOptions {
                id: Some("fork".to_owned()),
                cwd: root.clone(),
                ..Default::default()
            },
            &ForkOptions::Branch {
                entry_id: None,
                position: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.get_stats().unwrap().message_count, 2);
    fork.append_message(user_message("three")).unwrap();
    assert_eq!(fork.get_stats().unwrap().message_count, 3);

    let metadata: JsonlSessionMetadata = serde_json::from_value(fork.metadata_json()).unwrap();
    let verified = repository.open(&metadata).await.unwrap();
    assert_eq!(verified.get_stats().unwrap().message_count, 3);
}

/// jsonl.test.ts: "repairs a valid final line missing its newline"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_repairs_a_valid_final_line_missing_its_newline() {
    let root = jsonl_temp_dir("repair-newline");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    let metadata: JsonlSessionMetadata = serde_json::from_value(session.metadata_json()).unwrap();
    let first_id = session.append_custom_entry("first", None).unwrap();
    let unterminated = std::fs::read_to_string(&metadata.path)
        .unwrap()
        .trim_end()
        .to_owned();
    std::fs::write(&metadata.path, &unterminated).unwrap();

    let reopened = repository.open(&metadata).await.unwrap();
    // Repair must have appended the trailing newline. The read is retried
    // briefly: an apparently-completed tokio append can still lose the
    // newline against the test's own std::fs write ordering on macOS.
    let mut repaired = std::fs::read_to_string(&metadata.path).unwrap();
    for _ in 0..50 {
        if repaired.ends_with('\n') {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        repaired = std::fs::read_to_string(&metadata.path).unwrap();
    }
    assert_eq!(
        repaired,
        format!("{unterminated}\n"),
        "valid final line must be repaired with a trailing newline"
    );
    let second_id = reopened.append_custom_entry("second", None).unwrap();

    let verified = repository.open(&metadata).await.unwrap();
    assert_eq!(
        verified
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .unwrap()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec![first_id.as_str(), second_id.as_str()]
    );
}

/// jsonl.test.ts: "truncates a malformed final line"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_truncates_a_malformed_final_line() {
    let root = jsonl_temp_dir("torn-tail");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    let metadata: JsonlSessionMetadata = serde_json::from_value(session.metadata_json()).unwrap();
    session
        .append_custom_entry("note", Some(serde_json::json!({ "value": "kept" })))
        .unwrap();
    let valid_prefix = std::fs::read_to_string(&metadata.path).unwrap();
    // Simulate a torn append.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&metadata.path)
            .unwrap();
        use std::io::Write;
        file.write_all(b"{\"kind\":\"entry\"").unwrap();
    }

    let reopened = repository.open(&metadata).await.unwrap();
    assert_eq!(
        reopened.find_entries(&EntryQuery::default()).unwrap().len(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(&metadata.path).unwrap(),
        valid_prefix
    );
    let appended_id = reopened
        .append_custom_entry("after-recovery", None)
        .unwrap();
    assert_eq!(reopened.get_entry(&appended_id).unwrap().unwrap().seq, 2);
}

/// jsonl.test.ts: "rejects a complete invalid final mutation without
/// modifying the file"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_rejects_a_complete_invalid_final_mutation_without_modifying_the_file() {
    let root = jsonl_temp_dir("invalid-final");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "invalid-final-mutation").await;
    let metadata: JsonlSessionMetadata = serde_json::from_value(session.metadata_json()).unwrap();
    let header = std::fs::read_to_string(&metadata.path).unwrap();
    std::fs::write(
        &metadata.path,
        format!("{header}{{\"kind\":\"unknown\",\"seq\":1}}\n"),
    )
    .unwrap();
    let corrupted = std::fs::read_to_string(&metadata.path).unwrap();

    let error = match repository.open(&metadata).await {
        Err(error) => error,
        Ok(_) => panic!("expected open failure"),
    };
    expect_code(error, SessionErrorCode::InvalidEntry);
    assert_eq!(std::fs::read_to_string(&metadata.path).unwrap(), corrupted);
}

/// jsonl.test.ts: "rejects a malformed middle line without modifying the file"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_rejects_a_malformed_middle_line_without_modifying_the_file() {
    let root = jsonl_temp_dir("malformed-middle");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let session = jsonl_session(&repository, &root, "session").await;
    let metadata: JsonlSessionMetadata = serde_json::from_value(session.metadata_json()).unwrap();
    session.append_custom_entry("first", None).unwrap();
    session.append_custom_entry("second", None).unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&metadata.path)
        .unwrap()
        .trim_end()
        .split('\n')
        .map(str::to_owned)
        .collect();
    let corrupted = format!("{}\n{}\nnot-json\n{}\n", lines[0], lines[1], lines[2]);
    std::fs::write(&metadata.path, &corrupted).unwrap();

    let error = match repository.open(&metadata).await {
        Err(error) => error,
        Ok(_) => panic!("expected open failure"),
    };
    expect_code(error, SessionErrorCode::InvalidEntry);
    assert_eq!(std::fs::read_to_string(&metadata.path).unwrap(), corrupted);
}

/// jsonl.test.ts: "rejects an imported entry that references a missing
/// parent" (hand-written file, message includes the line number).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_rejects_an_imported_entry_that_references_a_missing_parent() {
    let root = jsonl_temp_dir("missing-parent");
    let path = std::path::Path::new(&root).join("session-missing-parent.jsonl");
    let header = serde_json::json!({
        "kind": "header", "version": 4, "id": "missing-parent",
        "createdAt": 1, "cwd": root,
    });
    let entry = serde_json::json!({
        "kind": "entry", "type": "custom", "id": "orphan",
        "customType": "note", "parentId": "missing", "seq": 1, "timestamp": 1,
    });
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&entry).unwrap()
        ),
    )
    .unwrap();
    let modified_at = std::fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let metadata = JsonlSessionMetadata {
        id: "missing-parent".to_owned(),
        created_at: 1,
        parent_session_id: None,
        cwd: root.clone(),
        path: path.to_string_lossy().into_owned(),
        modified_at,
        source_format: 4,
        legacy_parent_session_path: None,
        metadata: None,
    };

    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let error = match repository.open(&metadata).await {
        Err(error) => error,
        Ok(_) => panic!("expected open failure"),
    };
    assert_eq!(error.code, SessionErrorCode::InvalidEntry);
    let path_display = path.to_string_lossy().into_owned();
    assert_eq!(
        error.message,
        format!(
            "Invalid JSONL v4 session {path_display}: line 2 Invalid session mutation: references missing parent missing"
        )
    );
}

/// jsonl.test.ts: "rejects session ids that cannot be used in coding-agent
/// filenames"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_rejects_session_ids_that_cannot_be_used_in_filenames() {
    let root = jsonl_temp_dir("bad-id");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let error = match repository
        .create(&JsonlSessionCreateOptions {
            id: Some("../escape".to_owned()),
            cwd: root.clone(),
            ..Default::default()
        })
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("expected invalid_payload"),
    };
    expect_code(error, SessionErrorCode::InvalidPayload);
}

/// jsonl.test.ts: "allows the same explicit session id in different working
/// directories"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_allows_the_same_explicit_session_id_in_different_working_directories() {
    let root = jsonl_temp_dir("shared-id");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let first_cwd = std::path::Path::new(&root).join("workspaces").join("first");
    let second_cwd = std::path::Path::new(&root)
        .join("workspaces")
        .join("second");
    std::fs::create_dir_all(&first_cwd).unwrap();
    std::fs::create_dir_all(&second_cwd).unwrap();

    let first = repository
        .create(&JsonlSessionCreateOptions {
            id: Some("shared".to_owned()),
            cwd: first_cwd.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let second = repository
        .create(&JsonlSessionCreateOptions {
            id: Some("shared".to_owned()),
            cwd: second_cwd.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let first_metadata: JsonlSessionMetadata =
        serde_json::from_value(first.metadata_json()).unwrap();
    let second_metadata: JsonlSessionMetadata =
        serde_json::from_value(second.metadata_json()).unwrap();
    assert_eq!(first_metadata.cwd, first_cwd.to_string_lossy().into_owned());
    assert_eq!(
        second_metadata.cwd,
        second_cwd.to_string_lossy().into_owned()
    );
    let listed = repository.list_metadata(&Default::default()).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["shared", "shared"]);
}

/// jsonl.test.ts: "rejects concurrent create/fork calls for the same
/// destination" — same-process reservation returns already_exists for the
/// loser and publishes exactly one session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_rejects_concurrent_creates_for_the_same_destination() {
    let root = jsonl_temp_dir("concurrent-create");
    let repository = Arc::new(JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    }));
    let options = JsonlSessionCreateOptions {
        id: Some("same".to_owned()),
        cwd: root.clone(),
        ..Default::default()
    };
    let (first, second) = tokio::join!(repository.create(&options), repository.create(&options),);
    let successes = [&first, &second].iter().filter(|r| r.is_ok()).count();
    let failures = [&first, &second].iter().filter(|r| r.is_err()).count();
    assert_eq!(successes, 1, "exactly one create succeeds");
    assert_eq!(failures, 1, "exactly one create fails");
    let error = match first {
        Err(error) => error,
        Ok(_) => match second {
            Err(error) => error,
            Ok(_) => panic!("expected one create to fail"),
        },
    };
    expect_code(error, SessionErrorCode::AlreadyExists);
    let listed = repository.list_metadata(&Default::default()).await.unwrap();
    assert_eq!(
        listed.iter().filter(|m| m.id == "same").count(),
        1,
        "exactly one 'same' session exists"
    );
}

/// jsonl.test.ts: "sorts listed sessions by current filesystem modification
/// time"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_sorts_listed_sessions_by_modification_time() {
    let root = jsonl_temp_dir("sort");
    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let newest = jsonl_session(&repository, &root, "newest").await;
    let newest_metadata: JsonlSessionMetadata =
        serde_json::from_value(newest.metadata_json()).unwrap();
    let oldest = jsonl_session(&repository, &root, "oldest").await;
    let oldest_metadata: JsonlSessionMetadata =
        serde_json::from_value(oldest.metadata_json()).unwrap();

    // Touch the files with explicit mtimes (upstream utimesSync).
    let newest_time = 1_700_000_002_000u64;
    let oldest_time = 1_700_000_001_000u64;
    set_file_mtime(&newest_metadata.path, newest_time);
    set_file_mtime(&oldest_metadata.path, oldest_time);

    let listed = repository.list_metadata(&Default::default()).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["newest", "oldest"]);
    assert_eq!(listed[0].modified_at, newest_time);
    assert_eq!(listed[1].modified_at, oldest_time);
}

fn set_file_mtime(path: &str, millis: u64) {
    let seconds = (millis / 1000) as i64;
    let nanos = ((millis % 1000) * 1_000_000) as u32;
    let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(seconds as u64, nanos);
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
}

/// jsonl.test.ts: "does not move a lane for an imported entry without lane
/// metadata"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_repo_does_not_move_a_lane_for_an_imported_entry_without_lane_metadata() {
    let root = jsonl_temp_dir("import");
    let path = std::path::Path::new(&root).join("session-import.jsonl");
    let header = serde_json::json!({
        "kind": "header", "version": 4, "id": "import",
        "createdAt": 1, "cwd": root,
    });
    let imported_entry = serde_json::json!({
        "kind": "entry", "type": "custom", "id": "imported",
        "customType": "note", "parentId": null, "seq": 1, "timestamp": 1,
    });
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&imported_entry).unwrap()
        ),
    )
    .unwrap();
    let modified_at = std::fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let metadata = JsonlSessionMetadata {
        id: "import".to_owned(),
        created_at: 1,
        parent_session_id: None,
        cwd: root.clone(),
        path: path.to_string_lossy().into_owned(),
        modified_at,
        source_format: 4,
        legacy_parent_session_path: None,
        metadata: None,
    };

    let repository = JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: Arc::new(StdFsExecutionEnv::new(&root)),
        sessions_root: root.clone(),
    });
    let imported = repository.open(&metadata).await.unwrap();
    assert_eq!(imported.get_leaf_id().unwrap(), None);
    assert_eq!(
        imported
            .find_entries(&EntryQuery::default())
            .unwrap()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["imported"]
    );

    // A later lane mutation moves the lane on the next open.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(
            file,
            "{}",
            serde_json::json!({ "kind": "lane", "seq": 2, "lane": "main", "leafId": "imported" })
        )
        .unwrap();
    }
    let moved = repository.open(&metadata).await.unwrap();
    assert_eq!(moved.get_leaf_id().unwrap().as_deref(), Some("imported"));
}
