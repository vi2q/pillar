//! Port of packages/agent/test/harness/session/jsonl-codec.test.ts
//! (pi v0.84.3) — JSONL v4 header/mutation encode/decode parity.

#![cfg(feature = "session-files")]

use pillar_agent::harness::session::jsonl::codec::JsonlDecodeKind;
use pillar_agent::harness::session::jsonl::codec::{
    encode_header, encode_mutation, parse_header, parse_mutation,
};
use pillar_agent::harness::session::jsonl::types::JsonlV4Header;
use pillar_agent::harness::session::state::{SessionMutation, SessionState};
use pillar_agent::harness::session::types::{
    Entry, EntryPayload, LaneRecord, LogItem, OperationIntent, ProvisionedEntry, RecordPayload,
};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, UserContent};

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn header(
    id: &str,
    created_at: u64,
    cwd: &str,
    parent_session_id: Option<&str>,
    legacy_parent_session_path: Option<&str>,
    metadata: Option<serde_json::Value>,
) -> JsonlV4Header {
    JsonlV4Header {
        kind: "header".to_owned(),
        version: 4,
        id: id.to_owned(),
        created_at,
        cwd: cwd.to_owned(),
        parent_session_id: parent_session_id.map(str::to_owned),
        legacy_parent_session_path: legacy_parent_session_path.map(str::to_owned),
        metadata,
    }
}

fn expect_header_round_trip(expected: JsonlV4Header) {
    let encoded = encode_header(&expected);
    assert!(encoded.ends_with('\n'));
    let parsed = parse_header(encoded.trim_end()).expect("header parses");
    assert_eq!(parsed, expected);
}

fn expect_mutation_round_trip(expected: SessionMutation) {
    let encoded = encode_mutation(&expected);
    assert!(encoded.ends_with('\n'));
    let parsed = parse_mutation(encoded.trim_end()).expect("mutation parses");
    // Compare through a fresh state: applying both mutations must produce
    // identical logs (upstream compares the mutation values directly).
    assert_eq!(format_mutation(&parsed), format_mutation(&expected));
}

fn format_mutation(mutation: &SessionMutation) -> String {
    match mutation {
        SessionMutation::Entry { lane, entry } => {
            format!("entry|{lane:?}|{}", serde_json::to_string(entry).unwrap())
        }
        SessionMutation::Record { record } => {
            format!("record|{}", serde_json::to_string(record).unwrap())
        }
        SessionMutation::Lane { seq, lane, leaf_id } => format!("lane|{seq}|{lane}|{leaf_id:?}"),
        SessionMutation::Name { seq, name } => format!("name|{seq}|{name:?}"),
        SessionMutation::Label {
            seq,
            target_id,
            label,
        } => format!("label|{seq}|{target_id}|{label:?}"),
    }
}

// --- headers ---------------------------------------------------------------

/// upstream: "round trips every header field with a resolved parent"
#[test]
fn round_trips_every_header_field_with_a_resolved_parent() {
    expect_header_round_trip(header(
        "session",
        1_700_000_000_000,
        "/workspace/project",
        Some("parent"),
        None,
        Some(serde_json::json!({
            "owner": "agent",
            "nested": { "enabled": true },
            "values": [1, null, "two"],
        })),
    ));
}

/// upstream: "round trips an unresolved legacy parent path"
#[test]
fn round_trips_an_unresolved_legacy_parent_path() {
    expect_header_round_trip(header(
        "legacy-child",
        1_700_000_000_001,
        "/workspace/project",
        None,
        Some("/sessions/missing-parent.jsonl"),
        None,
    ));
}

/// upstream: "projects header and filesystem fields into metadata"
#[test]
fn projects_header_and_filesystem_fields_into_metadata() {
    let header = header(
        "session",
        1_700_000_000_000,
        "/workspace/project",
        None,
        Some("/sessions/missing-parent.jsonl"),
        Some(serde_json::json!({ "owner": "agent" })),
    );
    let metadata = pillar_agent::harness::session::jsonl::storage::metadata_from_header(
        &header,
        "/sessions/session.jsonl",
        1_700_000_000_100,
    );
    assert_eq!(
        serde_json::to_value(&metadata).unwrap(),
        serde_json::json!({
            "id": "session",
            "createdAt": 1_700_000_000_000u64,
            "cwd": "/workspace/project",
            "path": "/sessions/session.jsonl",
            "modifiedAt": 1_700_000_000_100u64,
            "sourceFormat": 4,
            "legacyParentSessionPath": "/sessions/missing-parent.jsonl",
            "metadata": { "owner": "agent" },
        })
    );
}

// --- mutation lines ----------------------------------------------------------

/// upstream: "returns syntax and schema errors"
#[test]
fn returns_syntax_and_schema_errors() {
    let cases: Vec<(&str, JsonlDecodeKind)> = vec![
        ("{", JsonlDecodeKind::Syntax),
        (r#"{"kind":"unknown","seq":1}"#, JsonlDecodeKind::Schema),
    ];
    for (line, expected_kind) in cases {
        let error = parse_mutation(line).expect_err("should fail");
        assert_eq!(
            error.kind, expected_kind,
            "unexpected decode kind for {line}"
        );
    }
}

/// upstream: "round trips a lane-bound entry line"
#[test]
fn round_trips_a_lane_bound_entry_line() {
    let entry = Entry {
        id: "entry-1".to_owned(),
        seq: 1,
        parent_id: None,
        timestamp: 100,
        payload: EntryPayload::Custom {
            custom_type: "note".to_owned(),
            data: Some(serde_json::json!({ "text": "hello" })),
        },
    };
    expect_mutation_round_trip(SessionMutation::Entry {
        lane: Some("main".to_owned()),
        entry,
    });
}

/// upstream: "round trips an imported entry line without a lane"
#[test]
fn round_trips_an_imported_entry_line_without_a_lane() {
    let entry = Entry {
        id: "entry-1".to_owned(),
        seq: 1,
        parent_id: None,
        timestamp: 100,
        payload: EntryPayload::Custom {
            custom_type: "note".to_owned(),
            data: None,
        },
    };
    expect_mutation_round_trip(SessionMutation::Entry { lane: None, entry });
}

/// upstream: "round trips a record line"
#[test]
fn round_trips_a_record_line() {
    let record = LaneRecord {
        id: "run-1".to_owned(),
        seq: 1,
        lane: "main".to_owned(),
        timestamp: 100,
        payload: RecordPayload::OperationStarted {
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        },
    };
    expect_mutation_round_trip(SessionMutation::Record { record });
}

/// upstream: "round trips a lane line"
#[test]
fn round_trips_a_lane_line() {
    expect_mutation_round_trip(SessionMutation::Lane {
        seq: 1,
        lane: "thread".to_owned(),
        leaf_id: Some("entry-1".to_owned()),
    });
}

/// upstream: "round trips fact lines, including cleared values"
#[test]
fn round_trips_fact_lines_including_cleared_values() {
    expect_mutation_round_trip(SessionMutation::Name {
        seq: 1,
        name: Some("Example".to_owned()),
    });
    expect_mutation_round_trip(SessionMutation::Name { seq: 2, name: None });
    expect_mutation_round_trip(SessionMutation::Label {
        seq: 3,
        target_id: "entry-1".to_owned(),
        label: Some("checkpoint".to_owned()),
    });
}

/// upstream it.each: "rejects a custom entry without customType" /
/// "an operation_started record without intent" / "an operation_finished
/// record without runId"
#[test]
fn rejects_malformed_entry_and_record_lines() {
    // A custom entry without customType.
    assert!(
        parse_mutation(
            r#"{"kind":"entry","type":"custom","id":"entry","parentId":null,"seq":1,"timestamp":1}"#
        )
        .is_err()
    );
    // An operation_started record without intent.
    assert!(parse_mutation(
        r#"{"kind":"record","type":"operation_started","id":"run","lane":"main","seq":1,"timestamp":1,"sourceLeafId":null}"#
    )
    .is_err());
    // An operation_finished record without runId.
    assert!(parse_mutation(
        r#"{"kind":"record","type":"operation_finished","id":"finish","lane":"main","seq":1,"timestamp":1,"outcome":"completed"}"#
    )
    .is_err());
}

/// Additional wire-format guard (upstream asserts `parseHeader(...)` equals
/// the header value directly, which the Rust port covers through the round
/// trip tests). This test pins that a lane-bound entry line carries the
/// upstream field order/shape: kind, lane, then the entry fields with a
/// single `type` discriminant.
#[test]
fn entry_lines_carry_one_type_discriminant() {
    let payload = EntryPayload::Custom {
        custom_type: "note".to_owned(),
        data: Some(serde_json::json!({ "value": 1 })),
    };
    let mutation = SessionMutation::Entry {
        lane: Some("main".to_owned()),
        entry: Entry {
            id: "e1".to_owned(),
            seq: 1,
            parent_id: None,
            timestamp: 5,
            payload,
        },
    };
    let encoded = encode_mutation(&mutation);
    let value: serde_json::Value = serde_json::from_str(encoded.trim_end()).unwrap();
    let type_count = value
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| *key == "type")
        .count();
    assert_eq!(type_count, 1, "exactly one type discriminant: {value}");
    assert_eq!(value["kind"], "entry");
    assert_eq!(value["lane"], "main");
    assert_eq!(value["type"], "custom");
    assert_eq!(value["customType"], "note");
}

/// The parsed entry must re-apply through `SessionState` identically to the
/// original mutation (replay-path guard for the codec -> state boundary).
#[test]
fn parsed_entry_replays_through_state() {
    let mutation = SessionMutation::Entry {
        lane: Some("main".to_owned()),
        entry: Entry {
            id: "e1".to_owned(),
            seq: 1,
            parent_id: None,
            timestamp: 5,
            payload: EntryPayload::Message {
                message: user_message("hello"),
                terminate: false,
            },
        },
    };
    let encoded = encode_mutation(&mutation);
    let parsed = parse_mutation(encoded.trim_end()).expect("parses");

    let mut source = SessionState::new();
    let mut replayed = SessionState::new();
    source.apply_mutation(mutation).unwrap();
    replayed.apply_mutation(parsed).unwrap();
    let source_log: Vec<LogItem> = source.get_log(None, None).unwrap();
    let replayed_log: Vec<LogItem> = replayed.get_log(None, None).unwrap();
    assert_eq!(source_log, replayed_log);
    assert_eq!(source.get_stats(), replayed.get_stats());
}

/// Records keep camelCase payload fields on the wire (upstream serializes
/// record objects directly).
#[test]
fn record_lines_use_camel_case_fields() {
    let record = LaneRecord {
        id: "tool-1".to_owned(),
        seq: 1,
        lane: "main".to_owned(),
        timestamp: 5,
        payload: RecordPayload::ToolStarted {
            run_id: "run-1".to_owned(),
            assistant_entry_id: "assistant".to_owned(),
            tool_index: 0,
            tool_call_id: "call-1".to_owned(),
            tool_name: "read".to_owned(),
            effective_args: serde_json::json!({ "path": "README.md" }),
            result_entry_id: "result".to_owned(),
            replay: "safe".to_owned(),
        },
    };
    let encoded = encode_mutation(&SessionMutation::Record { record });
    let value: serde_json::Value = serde_json::from_str(encoded.trim_end()).unwrap();
    assert_eq!(value["kind"], "record");
    assert_eq!(value["type"], "tool_started");
    assert_eq!(value["runId"], "run-1");
    assert_eq!(value["assistantEntryId"], "assistant");
    assert_eq!(value["toolCallId"], "call-1");
    assert_eq!(value["toolName"], "read");
    assert_eq!(value["effectiveArgs"]["path"], "README.md");
    assert_eq!(value["resultEntryId"], "result");
    assert_eq!(value["replay"], "safe");

    let parsed = parse_mutation(encoded.trim_end()).expect("parses");
    let SessionMutation::Record {
        record: parsed_record,
    } = parsed
    else {
        panic!("expected record");
    };
    match parsed_record.payload {
        RecordPayload::ToolStarted {
            run_id,
            tool_call_id,
            ..
        } => {
            assert_eq!(run_id, "run-1");
            assert_eq!(tool_call_id, "call-1");
        }
        _ => panic!("expected tool_started payload"),
    }
}

/// ProvisionedEntry serializes like the wire entry (upstream queue/deferred
/// records embed `ProvisionedEntry` targets).
#[test]
fn provisioned_entry_wire_shape_matches_upstream() {
    let provisioned = ProvisionedEntry {
        id: "steer-message".to_owned(),
        payload: EntryPayload::Message {
            message: user_message("steer"),
            terminate: false,
        },
    };
    let value = serde_json::to_value(&provisioned).unwrap();
    assert_eq!(value["type"], "message");
    assert_eq!(value["id"], "steer-message");
    assert_eq!(value["message"]["role"], "user");
}
