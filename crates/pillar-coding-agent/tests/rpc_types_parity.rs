//! Parity tests for the RPC JSON-lines protocol types (upstream
//! packages/coding-agent/src/modes/rpc/rpc-types.ts). Verifies the wire
//! tags and camelCase field names by round-tripping representative payloads.

use pillar_coding_agent::modes::rpc::rpc_types::{
    RpcCommand, RpcCommandEnvelope, RpcExtensionUiAnswer, RpcExtensionUiMethod,
    RpcExtensionUiRequest, RpcExtensionUiResponse, RpcResponse, RpcSessionState,
};
use serde_json::{Value, json};

fn command(value: Value) -> RpcCommandEnvelope {
    serde_json::from_value(value).expect("parse command")
}

fn round_trip_command(value: Value) -> Value {
    serde_json::to_value(serde_json::from_value::<RpcCommandEnvelope>(value).unwrap()).unwrap()
}

#[test]
fn parses_prompt_command() {
    let envelope = command(json!({ "type": "prompt", "message": "hi" }));
    assert_eq!(envelope.id, None);
    assert_eq!(
        envelope.command,
        RpcCommand::Prompt {
            message: "hi".to_string(),
            images: None,
            streaming_behavior: None,
        }
    );
}

#[test]
fn prompt_with_id_and_streaming_behavior_round_trips() {
    let value = json!({
        "id": "req-1",
        "type": "prompt",
        "message": "hi",
        "streamingBehavior": "steer",
    });
    assert_eq!(round_trip_command(value.clone()), value);
}

#[test]
fn parses_snake_case_command_tags() {
    let cases = [
        ("follow_up", json!({ "type": "follow_up", "message": "m" })),
        ("clear_queue", json!({ "type": "clear_queue" })),
        (
            "set_model",
            json!({ "type": "set_model", "provider": "faux", "modelId": "faux-1" }),
        ),
        (
            "set_thinking_level",
            json!({ "type": "set_thinking_level", "level": "high" }),
        ),
        (
            "new_session",
            json!({ "type": "new_session", "parentSession": "s-1" }),
        ),
        (
            "set_auto_compaction",
            json!({ "type": "set_auto_compaction", "enabled": true }),
        ),
        (
            "set_auto_retry",
            json!({ "type": "set_auto_retry", "enabled": false }),
        ),
        ("abort_retry", json!({ "type": "abort_retry" })),
        ("abort_bash", json!({ "type": "abort_bash" })),
        ("get_session_stats", json!({ "type": "get_session_stats" })),
        (
            "export_html",
            json!({ "type": "export_html", "outputPath": "/tmp/out.html" }),
        ),
        (
            "switch_session",
            json!({ "type": "switch_session", "sessionPath": "s.jsonl" }),
        ),
        ("fork", json!({ "type": "fork", "entryId": "e-1" })),
        ("clone", json!({ "type": "clone" })),
        ("get_fork_messages", json!({ "type": "get_fork_messages" })),
        (
            "get_entries",
            json!({ "type": "get_entries", "since": "e-1" }),
        ),
        ("get_tree", json!({ "type": "get_tree" })),
        (
            "get_last_assistant_text",
            json!({ "type": "get_last_assistant_text" }),
        ),
        (
            "set_session_name",
            json!({ "type": "set_session_name", "name": "n" }),
        ),
        ("get_messages", json!({ "type": "get_messages" })),
        ("get_commands", json!({ "type": "get_commands" })),
        (
            "get_available_models",
            json!({ "type": "get_available_models" }),
        ),
        (
            "get_available_thinking_levels",
            json!({ "type": "get_available_thinking_levels" }),
        ),
        ("cycle_model", json!({ "type": "cycle_model" })),
        (
            "cycle_thinking_level",
            json!({ "type": "cycle_thinking_level" }),
        ),
        (
            "set_steering_mode",
            json!({ "type": "set_steering_mode", "mode": "all" }),
        ),
        (
            "set_follow_up_mode",
            json!({ "type": "set_follow_up_mode", "mode": "one-at-a-time" }),
        ),
        (
            "compact",
            json!({ "type": "compact", "customInstructions": "focus" }),
        ),
        (
            "bash",
            json!({ "type": "bash", "command": "ls", "excludeFromContext": true }),
        ),
        ("abort", json!({ "type": "abort" })),
        ("get_state", json!({ "type": "get_state" })),
    ];
    for (tag, value) in cases {
        let round_tripped = round_trip_command(value.clone());
        assert_eq!(round_tripped, value, "round trip failed for {tag}");
    }
}

#[test]
fn omits_absent_optional_command_fields() {
    let value = json!({ "type": "prompt", "message": "hi" });
    assert_eq!(round_trip_command(value.clone()), value);
}

#[test]
fn parses_extension_ui_select_request() {
    let request: RpcExtensionUiRequest = serde_json::from_value(json!({
        "type": "extension_ui_request",
        "id": "ui-1",
        "method": "select",
        "title": "Pick",
        "options": ["a", "b"],
    }))
    .unwrap();
    assert_eq!(request.kind, "extension_ui_request");
    assert_eq!(request.id, "ui-1");
    assert_eq!(
        request.request,
        RpcExtensionUiMethod::Select {
            title: "Pick".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
            timeout: None,
        }
    );
}

#[test]
fn extension_ui_methods_round_trip() {
    let cases = [
        json!({ "type": "extension_ui_request", "id": "1", "method": "confirm", "title": "T", "message": "M", "timeout": 5 }),
        json!({ "type": "extension_ui_request", "id": "2", "method": "input", "title": "T", "placeholder": "P" }),
        json!({ "type": "extension_ui_request", "id": "3", "method": "editor", "title": "T", "prefill": "x" }),
        json!({ "type": "extension_ui_request", "id": "4", "method": "notify", "message": "hi", "notifyType": "warning" }),
        json!({ "type": "extension_ui_request", "id": "5", "method": "setStatus", "statusKey": "k", "statusText": "v" }),
        json!({ "type": "extension_ui_request", "id": "6", "method": "setWidget", "widgetKey": "w", "widgetLines": ["a"], "widgetPlacement": "aboveEditor" }),
        json!({ "type": "extension_ui_request", "id": "7", "method": "setTitle", "title": "T" }),
        json!({ "type": "extension_ui_request", "id": "8", "method": "set_editor_text", "text": "body" }),
    ];
    for value in cases {
        let parsed: RpcExtensionUiRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
}

#[test]
fn extension_ui_responses_round_trip() {
    let value = json!({ "type": "extension_ui_response", "id": "1", "value": "chosen" });
    let response: RpcExtensionUiResponse = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        response.answer,
        RpcExtensionUiAnswer::Value {
            value: "chosen".to_string()
        }
    );
    assert_eq!(serde_json::to_value(response).unwrap(), value);

    let confirmed = json!({ "type": "extension_ui_response", "id": "2", "confirmed": true });
    assert_eq!(
        serde_json::to_value(
            serde_json::from_value::<RpcExtensionUiResponse>(confirmed.clone()).unwrap()
        )
        .unwrap(),
        confirmed
    );

    let cancelled = json!({ "type": "extension_ui_response", "id": "3", "cancelled": true });
    assert_eq!(
        serde_json::to_value(
            serde_json::from_value::<RpcExtensionUiResponse>(cancelled.clone()).unwrap()
        )
        .unwrap(),
        cancelled
    );
}

#[test]
fn response_constructors_match_wire_shape() {
    let success = RpcResponse::success(Some("1".to_string()), "prompt", None);
    assert_eq!(
        serde_json::to_value(&success).unwrap(),
        json!({ "id": "1", "type": "response", "command": "prompt", "success": true })
    );
    let failure = RpcResponse::failure(None, "set_model", "boom");
    assert_eq!(
        serde_json::to_value(&failure).unwrap(),
        json!({ "type": "response", "command": "set_model", "success": false, "error": "boom" })
    );
}

#[test]
fn session_state_round_trips_camel_case() {
    let value = json!({
        "model": { "provider": "faux", "id": "faux-1" },
        "thinkingLevel": "high",
        "isStreaming": false,
        "isCompacting": false,
        "steeringMode": "one-at-a-time",
        "followUpMode": "one-at-a-time",
        "sessionFile": "/tmp/s.jsonl",
        "sessionId": "s-1",
        "sessionName": "named",
        "autoCompactionEnabled": true,
        "messageCount": 3,
        "pendingMessageCount": 0,
    });
    let state: RpcSessionState = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(state.session_id, "s-1");
    assert_eq!(state.thinking_level, "high");
    assert_eq!(serde_json::to_value(state).unwrap(), value);
}
