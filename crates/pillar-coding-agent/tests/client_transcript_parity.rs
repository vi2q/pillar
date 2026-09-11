//! Parity tests for the remote transcript projection (upstream
//! packages/coding-agent/test/client/transcript.test.ts).

use pillar_coding_agent::core::client_transcript::{
    apply_transcript_progress, apply_transcript_snapshot, create_transcript_state,
    select_transcript,
};
use pillar_protocol::schemas::{
    AssistantContent, AssistantStatus, AssistantTranscriptItem, DeltaKind, ModelRef,
    NonTerminalItem, NonTerminalItemKind, SessionPhase, SessionSnapshot, TerminalItem,
    TerminalItemKind, ThinkingLevel, ToolStatus, ToolTranscriptItem, TranscriptItem,
    TranscriptProgress, UserContent,
};
use serde_json::{Value, json};

fn model_ref() -> ModelRef {
    ModelRef {
        provider: "faux".to_string(),
        id: "faux-1".to_string(),
    }
}

fn text_part(text: &str) -> AssistantContent {
    AssistantContent::Text {
        text: text.to_string(),
    }
}

fn assistant_item(content: Vec<AssistantContent>, status: AssistantStatus) -> TranscriptItem {
    TranscriptItem::Assistant(AssistantTranscriptItem {
        id: "assistant-1".to_string(),
        content,
        model: model_ref(),
        response_model: None,
        usage: None,
        status,
        stop_reason: None,
        error_message: None,
        timestamp: 1,
    })
}

fn tool_item(input: Value, content: Vec<pillar_protocol::schemas::ToolContent>) -> TranscriptItem {
    TranscriptItem::Tool(ToolTranscriptItem {
        id: "tool-call-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        input,
        content,
        details: None,
        status: ToolStatus::Running,
        is_error: false,
        usage: None,
        timestamp: 2,
    })
}

fn snapshot(revision: u64, text: &str) -> SessionSnapshot {
    SessionSnapshot {
        id: "session-1".to_string(),
        name: None,
        cwd: "/workspace".to_string(),
        created_at: 1,
        updated_at: revision + 1,
        phase: SessionPhase::Turn,
        model: model_ref(),
        thinking_level: ThinkingLevel::Off,
        attached: true,
        locked: true,
        revision,
        transcript: vec![assistant_item(
            vec![text_part(text)],
            AssistantStatus::Streaming,
        )],
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    }
}

fn assistant_text(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("expected assistant item, got {other:?}"),
    }
}

fn assistant_input(item: &TranscriptItem) -> Value {
    match item {
        TranscriptItem::Assistant(assistant) => assistant
            .content
            .iter()
            .find_map(|part| match part {
                AssistantContent::ToolCall { input, .. } => Some(input.clone()),
                _ => None,
            })
            .expect("tool call content"),
        other => panic!("expected assistant item, got {other:?}"),
    }
}

fn non_terminal(item: TranscriptItem) -> NonTerminalItem {
    match item {
        TranscriptItem::Assistant(assistant) => NonTerminalItem {
            item: NonTerminalItemKind::Assistant(assistant),
        },
        TranscriptItem::Tool(tool) => NonTerminalItem {
            item: NonTerminalItemKind::Tool(tool),
        },
        TranscriptItem::User { .. } => panic!("user items are not non-terminal"),
    }
}

fn terminal(item: TranscriptItem) -> TerminalItem {
    match item {
        TranscriptItem::Assistant(assistant) => TerminalItem {
            item: TerminalItemKind::Assistant(assistant),
        },
        TranscriptItem::Tool(tool) => TerminalItem {
            item: TerminalItemKind::Tool(tool),
        },
        TranscriptItem::User { .. } => panic!("user items are not terminal"),
    }
}

fn delta(message_id: &str, content_index: u64, kind: DeltaKind, delta: &str) -> TranscriptProgress {
    TranscriptProgress::AssistantDelta {
        message_id: message_id.to_string(),
        content_index,
        kind,
        delta: delta.to_string(),
    }
}

#[test]
fn projects_progress_without_mutating_the_authoritative_snapshot() {
    let state = create_transcript_state(&snapshot(1, "saved"));
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::Text, " response"),
    );

    assert_eq!(
        assistant_text(&state.snapshot.transcript[0]),
        "saved",
        "snapshot stays authoritative"
    );
    assert_eq!(
        assistant_text(&select_transcript(&state)[0]),
        "saved response"
    );
}

#[test]
fn applies_streamed_tool_call_argument_deltas() {
    let mut base = snapshot(1, "saved");
    base.transcript = vec![assistant_item(
        vec![AssistantContent::ToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: Value::Null,
        }],
        AssistantStatus::Streaming,
    )];
    let state = create_transcript_state(&base);
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::ToolCall, "{\"command\":"),
    );
    assert_eq!(
        assistant_input(&select_transcript(&state)[0]),
        Value::String("{\"command\":".to_string())
    );

    // A full item update must not drop the accumulated argument buffer.
    let state = apply_transcript_progress(
        &state,
        &TranscriptProgress::ItemUpdated {
            item: non_terminal(base.transcript[0].clone()),
        },
    );
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::ToolCall, "\"pwd\"}"),
    );
    assert_eq!(
        assistant_input(&select_transcript(&state)[0]),
        json!({ "command": "pwd" })
    );
}

#[test]
fn appends_tool_call_deltas_to_a_partial_input_restored_from_a_snapshot() {
    let mut base = snapshot(1, "saved");
    base.transcript = vec![assistant_item(
        vec![AssistantContent::ToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: Value::String("{\"command\":".to_string()),
        }],
        AssistantStatus::Streaming,
    )];
    let state = create_transcript_state(&base);
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::ToolCall, "\"pwd\"}"),
    );
    assert_eq!(
        assistant_input(&select_transcript(&state)[0]),
        json!({ "command": "pwd" })
    );
}

#[test]
fn appends_transient_tool_progress_and_replaces_it_by_id() {
    let state = create_transcript_state(&snapshot(1, "saved"));
    let running = tool_item(json!({ "command": "printf hi" }), Vec::new());
    let state = apply_transcript_progress(
        &state,
        &TranscriptProgress::ItemStarted {
            item: running.clone(),
        },
    );
    let selected = select_transcript(&state);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[1], running);

    let with_content = tool_item(
        json!({ "command": "printf hi" }),
        vec![pillar_protocol::schemas::ToolContent::Text {
            text: "hi".to_string(),
        }],
    );
    let state = apply_transcript_progress(
        &state,
        &TranscriptProgress::ItemUpdated {
            item: non_terminal(with_content),
        },
    );
    let selected = select_transcript(&state);
    assert_eq!(selected.len(), 2, "replaced by id, not appended");
    assert_eq!(
        selected[1],
        tool_item(
            json!({ "command": "printf hi" }),
            vec![pillar_protocol::schemas::ToolContent::Text {
                text: "hi".to_string(),
            }],
        )
    );
}

#[test]
fn resets_revision_history_when_the_same_session_runtime_is_reacquired() {
    let _state = create_transcript_state(&snapshot(50, "old runtime"));
    let state = create_transcript_state(&snapshot(0, "new runtime"));

    assert_eq!(state.snapshot.revision, 0);
    assert_eq!(assistant_text(&select_transcript(&state)[0]), "new runtime");
}

#[test]
fn accepts_a_lower_revision_when_switching_to_a_different_session() {
    let state = create_transcript_state(&snapshot(50, "old session"));
    let mut other = snapshot(0, "new session");
    other.id = "session-2".to_string();
    let state = apply_transcript_snapshot(&state, &other);

    assert_eq!(state.snapshot.id, "session-2");
    assert_eq!(assistant_text(&select_transcript(&state)[0]), "new session");
}

#[test]
fn renders_accepted_steering_messages_from_authoritative_queued_state() {
    let mut base = snapshot(2, "saved");
    base.queued_steer_count = 1;
    base.queued_steer = vec![TranscriptItem::User {
        id: "user-steer".to_string(),
        content: vec![UserContent::Text {
            text: "adjust the approach".to_string(),
        }],
        timestamp: 2,
    }];
    let state = create_transcript_state(&base);

    let selected = select_transcript(&state);
    match selected.last().expect("steering item") {
        TranscriptItem::User { content, .. } => {
            assert_eq!(
                content,
                &vec![UserContent::Text {
                    text: "adjust the approach".to_string()
                }]
            );
        }
        other => panic!("expected user steer item, got {other:?}"),
    }
}

#[test]
fn a_newer_snapshot_is_authoritative_and_stale_snapshots_are_ignored() {
    let state = create_transcript_state(&snapshot(3, "new"));
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::Text, " transient"),
    );
    let state = apply_transcript_snapshot(&state, &snapshot(4, "authoritative"));
    let state = apply_transcript_snapshot(&state, &snapshot(2, "stale"));

    assert_eq!(state.snapshot.revision, 4);
    assert_eq!(
        assistant_text(&select_transcript(&state)[0]),
        "authoritative"
    );
}

// `terminal()` is exercised by the item-finished buffer cleanup below.
#[test]
fn item_finished_clears_tool_call_argument_buffers() {
    let mut base = snapshot(1, "saved");
    base.transcript = vec![assistant_item(
        vec![AssistantContent::ToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: Value::Null,
        }],
        AssistantStatus::Streaming,
    )];
    let state = create_transcript_state(&base);
    let state = apply_transcript_progress(
        &state,
        &delta("assistant-1", 0, DeltaKind::ToolCall, "{\"command\":"),
    );
    assert!(!state.tool_call_buffers.is_empty());

    let mut finished = assistant_item(
        vec![AssistantContent::ToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            input: json!({ "command": "pwd" }),
        }],
        AssistantStatus::Complete,
    );
    if let TranscriptItem::Assistant(assistant) = &mut finished {
        assistant.id = "assistant-1".to_string();
    }
    let state = apply_transcript_progress(
        &state,
        &TranscriptProgress::ItemFinished {
            item: terminal(finished),
        },
    );
    assert!(state.tool_call_buffers.is_empty());
}
