//! Parity tests for the JSON wire projection of session events (upstream
//! packages/coding-agent/src/modes/json-event.ts).

use pillar_agent::types::{AgentMessage, BranchSummaryMessage};
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Message, StopReason, ToolResultMessage,
    Usage, UsageCost, UserContent,
};
use pillar_coding_agent::core::agent_session_class::AgentSessionEvent;
use pillar_coding_agent::modes::json_event::to_json_event;
use serde_json::{Value, json};

fn usage() -> Usage {
    Usage {
        input: 3,
        output: 5,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 8,
        cost: UsageCost::default(),
    }
}

fn assistant(content: Vec<Content>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        response_model: None,
        usage: usage(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

fn assistant_message(content: Vec<Content>) -> AgentMessage {
    AgentMessage::Message(Message::Assistant(Box::new(assistant(content))))
}

fn text_event(event: AssistantMessageEvent) -> AgentSessionEvent {
    AgentSessionEvent::MessageUpdate {
        message: assistant_message(vec![Content::text("hello")]),
        assistant_message_event: Box::new(event),
    }
}

#[test]
fn message_update_text_delta_drops_partial_and_keeps_usage() {
    let event = text_event(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: " world".to_string(),
        partial: assistant(vec![Content::text("hello")]),
    });
    let value = to_json_event(&event).expect("json event");
    assert_eq!(value["type"], "message_update");
    assert_eq!(value["usage"]["input"], 3);
    assert_eq!(value["usage"]["totalTokens"], 8);
    let assistant_event = &value["assistantMessageEvent"];
    assert_eq!(assistant_event["type"], "text_delta");
    assert_eq!(assistant_event["contentIndex"], 0);
    assert_eq!(assistant_event["delta"], " world");
    assert!(
        assistant_event.get("partial").is_none(),
        "partial stripped: {assistant_event}"
    );
}

#[test]
fn message_update_toolcall_start_adds_id_and_tool_name() {
    let partial = assistant(vec![
        Content::text("thinking"),
        Content::tool_call("call-1", "bash", json!({ "command": "ls" })),
    ]);
    let event = AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(partial.clone()))),
        assistant_message_event: Box::new(AssistantMessageEvent::ToolcallStart {
            content_index: 1,
            partial,
        }),
    };
    let value = to_json_event(&event).expect("json event");
    let assistant_event = &value["assistantMessageEvent"];
    assert_eq!(assistant_event["type"], "toolcall_start");
    assert_eq!(assistant_event["contentIndex"], 1);
    assert_eq!(assistant_event["id"], "call-1");
    assert_eq!(assistant_event["toolName"], "bash");
    assert!(assistant_event.get("partial").is_none());
}

#[test]
fn message_update_toolcall_start_rejects_non_tool_content() {
    let partial = assistant(vec![Content::text("hello")]);
    let event = AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(partial.clone()))),
        assistant_message_event: Box::new(AssistantMessageEvent::ToolcallStart {
            content_index: 0,
            partial,
        }),
    };
    let error = to_json_event(&event).expect_err("not a tool call");
    assert!(error.contains("is not a tool call"), "{error}");
}

#[test]
fn message_update_requires_an_assistant_message() {
    let event = AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::User {
            content: UserContent::Text("hi".to_string()),
            timestamp: 1,
        }),
        assistant_message_event: Box::new(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".to_string(),
            partial: assistant(vec![Content::text("x")]),
        }),
    };
    let error = to_json_event(&event).expect_err("not assistant");
    assert!(error.contains("not an assistant message"), "{error}");
}

#[test]
fn lifecycle_and_tool_events_use_camel_case_wire_keys() {
    let cases: Vec<(AgentSessionEvent, Value)> = vec![
        (
            AgentSessionEvent::AgentStart,
            json!({ "type": "agent_start" }),
        ),
        (
            AgentSessionEvent::TurnStart,
            json!({ "type": "turn_start" }),
        ),
        (
            AgentSessionEvent::AgentSettled,
            json!({ "type": "agent_settled" }),
        ),
        (
            AgentSessionEvent::QueueUpdate {
                steering: vec!["a".to_string()],
                follow_up: vec!["b".to_string()],
            },
            json!({ "type": "queue_update", "steering": ["a"], "followUp": ["b"] }),
        ),
        (
            AgentSessionEvent::ThinkingLevelChanged {
                level: "high".to_string(),
            },
            json!({ "type": "thinking_level_changed", "level": "high" }),
        ),
        (
            AgentSessionEvent::CompactionStart {
                reason: "threshold",
            },
            json!({ "type": "compaction_start", "reason": "threshold" }),
        ),
        (
            AgentSessionEvent::ToolExecutionStart {
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                args: json!({ "command": "ls" }),
            },
            json!({
                "type": "tool_execution_start",
                "toolCallId": "call-1",
                "toolName": "bash",
                "args": { "command": "ls" },
            }),
        ),
        (
            AgentSessionEvent::ToolExecutionEnd {
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                result: json!({ "ok": true }),
                is_error: false,
            },
            json!({
                "type": "tool_execution_end",
                "toolCallId": "call-1",
                "toolName": "bash",
                "result": { "ok": true },
                "isError": false,
            }),
        ),
        (
            AgentSessionEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 1000,
                error_message: "overloaded".to_string(),
            },
            json!({
                "type": "auto_retry_start",
                "attempt": 1,
                "maxAttempts": 3,
                "delayMs": 1000,
                "errorMessage": "overloaded",
            }),
        ),
        (
            AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt: 3,
                final_error: Some("boom".to_string()),
            },
            json!({
                "type": "auto_retry_end",
                "success": false,
                "attempt": 3,
                "finalError": "boom",
            }),
        ),
        (
            AgentSessionEvent::SessionInfoChanged { name: None },
            json!({ "type": "session_info_changed", "name": null }),
        ),
        (
            AgentSessionEvent::SummarizationRetryFinished,
            json!({ "type": "summarization_retry_finished" }),
        ),
        (
            AgentSessionEvent::BashExecutionUpdate {
                id: Some("b-1".to_string()),
                delta: "partial".to_string(),
            },
            json!({ "type": "bash_execution_update", "id": "b-1", "delta": "partial" }),
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(to_json_event(&event).expect("json event"), expected);
    }
}

#[test]
fn summarization_retry_attempt_start_maps_both_sources() {
    use pillar_coding_agent::core::agent_session_class::SummarizationRetrySource;

    let branch = AgentSessionEvent::SummarizationRetryAttemptStart {
        source: SummarizationRetrySource::BranchSummary,
    };
    assert_eq!(
        to_json_event(&branch).unwrap(),
        json!({ "type": "summarization_retry_attempt_start", "source": "branchSummary" })
    );

    let compaction = AgentSessionEvent::SummarizationRetryAttemptStart {
        source: SummarizationRetrySource::Compaction { reason: "overflow" },
    };
    assert_eq!(
        to_json_event(&compaction).unwrap(),
        json!({
            "type": "summarization_retry_attempt_start",
            "source": "compaction",
            "reason": "overflow",
        })
    );
}

#[test]
fn message_events_serialize_role_tagged_messages() {
    let event = AgentSessionEvent::AgentEnd {
        messages: vec![
            AgentMessage::Message(Message::User {
                content: UserContent::Text("hi".to_string()),
                timestamp: 1,
            }),
            AgentMessage::BranchSummary(Box::new(BranchSummaryMessage {
                summary: "s".to_string(),
                from_id: "e-1".to_string(),
                timestamp: 2,
            })),
        ],
        will_retry: false,
    };
    let value = to_json_event(&event).expect("json event");
    assert_eq!(value["type"], "agent_end");
    assert_eq!(value["willRetry"], false);
    assert_eq!(value["messages"][0]["role"], "user");
    assert_eq!(value["messages"][1]["role"], "branchSummary");
}

#[test]
fn turn_end_serializes_tool_results() {
    let event = AgentSessionEvent::TurnEnd {
        message: assistant_message(vec![Content::text("done")]),
        tool_results: vec![ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 3,
        }],
    };
    let value = to_json_event(&event).expect("json event");
    assert_eq!(value["type"], "turn_end");
    assert_eq!(value["message"]["role"], "assistant");
    assert_eq!(value["toolResults"][0]["toolCallId"], "call-1");
}

#[test]
fn entry_appended_serializes_the_full_entry() {
    use pillar_coding_agent::core::messages::CodingAgentMessage;
    use pillar_coding_agent::core::session_entries::{
        SessionEntry, SessionEntryBase, SessionMessageEntry,
    };

    let entry = SessionEntry::Message(SessionMessageEntry {
        base: SessionEntryBase {
            id: "e1".to_string(),
            parent_id: Some("e0".to_string()),
            timestamp: 1_700_000_000_000,
        },
        message: CodingAgentMessage::Base(Message::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: 1_700_000_000_000,
        }),
    });
    let wire = to_json_event(&AgentSessionEvent::EntryAppended { entry }).expect("wire");
    assert_eq!(wire["type"], json!("entry_appended"));
    assert_eq!(wire["entry"]["type"], json!("message"));
    assert_eq!(wire["entry"]["id"], json!("e1"));
    assert_eq!(wire["entry"]["parentId"], json!("e0"));
    // The canonical entry shape, not just the base fields.
    assert_eq!(wire["entry"]["message"]["role"], json!("user"));
    assert_eq!(wire["entry"]["message"]["content"], json!("hello"));
}
