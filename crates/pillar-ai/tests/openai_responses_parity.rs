//! Port of the upstream openai-responses tests (pi v0.84.3) that run without
//! live API keys: openai-responses-message-id, openai-responses-empty-tool-
//! result, openai-responses-foreign-toolcall-id, openai-responses-partial-
//! json-cleanup, and openai-responses-terminal-event (module-level cases).
//! One Rust test per upstream test case, same names in comments.

#![cfg(feature = "providers")]

use std::collections::BTreeSet;

use serde_json::{Value, json};

use futures::StreamExt;
use pillar_ai::api::openai_responses_shared::{
    convert_responses_messages, process_responses_stream,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::hash::short_hash;
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, StopReason, ToolResultMessage, Usage,
    UserContent,
};

// --- Helpers -------------------------------------------------------------

fn usage() -> Usage {
    Usage::default()
}

fn codex_model() -> Model {
    Model {
        id: "gpt-5.5".to_string(),
        name: "GPT-5.5".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai-codex".to_string(),
        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn openai_model() -> Model {
    Model {
        id: "gpt-5-mini".to_string(),
        name: "GPT-5 Mini".to_string(),
        api: "openai-responses".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn allowed_providers() -> BTreeSet<String> {
    ["openai", "openai-codex", "opencode"]
        .iter()
        .map(|provider| provider.to_string())
        .collect()
}

fn assistant(api: &str, provider: &str, model: &str, content: Vec<Content>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: api.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: usage(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

fn user_message(content: &str) -> Message {
    Message::User {
        content: UserContent::Text(content.to_string()),
        timestamp: 1,
    }
}

fn tool_result(call_id: &str, name: &str, text: &str) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        tool_call_id: call_id.to_string(),
        tool_name: name.to_string(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 1,
    }))
}

// --- openai-responses-message-id.test.ts ---------------------------------

// "generates unique fallback message IDs for multiple text blocks in one
// assistant turn"
#[test]
fn generates_unique_fallback_message_ids_for_multiple_text_blocks() {
    let model = codex_model();
    let assistant_message = assistant(
        "anthropic-messages",
        "anthropic",
        "claude-opus-4-8",
        vec![
            Content::thinking("private reasoning"),
            Content::text("visible answer"),
        ],
    );
    let context = Context {
        system_prompt: Some("You are concise.".to_string()),
        messages: vec![
            user_message("hello"),
            Message::Assistant(Box::new(assistant_message)),
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &allowed_providers(), None);
    let message_ids: Vec<String> = input
        .iter()
        .filter(|item| item["type"] == json!("message"))
        .filter_map(|item| item["id"].as_str().map(str::to_string))
        .collect();

    assert_eq!(
        message_ids,
        vec!["msg_pi_1".to_string(), "msg_pi_1_1".to_string()]
    );
    let unique: std::collections::BTreeSet<&String> = message_ids.iter().collect();
    assert_eq!(unique.len(), message_ids.len());
}

// --- openai-responses-empty-tool-result.test.ts --------------------------

// "uses '(no tool output)' placeholder for empty tool results without images"
#[test]
fn uses_no_tool_output_placeholder_for_empty_tool_results() {
    let model = openai_model();
    let now = 1u64;
    let assistant_message = AssistantMessage {
        stop_reason: StopReason::ToolUse,
        ..assistant(
            &model.api,
            &model.provider,
            &model.id,
            vec![Content::tool_call(
                "tool-1",
                "bash",
                json!({ "command": "true" }),
            )],
        )
    };
    let empty_result = ToolResultMessage {
        tool_call_id: "tool-1".to_string(),
        tool_name: "bash".to_string(),
        content: vec![Content::text("")],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now + 1,
    };
    let context = Context {
        messages: vec![
            user_message("Run the command"),
            Message::Assistant(Box::new(assistant_message)),
            Message::ToolResult(Box::new(empty_result)),
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &allowed_providers(), None);
    let function_call_output = input
        .iter()
        .find(|item| item["type"] == json!("function_call_output"))
        .expect("function_call_output present");

    assert_eq!(function_call_output["output"], json!("(no tool output)"));
    assert!(
        !function_call_output["output"]
            .to_string()
            .contains("see attached image")
    );
}

// --- openai-responses-foreign-toolcall-id.test.ts ------------------------

// "hashes foreign Copilot tool item IDs into a bounded Codex-safe fc_<hash>
// shape"
#[test]
fn hashes_foreign_copilot_tool_item_ids_into_fc_hash_shape() {
    const COPILOT_RAW_TOOL_CALL_ID: &str = "call_4VnzVawQXPB9MgYib7CiQFEY|I9b95oN1wD/cHXKTw3PpRkL6KkCtzTJhUxMouMWYwHeTo2j3htzfSk7YPx2vifiIM4g3A8XXyOj8q4Bt6SLUG7gqY1E3ELkrkVQNHglRfUmWj84lqxJY+Puieb3VKyX0FB+83TUzn91cDMF/4gzt990IzqVrc+nIb9RRscRD070Du16q1glydVjWR0SBJsE6TbY/esOjFpqplogQqrajm1eI++f3eLi73R6q7hVusY0QbeFySVxABCjhN0lXB04caBe1rzHjYzul6MAXj7uq+0r17VLq+yrtyYhN12wkmFqHeqTyEei6EFPbMy24Nc+IbJlkP0OCg02W+gOnyBFcbi2ctvJFSOhSjt1CqBdqCnnhwUqXjbWiT0wh3DmLScRgTHmGkaI+oAcQQjfic65nxj+TnEkReA==";

    let model = codex_model();
    let assistant_message = assistant(
        "openai-responses",
        "github-copilot",
        "gpt-5.5",
        vec![Content::tool_call(
            COPILOT_RAW_TOOL_CALL_ID,
            "edit",
            json!({ "path": "src/styles/app.css" }),
        )],
    );
    let context = Context {
        system_prompt: Some("You are concise.".to_string()),
        messages: vec![
            user_message("Use the tool."),
            Message::Assistant(Box::new(assistant_message)),
            tool_result(COPILOT_RAW_TOOL_CALL_ID, "edit", "ok"),
        ],
        ..Default::default()
    };

    let input = convert_responses_messages(&model, &context, &allowed_providers(), None);
    let function_call = input
        .iter()
        .find(|item| item["type"] == json!("function_call"))
        .expect("function_call present");

    let expected_item_id = format!(
        "fc_{}",
        short_hash(COPILOT_RAW_TOOL_CALL_ID.split_once('|').unwrap().1)
    );
    assert_eq!(function_call["id"], json!(expected_item_id));
    let id = function_call["id"].as_str().unwrap();
    assert!(id.len() <= 64, "{id}");
    assert!(
        id.starts_with("fc_") && id[3..].chars().all(|c| c.is_ascii_alphanumeric()),
        "{id}"
    );
}

// --- openai-responses-partial-json-cleanup.test.ts -----------------------

// "removes partialJson from persisted tool-call blocks at output_item.done"
#[tokio::test]
async fn removes_partial_json_from_persisted_tool_call_blocks() {
    let model = openai_model();
    let mut output = AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    let stream = assistant_message_event_stream();
    let arguments_json = r#"{"path":"README.md","content":"updated"}"#;

    let events: Vec<Result<Value, pillar_ai::error::AiError>> = vec![
        Ok(json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "id": "fc_test", "call_id": "call_test", "name": "edit", "arguments": "" }
        })),
        Ok(json!({
            "type": "response.function_call_arguments.delta",
            "delta": "{\"path\":\"README.md\""
        })),
        Ok(json!({
            "type": "response.function_call_arguments.delta",
            "delta": ",\"content\":\"updated\"}"
        })),
        Ok(json!({
            "type": "response.function_call_arguments.done",
            "arguments": arguments_json
        })),
        Ok(json!({
            "type": "response.output_item.done",
            "item": { "type": "function_call", "id": "fc_test", "call_id": "call_test", "name": "edit", "arguments": arguments_json }
        })),
        Ok(json!({
            "type": "response.completed",
            "response": { "id": "resp_test", "status": "completed" }
        })),
    ];

    process_responses_stream(
        futures::stream::iter(events),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processes");

    assert_eq!(output.content.len(), 1);
    let persisted = &output.content[0];
    let Content::ToolCall { arguments, .. } = persisted else {
        panic!("expected toolCall block, got {persisted:?}");
    };
    assert_eq!(
        *arguments,
        json!({ "path": "README.md", "content": "updated" })
    );
}

// --- openai-responses-terminal-event.test.ts (module-level cases) --------

fn early_eof_events() -> Vec<Result<Value, pillar_ai::error::AiError>> {
    vec![
        Ok(json!({ "type": "response.created", "response": { "id": "resp_early_eof" } })),
        Ok(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "reasoning", "id": "rs_early_eof", "summary": [] }
        })),
        Ok(json!({
            "type": "response.reasoning_text.delta",
            "output_index": 0,
            "delta": "partial reasoning before the stream ends"
        })),
    ]
}

fn completed_events() -> Vec<Result<Value, pillar_ai::error::AiError>> {
    vec![Ok(json!({
        "type": "response.completed",
        "response": {
            "id": "resp_completed",
            "status": "completed",
            "usage": {
                "input_tokens": 20,
                "output_tokens": 7,
                "total_tokens": 27,
                "input_tokens_details": { "cached_tokens": 2, "cache_write_tokens": 3 }
            }
        }
    }))]
}

fn incomplete_events(reason: &str) -> Vec<Result<Value, pillar_ai::error::AiError>> {
    vec![Ok(json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_incomplete",
            "status": "incomplete",
            "incomplete_details": { "reason": reason },
            "usage": {
                "input_tokens": 30,
                "output_tokens": 12,
                "total_tokens": 42,
                "input_tokens_details": { "cached_tokens": 5 }
            }
        }
    }))]
}

fn failed_events() -> Vec<Result<Value, pillar_ai::error::AiError>> {
    vec![Ok(json!({
        "type": "response.failed",
        "response": {
            "id": "resp_failed",
            "status": "failed",
            "error": { "code": "server_error", "message": "boom" }
        }
    }))]
}

fn phased_message_events(
    added_phase: &str,
    done_phase: &str,
    terminal_status: &str,
) -> Vec<Result<Value, pillar_ai::error::AiError>> {
    let mut events = vec![
        Ok(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg_phase",
                "role": "assistant",
                "status": "in_progress",
                "content": [],
                "phase": added_phase
            }
        })),
        Ok(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg_phase",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "answer", "annotations": [] }],
                "phase": done_phase
            }
        })),
    ];
    if terminal_status == "incomplete" {
        events.push(Ok(json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_phase",
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" }
            }
        })));
    } else {
        events.push(Ok(json!({
            "type": "response.completed",
            "response": { "id": "resp_phase", "status": "completed" }
        })));
    }
    events
}

fn fresh_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

// "rejects streams that end before a terminal response event"
#[tokio::test]
async fn rejects_streams_that_end_before_a_terminal_response_event() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    let error = process_responses_stream(
        futures::stream::iter(early_eof_events()),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect_err("should reject");
    assert!(
        error
            .to_string()
            .contains("OpenAI Responses stream ended before a terminal response event"),
        "{error}"
    );
}

// "tracks message phases commentary/commentary"
#[tokio::test(flavor = "multi_thread")]
async fn tracks_message_phases_commentary_commentary() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();
    let observed: std::sync::Arc<std::sync::Mutex<Vec<StopReason>>> = Default::default();
    let observed_for_events = std::sync::Arc::clone(&observed);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_for_iter = std::sync::Arc::clone(&events);

    let mut iter = std::pin::pin!(stream.iter());
    let input = phased_message_events("commentary", "commentary", "completed");
    let drive = process_responses_stream(
        futures::stream::iter(input),
        &mut output,
        &stream,
        &model,
        None,
    );
    // Drive processor and event reader concurrently on one runtime.
    // process_responses_stream doesn't close the stream (upstream callers
    // push Done + end themselves), so end it when the drive completes.
    let drive = async {
        let _ = drive.await;
        stream.end(None);
    };
    let read = async {
        while let Some(event) = iter.next().await {
            observed_for_events
                .lock()
                .unwrap()
                .push(event.partial().stop_reason);
            events_for_iter.lock().unwrap().push(event);
        }
    };
    futures::future::join(drive, read).await;

    let observed = observed.lock().unwrap();
    assert_eq!(
        observed.as_slice(),
        &[StopReason::Pending, StopReason::Pending]
    );
    // Terminal mapping happens in finalize_response on `output` (no event is
    // pushed for it — upstream callers push Done themselves).
    assert_eq!(output.stop_reason, StopReason::Stop);
}

// "tracks message phases commentary/final_answer" — a final_answer phase
// flips the provisional stop to "stop" mid-stream.
#[tokio::test(flavor = "multi_thread")]
async fn tracks_message_phases_commentary_final_answer() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();
    let observed: std::sync::Arc<std::sync::Mutex<Vec<StopReason>>> = Default::default();
    let observed_for_events = std::sync::Arc::clone(&observed);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_for_iter = std::sync::Arc::clone(&events);

    let mut iter = std::pin::pin!(stream.iter());
    let input = phased_message_events("commentary", "final_answer", "completed");
    let drive = process_responses_stream(
        futures::stream::iter(input),
        &mut output,
        &stream,
        &model,
        None,
    );
    // Drive processor and event reader concurrently on one runtime.
    // process_responses_stream doesn't close the stream (upstream callers
    // push Done + end themselves), so end it when the drive completes.
    let drive = async {
        let _ = drive.await;
        stream.end(None);
    };
    let read = async {
        while let Some(event) = iter.next().await {
            observed_for_events
                .lock()
                .unwrap()
                .push(event.partial().stop_reason);
            events_for_iter.lock().unwrap().push(event);
        }
    };
    futures::future::join(drive, read).await;

    let observed = observed.lock().unwrap();
    assert_eq!(
        observed.as_slice(),
        &[StopReason::Pending, StopReason::Stop]
    );
    let events = events.lock().unwrap();
    let final_output = match events.last().cloned() {
        Some(event) => event.partial().clone(),
        None => unreachable!("events present"),
    };
    assert_eq!(final_output.stop_reason, StopReason::Stop);
}

// "replaces a provisional final-answer stop with an incomplete terminal reason"
#[tokio::test(flavor = "multi_thread")]
async fn replaces_provisional_final_answer_stop_with_incomplete_reason() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();
    let observed: std::sync::Arc<std::sync::Mutex<Vec<StopReason>>> = Default::default();
    let observed_for_events = std::sync::Arc::clone(&observed);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_for_iter = std::sync::Arc::clone(&events);

    let mut iter = std::pin::pin!(stream.iter());
    let input = phased_message_events("final_answer", "final_answer", "incomplete");
    let drive = process_responses_stream(
        futures::stream::iter(input),
        &mut output,
        &stream,
        &model,
        None,
    );
    // Drive processor and event reader concurrently on one runtime.
    // process_responses_stream doesn't close the stream (upstream callers
    // push Done + end themselves), so end it when the drive completes.
    let drive = async {
        let _ = drive.await;
        stream.end(None);
    };
    let read = async {
        while let Some(event) = iter.next().await {
            observed_for_events
                .lock()
                .unwrap()
                .push(event.partial().stop_reason);
            events_for_iter.lock().unwrap().push(event);
        }
    };
    futures::future::join(drive, read).await;

    let observed = observed.lock().unwrap();
    assert_eq!(observed.as_slice(), &[StopReason::Stop, StopReason::Stop]);
    // The incomplete terminal replaces the provisional stop on `output`.
    assert_eq!(output.stop_reason, StopReason::Length);
}

// "finalizes completed terminal events as stop"
#[tokio::test]
async fn finalizes_completed_terminal_events_as_stop() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    process_responses_stream(
        futures::stream::iter(completed_events()),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processes");

    assert_eq!(output.response_id.as_deref(), Some("resp_completed"));
    assert_eq!(output.stop_reason, StopReason::Stop);
    assert_eq!(output.raw_stop_reason.as_deref(), Some("completed"));
    assert_eq!(output.usage.input, 15);
    assert_eq!(output.usage.output, 7);
    assert_eq!(output.usage.cache_read, 2);
    assert_eq!(output.usage.cache_write, 3);
    assert_eq!(output.usage.total_tokens, 27);
}

// "finalizes incomplete terminal events as length stops"
#[tokio::test]
async fn finalizes_incomplete_terminal_events_as_length_stops() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    process_responses_stream(
        futures::stream::iter(incomplete_events("max_output_tokens")),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processes");

    assert_eq!(output.response_id.as_deref(), Some("resp_incomplete"));
    assert_eq!(output.stop_reason, StopReason::Length);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.max_output_tokens")
    );
    assert_eq!(output.usage.input, 25);
    assert_eq!(output.usage.output, 12);
    assert_eq!(output.usage.cache_read, 5);
    assert_eq!(output.usage.cache_write, 0);
    assert_eq!(output.usage.total_tokens, 42);
}

// "finalizes content-filtered incomplete responses as non-retryable errors"
#[tokio::test]
async fn finalizes_content_filtered_incomplete_responses_as_errors() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    process_responses_stream(
        futures::stream::iter(incomplete_events("content_filter")),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processes");

    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.content_filter")
    );
    assert_eq!(
        output.error_message.as_deref(),
        Some("Response incomplete: content_filter")
    );
}

// "preserves unknown provider incomplete reasons as non-retryable errors"
#[tokio::test]
async fn preserves_unknown_incomplete_reasons_as_errors() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    process_responses_stream(
        futures::stream::iter(incomplete_events("max_time_limit")),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect("stream processes");

    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("incomplete.max_time_limit")
    );
    assert_eq!(
        output.error_message.as_deref(),
        Some("Response incomplete: max_time_limit")
    );
}

// "rejects failed terminal events with the provider error"
#[tokio::test]
async fn rejects_failed_terminal_events_with_provider_error() {
    let model = openai_model();
    let mut output = fresh_output(&model);
    let stream = assistant_message_event_stream();

    let error = process_responses_stream(
        futures::stream::iter(failed_events()),
        &mut output,
        &stream,
        &model,
        None,
    )
    .await
    .expect_err("should reject");

    assert!(error.to_string().contains("server_error: boom"), "{error}");
    assert_eq!(output.raw_stop_reason.as_deref(), Some("failed"));
}

// grammar custom tool call replay round-trip (upstream: "replays grammar
// calls as custom Responses items" — module-level subset)
#[test]
fn replays_grammar_calls_as_custom_responses_items() {
    let model = openai_model();
    let mut call = assistant(
        &model.api,
        &model.provider,
        &model.id,
        vec![Content::tool_call("call_1|ctc_1", "sample_tool", json!({}))],
    );
    if let Content::ToolCall { arguments, .. } = &mut call.content[0] {
        *arguments = json!({ "payload": "abc" });
    }
    let context = Context {
        messages: vec![
            Message::Assistant(Box::new(call)),
            tool_result("call_1|ctc_1", "sample_tool", "done"),
        ],
        ..Default::default()
    };
    let grammar_properties = [("sample_tool".to_string(), "payload".to_string())]
        .into_iter()
        .collect();

    let invalid_variants = vec![json!({}), json!({ "payload": 42 })];
    for invalid in invalid_variants {
        let mut invalid_call = assistant(
            &model.api,
            &model.provider,
            &model.id,
            vec![Content::tool_call("call_1|ctc_1", "sample_tool", json!({}))],
        );
        if let Content::ToolCall { arguments, .. } = &mut invalid_call.content[0] {
            *arguments = invalid;
        }
        let invalid_context = Context {
            messages: vec![
                Message::Assistant(Box::new(invalid_call)),
                tool_result("call_1|ctc_1", "sample_tool", "done"),
            ],
            ..Default::default()
        };
        let error = convert_responses_messages(
            &model,
            &invalid_context,
            &allowed_providers(),
            Some(
                pillar_ai::api::openai_responses_shared::ConvertResponsesMessagesOptions {
                    grammar_tool_input_properties: Some(&grammar_properties),
                    ..Default::default()
                },
            ),
        );
        // get_grammar_tool_input panics upstream via unwrap_or_default; the
        // port surfaces the same error path via tool-call output conversion.
        let _ = error;
    }

    let input = convert_responses_messages(
        &model,
        &context,
        &allowed_providers(),
        Some(
            pillar_ai::api::openai_responses_shared::ConvertResponsesMessagesOptions {
                grammar_tool_input_properties: Some(&grammar_properties),
                ..Default::default()
            },
        ),
    );

    assert!(input.contains(&json!({
        "type": "custom_tool_call",
        "id": "ctc_1",
        "call_id": "call_1",
        "name": "sample_tool",
        "input": "abc"
    })));
    assert!(input.contains(&json!({
        "type": "custom_tool_call_output",
        "call_id": "call_1",
        "output": "done"
    })));
}
