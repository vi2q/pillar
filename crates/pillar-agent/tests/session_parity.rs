//! Port of packages/agent/test/harness/session tests (pi v0.84.3):
//! context.test.ts (3 cases), memory.test.ts (id generator case), and the
//! backend conformance suite (conformance.ts cases runnable against the
//! in-memory repo; JSONL-backend cases land with the jsonl port).

use std::sync::Arc;

use pillar_agent::harness::session::context::{SessionContextBuildOptions, build_session_context};
use pillar_agent::harness::session::memory::{
    InMemorySessionRepo, ProvisionedEntry, ProvisionedRecord, Session, SessionCreateOptions,
};
use pillar_agent::harness::session::types::{
    BranchBounds, Entry, EntryOrder, EntryPayload, EntryQuery, ForkOptions, ForkPosition,
    OperationIntent, RecordPayload, RecordQuery, SessionError, SessionErrorCode,
};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, UserContent};

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![Content::text(text)],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Default::default(),
            stop_reason: pillar_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    )))
}

fn entry(id: &str, parent_id: Option<&str>, seq: u64, payload: EntryPayload) -> Entry {
    Entry {
        id: id.to_owned(),
        seq,
        parent_id: parent_id.map(str::to_owned),
        timestamp: seq,
        payload,
    }
}

fn expect_code(error: SessionError, code: SessionErrorCode) {
    assert_eq!(
        error.code, code,
        "expected {code:?}, got {error:?} ({})",
        error.message
    );
}

// --- context.test.ts -------------------------------------------------------

#[test]
fn starts_at_the_latest_compaction_and_materializes_its_retained_tail() {
    let entries = vec![
        entry(
            "old",
            None,
            1,
            EntryPayload::Message {
                message: user_message("old"),
                terminate: false,
            },
        ),
        entry(
            "compact",
            Some("old"),
            2,
            EntryPayload::Compaction {
                summary: "summary".to_owned(),
                retained_tail: vec![user_message("retained"), assistant_message("answer")],
                tokens_before: 100,
                details: None,
                usage: None,
            },
        ),
        entry(
            "model",
            Some("compact"),
            3,
            EntryPayload::ModelChange {
                provider: "openai".to_owned(),
                model_id: "gpt-5".to_owned(),
            },
        ),
        entry(
            "thinking",
            Some("model"),
            4,
            EntryPayload::ThinkingLevelChange {
                thinking_level: "high".to_owned(),
            },
        ),
        entry(
            "tail",
            Some("thinking"),
            5,
            EntryPayload::Message {
                message: user_message("tail"),
                terminate: false,
            },
        ),
    ];

    let context = build_session_context(&entries, &SessionContextBuildOptions::default());
    let roles: Vec<&str> = context.messages.iter().map(|m| m.role_name()).collect();
    assert_eq!(
        roles,
        vec!["compactionSummary", "user", "assistant", "user"]
    );
    assert_eq!(
        context.model,
        Some(pillar_agent::harness::session::context::SessionModelRef {
            provider: "openai".to_owned(),
            model_id: "gpt-5".to_owned(),
        })
    );
    assert_eq!(context.thinking_level, "high");
}

#[test]
fn applies_caller_transforms_after_the_compaction_boundary() {
    let entries = vec![
        entry(
            "old",
            None,
            1,
            EntryPayload::Message {
                message: user_message("old"),
                terminate: false,
            },
        ),
        entry(
            "compact",
            Some("old"),
            2,
            EntryPayload::Compaction {
                summary: "summary".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 100,
                details: None,
                usage: None,
            },
        ),
        entry(
            "branch",
            Some("compact"),
            3,
            EntryPayload::BranchSummary {
                from_id: "abandoned".to_owned(),
                summary: "branch summary".to_owned(),
                details: None,
                usage: None,
            },
        ),
        entry(
            "tail",
            Some("branch"),
            4,
            EntryPayload::Message {
                message: user_message("tail"),
                terminate: false,
            },
        ),
    ];

    let mut options = SessionContextBuildOptions::default();
    options.entry_transforms.push(Arc::new(|entries: &[Entry]| {
        entries
            .iter()
            .filter(|candidate| candidate.kind() != "compaction")
            .cloned()
            .collect()
    }));
    let context = build_session_context(&entries, &options);
    let roles: Vec<&str> = context.messages.iter().map(|m| m.role_name()).collect();
    assert_eq!(roles, vec!["branchSummary", "user"]);
}

#[test]
fn projects_custom_entries_and_omits_deferred_assistant_handles() {
    let mut deferred_assistant = pillar_ai::types::AssistantMessage {
        content: Vec::new(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: pillar_ai::types::StopReason::Deferred,
        deferred: Some(pillar_ai::types::DeferredHandle {
            provider: "openai".into(),
            model_id: "gpt-5".to_owned(),
            api: "openai-responses".into(),
            id: "response-1".to_owned(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        }),
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    deferred_assistant.usage = Default::default();
    let entries = vec![
        entry(
            "user",
            None,
            1,
            EntryPayload::Message {
                message: user_message("hello"),
                terminate: false,
            },
        ),
        entry(
            "deferred",
            Some("user"),
            2,
            EntryPayload::Message {
                message: AgentMessage::Message(Message::Assistant(Box::new(deferred_assistant))),
                terminate: false,
            },
        ),
        entry(
            "custom",
            Some("deferred"),
            3,
            EntryPayload::Custom {
                custom_type: "note".to_owned(),
                data: Some(serde_json::json!("project me")),
            },
        ),
    ];

    let mut options = SessionContextBuildOptions::default();
    options.entry_projectors.insert(
        "note".to_owned(),
        Arc::new(|custom: &Entry| {
            let Some(EntryPayload::Custom { data, .. }) = Some(&custom.payload) else {
                return None;
            };
            let text = data.as_ref().and_then(|value| value.as_str()).unwrap_or("");
            Some(vec![user_message(&format!("note: {text}"))])
        }),
    );
    let context = build_session_context(&entries, &options);
    let roles: Vec<&str> = context.messages.iter().map(|m| m.role_name()).collect();
    assert_eq!(roles, vec!["user", "user"]);
    let second = context.messages[1].as_base_message();
    let Message::User { content, .. } = second else {
        panic!("expected user message");
    };
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    let Some(Content::Text { text, .. }) = blocks.first() else {
        panic!("expected text");
    };
    assert_eq!(text, "note: project me");
}

// --- memory.test.ts --------------------------------------------------------

#[test]
fn uses_one_injectable_id_generator_across_lane_views() {
    let counter = std::cell::Cell::new(0u32);
    let counter_for_fn = std::rc::Rc::new(&counter);
    // The shared generator must be Send+Sync; use an atomic instead.
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter_for_gen = Arc::clone(&counter);
    let _ = counter_for_fn;
    let session = Session::new(Box::new(
        pillar_agent::harness::session::memory::InMemorySessionStorage::new(
            pillar_agent::harness::session::types::SessionMetadata {
                id: "session".to_owned(),
                created_at: 1,
                parent_session_id: None,
            },
        ),
    ))
    .with_id_generator(Box::new(move || {
        let next = counter_for_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        format!("generated-{next}")
    }));

    let main_id = session
        .append_custom_entry("note", None)
        .expect("append main");
    session.create_lane("thread", Some(&main_id)).expect("lane");
    let thread_id = session
        .append_custom_entry("note", None)
        .expect("append thread");

    assert_eq!(main_id, "generated-1");
    assert_eq!(thread_id, "generated-2");
}

// --- conformance.ts (in-memory backend cases) ------------------------------

fn repo() -> InMemorySessionRepo {
    InMemorySessionRepo::new()
}

fn create_session(repo: &InMemorySessionRepo, id: &str) -> Session {
    repo.create(Some(SessionCreateOptions {
        id: Some(id.to_owned()),
        parent_session_id: None,
    }))
    .expect("create session")
}

fn append_message_entry(session: &Session, id: &str, text: &str) -> Entry {
    session
        .append_entry(
            ProvisionedEntry {
                id: id.to_owned(),
                payload: EntryPayload::Message {
                    message: user_message(text),
                    terminate: false,
                },
            },
            "main",
        )
        .expect("append entry")
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
    ProvisionedRecord {
        id: id.to_owned(),
        lane: lane.to_owned(),
        payload: RecordPayload::OperationStarted {
            source_leaf_id: None,
            intent,
        },
    }
}

fn entry_ids(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

/// conformance: "assigns parents and one sequence across every mutation"
#[test]
fn assigns_parents_and_one_sequence_across_every_mutation() {
    let repository = repo();
    let session = create_session(&repository, "session");
    let root = append_message_entry(&session, "root", "root");
    session.create_lane("thread", Some("root")).expect("lane");
    let child = session
        .append_entry(
            ProvisionedEntry {
                id: "child".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!({"value": 1})),
                },
            },
            "thread",
        )
        .expect("append child");
    let record = session
        .append_record(operation_started("run", "thread", "run"))
        .expect("append record");
    session.set_name(Some("Example".to_owned())).expect("name");
    session
        .set_label("root", Some("checkpoint".to_owned()))
        .expect("label");
    session.move_lane("main", Some("child")).expect("move");

    assert_eq!(root.parent_id, None);
    assert_eq!(root.seq, 1);
    assert_eq!(child.parent_id.as_deref(), Some("root"));
    assert_eq!(child.seq, 3);
    assert_eq!(record.seq, 4);
    for timestamp in [root.timestamp, child.timestamp, record.timestamp] {
        assert!(timestamp > 0, "storage-assigned timestamps must be Unix ms");
    }
    let log_kinds: Vec<(&str, u64)> = session
        .get_log(&Default::default())
        .expect("log")
        .iter()
        .map(|item| match item {
            pillar_agent::harness::session::types::LogItem::Entry { seq, .. } => ("entry", *seq),
            pillar_agent::harness::session::types::LogItem::Record { seq, .. } => ("record", *seq),
            pillar_agent::harness::session::types::LogItem::Lane { seq, .. } => ("lane", *seq),
            pillar_agent::harness::session::types::LogItem::Name { seq, .. }
            | pillar_agent::harness::session::types::LogItem::Label { seq, .. } => ("fact", *seq),
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
    let lanes = session.get_lanes().expect("lanes");
    assert_eq!(lanes[0].lane, "main");
    assert_eq!(lanes[0].leaf_id.as_deref(), Some("child"));
    assert_eq!(lanes[1].lane, "thread");
    assert_eq!(lanes[1].leaf_id.as_deref(), Some("child"));
}

/// conformance: "commits records and lane moves as separate mutations"
#[test]
fn commits_records_and_lane_moves_as_separate_mutations() {
    let repository = repo();
    let session = create_session(&repository, "session");
    let _root = append_message_entry(&session, "root", "root");
    let finished = session
        .append_record(ProvisionedRecord {
            id: "finish".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationFinished {
                run_id: "run".to_owned(),
                outcome: pillar_agent::harness::session::types::OperationOutcome::Completed,
                error: None,
            },
        })
        .expect("append record");

    assert_eq!(finished.seq, 2);
    let lanes = session.get_lanes().expect("lanes");
    assert_eq!(lanes[0].leaf_id.as_deref(), Some("root"));
    session.move_lane("main", None).expect("move");
    let lanes = session.get_lanes().expect("lanes");
    assert_eq!(lanes[0].leaf_id, None);

    let error = session
        .move_lane("main", Some("missing"))
        .expect_err("not found");
    expect_code(error, SessionErrorCode::NotFound);
    assert_eq!(session.find_records(&Default::default()).unwrap().len(), 1);
}

/// conformance: "rejects duplicate ids without changing state"
#[test]
fn rejects_duplicate_ids_without_changing_state() {
    let repository = repo();
    let session = create_session(&repository, "session");
    append_message_entry(&session, "shared", "root");
    let error = session
        .append_record(operation_started("shared", "main", "run"))
        .expect_err("duplicate");
    expect_code(error, SessionErrorCode::AlreadyExists);
    session
        .append_record(operation_started("run", "main", "run"))
        .expect("run");
    let error = session
        .append_entry(
            ProvisionedEntry {
                id: "run".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: None,
                },
            },
            "main",
        )
        .expect_err("duplicate entry");
    expect_code(error, SessionErrorCode::AlreadyExists);
    let log = session.get_log(&Default::default()).unwrap();
    let seqs: Vec<u64> = log.iter().map(|item| item.seq()).collect();
    assert_eq!(seqs, vec![1, 2]);
}

/// conformance: "isolates lanes while sharing the tree"
#[test]
fn isolates_lanes_while_sharing_the_tree() {
    let repository = repo();
    let session = create_session(&repository, "session");
    append_message_entry(&session, "root", "root");
    session.create_lane("thread", Some("root")).expect("lane");
    append_message_entry(&session, "main-child", "main");
    session
        .append_entry(
            ProvisionedEntry {
                id: "thread-child".to_owned(),
                payload: EntryPayload::Message {
                    message: user_message("thread"),
                    terminate: false,
                },
            },
            "thread",
        )
        .expect("thread child");

    let lanes = session.get_lanes().unwrap();
    assert_eq!(lanes[0].leaf_id.as_deref(), Some("main-child"));
    assert_eq!(lanes[1].leaf_id.as_deref(), Some("thread-child"));

    let mut query = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        start: Some("main-child".to_owned()),
        ..Default::default()
    };
    let main_branch = session
        .find_entries_on_branch(&query, &BranchBounds::default())
        .unwrap();
    assert_eq!(entry_ids(&main_branch), vec!["root", "main-child"]);
    query.start = Some("thread-child".to_owned());
    let thread_branch = session
        .find_entries_on_branch(&query, &BranchBounds::default())
        .unwrap();
    assert_eq!(entry_ids(&thread_branch), vec!["root", "thread-child"]);
}

/// conformance: "rejects invalid queries before empty reads"
#[test]
fn rejects_invalid_queries_before_empty_reads() {
    let repository = repo();
    let session = create_session(&repository, "invalid-queries");
    session.create_lane("thread", None).expect("lane");

    let zero = EntryQuery {
        limit: Some(0),
        ..Default::default()
    };
    let error = session.find_entries(&zero).unwrap_err();
    expect_code(error, SessionErrorCode::InvalidQuery);
    let error = session
        .find_records(&RecordQuery {
            limit: Some(0),
            ..Default::default()
        })
        .unwrap_err();
    expect_code(error, SessionErrorCode::InvalidQuery);
    // operationKind requires type operation_started
    let error = session
        .find_records(&RecordQuery {
            operation_kind: Some("run".to_owned()),
            ..Default::default()
        })
        .unwrap_err();
    expect_code(error, SessionErrorCode::InvalidQuery);
    let error = session.find_open_operations("main", Some(0)).unwrap_err();
    expect_code(error, SessionErrorCode::InvalidQuery);
}

/// conformance: "supports bounded filtered and cursor-based queries"
#[test]
fn supports_bounded_filtered_and_cursor_based_queries() {
    let repository = repo();
    let session = create_session(&repository, "session");
    append_message_entry(&session, "root", "root");
    session
        .append_entry(
            ProvisionedEntry {
                id: "old-note".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!(1)),
                },
            },
            "main",
        )
        .unwrap();
    session
        .append_entry(
            ProvisionedEntry {
                id: "compact".to_owned(),
                payload: EntryPayload::Compaction {
                    summary: "summary".to_owned(),
                    retained_tail: Vec::new(),
                    tokens_before: 10,
                    details: None,
                    usage: None,
                },
            },
            "main",
        )
        .unwrap();
    session
        .append_entry(
            ProvisionedEntry {
                id: "new-note".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!(2)),
                },
            },
            "main",
        )
        .unwrap();
    session
        .append_entry(
            ProvisionedEntry {
                id: "tail".to_owned(),
                payload: EntryPayload::Message {
                    message: assistant_message("tail"),
                    terminate: false,
                },
            },
            "main",
        )
        .unwrap();

    let entries = session.find_entries(&EntryQuery::default()).unwrap();
    assert_eq!(
        entry_ids(&entries),
        vec!["tail", "new-note", "compact", "old-note", "root"]
    );

    let paged = session
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            after_seq: Some(2),
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entry_ids(&paged), vec!["compact", "new-note"]);

    let notes = session
        .find_entries(&EntryQuery {
            custom_type: Some("note".to_owned()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entry_ids(&notes), vec!["new-note", "old-note"]);

    let branch_notes = session
        .find_entries_on_branch(
            &EntryQuery {
                custom_type: Some("note".to_owned()),
                limit: Some(1),
                start: Some("tail".to_owned()),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .unwrap();
    assert_eq!(entry_ids(&branch_notes), vec!["new-note"]);

    // stopAtType is exclusive-inclusive: stops at compaction, matches only
    // message entries.
    let stopped = session
        .find_entries_on_branch(
            &EntryQuery {
                kind: Some("message".to_owned()),
                start: Some("tail".to_owned()),
                ..Default::default()
            },
            &BranchBounds {
                stop_at_kind: Some("compaction".to_owned()),
                stop_at_id: None,
            },
        )
        .unwrap();
    assert_eq!(entry_ids(&stopped), vec!["tail"]);

    let error = session
        .find_entries(&EntryQuery {
            limit: Some(0),
            ..Default::default()
        })
        .unwrap_err();
    expect_code(error, SessionErrorCode::InvalidQuery);
    let error = session
        .find_entries_on_branch(
            &EntryQuery {
                start: Some("missing".to_owned()),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .unwrap_err();
    expect_code(error, SessionErrorCode::NotFound);
}

/// conformance: "tracks and enforces one open operation per lane"
#[test]
fn tracks_and_enforces_one_open_operation_per_lane() {
    let repository = repo();
    let session = create_session(&repository, "session");
    assert!(
        session
            .find_open_operations("main", Some(2))
            .unwrap()
            .is_empty()
    );

    let first = session
        .append_record(operation_started("first", "main", "run"))
        .unwrap();
    assert_eq!(
        session.find_open_operations("main", Some(2)).unwrap().len(),
        1
    );
    let error = session
        .append_record(operation_started("second", "main", "run"))
        .unwrap_err();
    expect_code(error, SessionErrorCode::Storage);
    assert_eq!(
        session.find_open_operations("main", Some(2)).unwrap().len(),
        1
    );

    session
        .append_record(ProvisionedRecord {
            id: "finish-first".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationFinished {
                run_id: first.id.clone(),
                outcome: pillar_agent::harness::session::types::OperationOutcome::Completed,
                error: None,
            },
        })
        .unwrap();
    assert!(
        session
            .find_open_operations("main", Some(2))
            .unwrap()
            .is_empty()
    );
}

/// conformance: "keeps latest-value facts and computes ledger statistics
/// across lanes"
#[test]
fn keeps_latest_value_facts_and_computes_ledger_statistics_across_lanes() {
    let repository = repo();
    let session = create_session(&repository, "session");
    let mut assistant = pillar_ai::types::AssistantMessage {
        content: vec![Content::text("answer")],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: pillar_ai::types::StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    assistant.usage = pillar_ai::types::Usage {
        input: 10,
        output: 5,
        cache_read: 3,
        cache_write: 2,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 20,
        cost: pillar_ai::types::UsageCost {
            input: 1.0,
            output: 2.0,
            cache_read: 3.0,
            cache_write: 4.0,
            total: 10.0,
        },
    };
    session
        .append_entry(
            ProvisionedEntry {
                id: "user".to_owned(),
                payload: EntryPayload::Message {
                    message: user_message("question"),
                    terminate: false,
                },
            },
            "main",
        )
        .unwrap();
    session
        .append_entry(
            ProvisionedEntry {
                id: "assistant".to_owned(),
                payload: EntryPayload::Message {
                    message: AgentMessage::Message(Message::Assistant(Box::new(assistant.clone()))),
                    terminate: false,
                },
            },
            "main",
        )
        .unwrap();
    session
        .append_record(ProvisionedRecord {
            id: "assistant-usage".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::UsageRecord {
                usage: assistant.usage.clone(),
                cause: "assistant".to_owned(),
                run_id: Some("run".to_owned()),
                entry_id: Some("assistant".to_owned()),
                attempt: Some(1),
                tool_call_id: None,
                stop_reason: Some("stop".to_owned()),
                details: None,
            },
        })
        .unwrap();
    session.create_lane("thread", Some("assistant")).unwrap();
    session
        .append_record(ProvisionedRecord {
            id: "correction".to_owned(),
            lane: "thread".to_owned(),
            payload: RecordPayload::UsageRecord {
                // divergence: upstream adjustments carry negative usage; the
                // port's Usage counters are unsigned, so the correction is
                // recorded with zero deltas (stats assertions updated below).
                usage: pillar_ai::types::Usage {
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cache_write_1h: None,
                    reasoning: None,
                    total_tokens: 0,
                    cost: pillar_ai::types::UsageCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        total: 0.0,
                    },
                },
                cause: "adjustment".to_owned(),
                run_id: None,
                entry_id: None,
                attempt: None,
                tool_call_id: None,
                stop_reason: None,
                details: Some(serde_json::json!({"reason": "provider correction"})),
            },
        })
        .unwrap();
    session.set_name(Some("First".to_owned())).unwrap();
    session.set_name(Some("Second".to_owned())).unwrap();
    session.set_label("user", Some("keep".to_owned())).unwrap();
    session.set_label("user", None).unwrap();
    let error = session
        .set_label("missing", Some("checkpoint".to_owned()))
        .unwrap_err();
    expect_code(error, SessionErrorCode::NotFound);

    assert_eq!(session.get_name().unwrap().as_deref(), Some("Second"));
    assert_eq!(session.get_label("user").unwrap(), None);
    let stats = session.get_stats().unwrap();
    // divergence: upstream includes the negative adjustment (-2 input /
    // -2 total / -0.5 cost); the port's unsigned Usage records the
    // correction with zero deltas, so uncached stays 12 (10+2) instead of
    // upstream's 10, total 20 instead of 18, cost 10 instead of 9.5.
    assert_eq!(stats.message_count, 2);
    assert_eq!(stats.cached_tokens, 3.0);
    assert_eq!(stats.uncached_tokens, 12.0);
    assert_eq!(stats.total_tokens, 20.0);
    assert_eq!(stats.cost_total, 10.0);
}

/// conformance: "clears session names durably"
#[test]
fn clears_session_names_durably() {
    let repository = repo();
    let session = create_session(&repository, "session");
    session.set_name(Some("Temporary".to_owned())).unwrap();
    session.set_name(None).unwrap();

    assert_eq!(session.get_name().unwrap(), None);
    let metadata = session.get_metadata().unwrap();
    let reopened = repository.open(&metadata).unwrap();
    assert_eq!(reopened.get_name().unwrap(), None);

    let fork = repository
        .fork(
            &metadata,
            Some((
                ForkOptions::Branch {
                    entry_id: None,
                    position: None,
                },
                SessionCreateOptions {
                    id: Some("fork".to_owned()),
                    parent_session_id: None,
                },
            )),
        )
        .unwrap();
    assert_eq!(fork.get_name().unwrap(), None);
}

/// conformance: "creates lists and opens sessions"
#[test]
fn creates_lists_and_opens_sessions() {
    let repository = repo();
    let session = create_session(&repository, "one");
    let entry_id = session.append_message(user_message("persisted")).unwrap();
    let metadata = session.get_metadata().unwrap();

    let listed = repository.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, metadata.id);
    assert_eq!(listed[0].created_at, metadata.created_at);
    assert_eq!(listed[0].parent_session_id, metadata.parent_session_id);
    let reopened = repository.open(&metadata).unwrap();
    let entries = reopened.find_entries(&EntryQuery::default()).unwrap();
    assert_eq!(entry_ids(&entries), vec![entry_id.as_str()]);
    let error = match repository.create(Some(SessionCreateOptions {
        id: Some("one".to_owned()),
        parent_session_id: None,
    })) {
        Err(error) => error,
        Ok(_) => panic!("expected already_exists"),
    };
    expect_code(error, SessionErrorCode::AlreadyExists);
}

/// conformance: "deletes sessions idempotently"
#[test]
fn deletes_sessions_idempotently() {
    let repository = repo();
    let session = create_session(&repository, "one");
    let metadata = session.get_metadata().unwrap();

    repository.delete(&metadata).unwrap();
    let error = match repository.open(&metadata) {
        Err(error) => error,
        Ok(_) => panic!("expected not_found after delete"),
    };
    expect_code(error, SessionErrorCode::NotFound);
    repository.delete(&metadata).unwrap();
}

/// conformance: "forks one branch with selected facts and no records"
#[test]
fn forks_one_branch_with_selected_facts_and_no_records() {
    let repository = repo();
    let source = create_session(&repository, "source");
    let root = source.append_message(user_message("root")).unwrap();
    let shared = source
        .append_entry(
            ProvisionedEntry {
                id: "shared".to_owned(),
                payload: EntryPayload::Message {
                    message: assistant_message("shared"),
                    terminate: false,
                },
            },
            "main",
        )
        .unwrap();
    source.create_lane("thread", Some(&shared.id)).unwrap();
    let thread_child = source
        .append_entry(
            ProvisionedEntry {
                id: "thread-child".to_owned(),
                payload: EntryPayload::Message {
                    message: user_message("thread"),
                    terminate: false,
                },
            },
            "thread",
        )
        .unwrap();
    let main_child = source.append_message(user_message("main")).unwrap();
    source.set_name(Some("Source".to_owned())).unwrap();
    source
        .set_label(&shared.id, Some("copied".to_owned()))
        .unwrap();
    source
        .set_label(&thread_child.id, Some("excluded".to_owned()))
        .unwrap();
    source
        .append_record(operation_started("run", "main", "run"))
        .unwrap();

    let fork = repository
        .fork(
            &source.get_metadata().unwrap(),
            Some((
                ForkOptions::Branch {
                    entry_id: Some(main_child.clone()),
                    position: Some(ForkPosition::At),
                },
                SessionCreateOptions {
                    id: Some("branch-fork".to_owned()),
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
        vec![root.as_str(), "shared", main_child.as_str()]
    );
    let lanes = fork.get_lanes().unwrap();
    assert_eq!(lanes.len(), 1);
    assert_eq!(lanes[0].lane, "main");
    assert_eq!(lanes[0].leaf_id.as_deref(), Some(main_child.as_str()));
    assert_eq!(fork.get_name().unwrap().as_deref(), Some("Source"));
    assert_eq!(
        fork.get_label(&shared.id).unwrap().as_deref(),
        Some("copied")
    );
    assert_eq!(fork.get_label(&thread_child.id).unwrap(), None);
    assert!(
        fork.find_records(&RecordQuery::default())
            .unwrap()
            .is_empty()
    );
    let stats = fork.get_stats().unwrap();
    assert_eq!(stats.message_count, 3);
    assert_eq!(stats.cached_tokens, 0.0);
    assert_eq!(stats.cost_total, 0.0);
    fork.append_message(user_message("after fork")).unwrap();
    assert_eq!(fork.get_stats().unwrap().message_count, 4);
    let metadata = fork.get_metadata().unwrap();
    assert_eq!(metadata.id, "branch-fork");
    assert_eq!(metadata.parent_session_id.as_deref(), Some("source"));
}

/// conformance: "forks before an entry without modifying the source"
#[test]
fn forks_before_an_entry_without_modifying_the_source() {
    let repository = repo();
    let source = create_session(&repository, "source");
    let root = source.append_message(user_message("root")).unwrap();
    let tail = source.append_message(user_message("tail")).unwrap();
    let fork = repository
        .fork(
            &source.get_metadata().unwrap(),
            Some((
                ForkOptions::Branch {
                    entry_id: Some(tail.clone()),
                    position: None,
                },
                SessionCreateOptions {
                    id: Some("fork".to_owned()),
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
    assert_eq!(entry_ids(&entries), vec![root.as_str()]);
    assert_eq!(fork.get_leaf_id().unwrap().as_deref(), Some(root.as_str()));
    assert_eq!(
        source.get_leaf_id().unwrap().as_deref(),
        Some(tail.as_str())
    );

    let error = match repository.fork(
        &source.get_metadata().unwrap(),
        Some((
            ForkOptions::Branch {
                entry_id: Some("missing".to_owned()),
                position: None,
            },
            SessionCreateOptions {
                id: Some("fork-missing".to_owned()),
                parent_session_id: None,
            },
        )),
    ) {
        Err(error) => error,
        Ok(_) => panic!("expected invalid_fork_target"),
    };
    expect_code(error, SessionErrorCode::InvalidForkTarget);
}

/// conformance: "validates the default fork target"
#[test]
fn validates_the_default_fork_target() {
    let repository = repo();
    let source = create_session(&repository, "source-with-custom-leaf");
    source.append_custom_entry("not-a-message", None).unwrap();

    let error = match repository.fork(
        &source.get_metadata().unwrap(),
        Some((
            ForkOptions::Branch {
                entry_id: None,
                position: None,
            },
            SessionCreateOptions {
                id: Some("fork".to_owned()),
                parent_session_id: None,
            },
        )),
    ) {
        Err(error) => error,
        Ok(_) => panic!("expected invalid_fork_target"),
    };
    expect_code(error, SessionErrorCode::InvalidForkTarget);
}

/// conformance: "appends provisioned entries with their existing ids"
#[test]
fn appends_provisioned_entries_with_their_existing_ids() {
    let repository = repo();
    let session = create_session(&repository, "session");
    let entry = session
        .append_entry(
            ProvisionedEntry {
                id: "provisioned".to_owned(),
                payload: EntryPayload::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!({"value": 1})),
                },
            },
            "main",
        )
        .unwrap();

    assert_eq!(entry.id, "provisioned");
    assert_eq!(entry.parent_id, None);
    assert_eq!(entry.seq, 1);
    assert_eq!(
        session.get_leaf_id().unwrap().as_deref(),
        Some("provisioned")
    );
}

/// conformance: "linearizes concurrent writes across two lanes"
#[test]
fn linearizes_concurrent_writes_across_two_lanes() {
    let repository = repo();
    let session = Arc::new(create_session(&repository, "session"));
    append_message_entry(&session, "root", "root");
    session.create_lane("thread", Some("root")).unwrap();

    let mut handles = Vec::new();
    for id in ["main-1", "thread-1", "main-2", "thread-2"] {
        let session = Arc::clone(&session);
        let lane = if id.starts_with("main") {
            "main"
        } else {
            "thread"
        };
        handles.push(std::thread::spawn(move || {
            session
                .append_entry(
                    ProvisionedEntry {
                        id: id.to_owned(),
                        payload: EntryPayload::Custom {
                            custom_type: "note".to_owned(),
                            data: None,
                        },
                    },
                    lane,
                )
                .unwrap()
        }));
    }
    let entries: Vec<Entry> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
    let unique_seqs: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique_seqs.len(), entries.len(), "seqs are unique");
    seqs.sort_unstable();
    let log = session.get_log(&Default::default()).unwrap();
    let log_seqs: Vec<u64> = log.iter().map(|item| item.seq()).collect();
    let mut sorted = log_seqs.clone();
    sorted.sort_unstable();
    assert_eq!(log_seqs, sorted);
}
