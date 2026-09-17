//! Port of the upstream mistral-* tests (pi v0.84.3) that run without live
//! API keys: mistral-http-transport, mistral-raw-stop-reason, mistral-
//! reasoning-mode, mistral-tool-schema.
//! One Rust test per upstream test case, same names in comments.

#![cfg(feature = "providers")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_ai::ProviderHeaders;
use pillar_ai::api::mistral_conversations::{
    MistralOptions, SimpleStreamOptions, to_mistral_wire_payload,
};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    CacheRetention, Content, Context, Message, Model, StopReason, Tool, ToolResultMessage,
    UserContent,
};
use serde_json::{Value, json};

// --- Helpers ---------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn mistral_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "mistral-conversations".to_string(),
        provider: "mistral".to_string(),
        base_url: "https://api.mistral.ai".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 8192,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: now_ms(),
    }
}

fn sse_response(events: &[Value]) -> FetchResponse {
    let body = format!(
        "{}\r\n\r\ndata: [DONE]\r\n\r\n",
        events
            .iter()
            .map(|e| format!("data: {}", e))
            .collect::<Vec<_>>()
            .join("\r\n\r\n")
    );
    FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(body.into_bytes())])),
    }
}

fn sse_response_raw(body: String) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(body.into_bytes())])),
    }
}

fn error_response(status: u16, body: &str) -> FetchResponse {
    FetchResponse {
        status,
        headers: vec![],
        body: Box::pin(futures::stream::iter(vec![Ok(body.as_bytes().to_vec())])),
    }
}

fn terminal_event(finish_reason: &str) -> Value {
    json!({
        "id": "mistral-response-id",
        "model": "mistral-large-latest",
        "choices": [{ "index": 0, "finish_reason": finish_reason, "delta": {} }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// Mock fetch capturing requests and returning a scripted response.
struct MockFetch {
    responses: Mutex<Vec<FetchResponse>>,
    requests: Mutex<Vec<FetchRequest>>,
}

impl MockFetch {
    fn new(responses: Vec<FetchResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn last_request(&self) -> FetchRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request was made")
    }
}

#[async_trait]
impl FetchFn for MockFetch {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| AiError::Other("no scripted response".to_string()))
    }
}

fn header_value<'a>(request: &'a FetchRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn has_header(request: &FetchRequest, name: &str) -> bool {
    request
        .headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case(name))
}

// --- mistral-http-transport.test.ts ------------------------------------------

#[tokio::test]
async fn serializes_sdk_style_payloads_to_the_mistral_wire_format() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: Some("Be precise".to_string()),
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![
                Content::text("describe"),
                Content::Image {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                },
            ]),
            timestamp: 1,
        }],
        tools: vec![Tool {
            name: "lookup".to_string(),
            description: "Look something up".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
            }),
            constrained_sampling: None,
        }],
    };

    let captured_payload: Arc<Mutex<Option<Value>>> = Arc::default();

    let fetch = MockFetch::new(vec![sse_response(&[terminal_event("stop")])]);
    let mut options = MistralOptions {
        api_key: Some("secret".to_string()),
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        headers: {
            let mut h = ProviderHeaders::new();
            h.insert("x-custom".to_string(), Some("value".to_string()));
            Some(h)
        },
        max_tokens: Some(123),
        prompt_mode: Some("reasoning".to_string()),
        reasoning_effort: Some("high".to_string()),
        tool_choice: Some(json!({ "type": "function", "function": { "name": "lookup" } })),
        session_id: Some("session-1".to_string()),
        ..Default::default()
    };

    let captured_for_callback = Arc::clone(&captured_payload);
    options.on_payload = Some(Arc::new(move |_model, payload| {
        *captured_for_callback.lock().unwrap() = Some(payload.clone());
        let mut next = payload;
        if let Some(obj) = next.as_object_mut() {
            obj.insert("topP".to_string(), json!(0.9));
            obj.insert("randomSeed".to_string(), json!(42));
            obj.insert(
                "responseFormat".to_string(),
                json!({
                    "type": "json_schema",
                    "jsonSchema": {
                        "name": "result",
                        "schemaDefinition": {
                            "type": "object",
                            "properties": { "maxTokens": { "type": "number" } },
                        },
                    },
                }),
            );
            obj.insert("presencePenalty".to_string(), json!(0.1));
            obj.insert("frequencyPenalty".to_string(), json!(0.2));
            obj.insert("parallelToolCalls".to_string(), json!(true));
            obj.insert("safePrompt".to_string(), json!(true));
        }
        Box::pin(async move { Some(next) })
    }));

    let message = pillar_ai::api::mistral_conversations::stream(model, context, Some(options))
        .result()
        .await;

    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );

    let request = fetch.last_request();
    assert_eq!(request.url, "https://api.mistral.ai/v1/chat/completions");
    assert_eq!(
        header_value(&request, "authorization"),
        Some("Bearer secret")
    );
    assert_eq!(header_value(&request, "accept"), Some("text/event-stream"));
    assert_eq!(header_value(&request, "x-affinity"), Some("session-1"));
    assert_eq!(header_value(&request, "x-custom"), Some("value"));
    assert!(
        header_value(&request, "user-agent")
            .unwrap()
            .starts_with("pillar (")
    );

    let payload = captured_payload
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured");
    assert_eq!(payload["maxTokens"], json!(123));
    assert_eq!(payload["promptMode"], json!("reasoning"));
    assert_eq!(payload["promptCacheKey"], json!("session-1"));

    let wire: Value = serde_json::from_slice(request.body.as_deref().expect("body")).unwrap();
    assert_eq!(wire["max_tokens"], json!(123));
    assert_eq!(wire["prompt_mode"], json!("reasoning"));
    assert_eq!(wire["reasoning_effort"], json!("high"));
    assert_eq!(
        wire["tool_choice"],
        json!({ "type": "function", "function": { "name": "lookup" } })
    );
    assert_eq!(wire["prompt_cache_key"], json!("session-1"));
    assert_eq!(wire["top_p"], json!(0.9));
    assert_eq!(wire["random_seed"], json!(42));
    assert_eq!(wire["presence_penalty"], json!(0.1));
    assert_eq!(wire["frequency_penalty"], json!(0.2));
    assert_eq!(wire["parallel_tool_calls"], json!(true));
    assert_eq!(wire["safe_prompt"], json!(true));
    assert_eq!(
        wire["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "result",
                "schema": {
                    "type": "object",
                    "properties": { "maxTokens": { "type": "number" } },
                },
            },
        })
    );
    assert!(wire.get("maxTokens").is_none());
    assert!(wire.get("promptMode").is_none());
    assert!(wire.get("promptCacheKey").is_none());
    assert_eq!(
        wire["messages"],
        json!([
            { "role": "system", "content": "Be precise" },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "describe" },
                    { "type": "image_url", "image_url": "data:image/png;base64,aGVsbG8=" },
                ],
            },
        ])
    );
}

#[tokio::test]
async fn serializes_assistant_thinking_tool_calls_and_tool_results_for_replay() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![
            Message::Assistant(Box::new(pillar_ai::types::AssistantMessage {
                content: vec![
                    Content::Thinking {
                        thinking: "reason".to_string(),
                        thinking_signature: None,
                        redacted: None,
                    },
                    Content::text("answer"),
                    Content::ToolCall {
                        id: "abc123456".to_string(),
                        name: "lookup".to_string(),
                        arguments: json!({ "query": "pi" }),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
                api: "mistral-conversations".to_string(),
                provider: "mistral".to_string(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1,
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                tool_call_id: "abc123456".to_string(),
                tool_name: "lookup".to_string(),
                content: vec![
                    Content::text("found"),
                    Content::Image {
                        data: "aGVsbG8=".to_string(),
                        mime_type: "image/png".to_string(),
                    },
                ],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 2,
            })),
        ],
    };

    let fetch = MockFetch::new(vec![sse_response(&[terminal_event("stop")])]);
    let message = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    let request = fetch.last_request();
    let wire: Value = serde_json::from_slice(request.body.as_deref().expect("body")).unwrap();
    assert_eq!(
        wire["messages"],
        json!([
            {
                "role": "assistant",
                "prefix": false,
                "content": [
                    { "type": "thinking", "thinking": [{ "type": "text", "text": "reason" }] },
                    { "type": "text", "text": "answer" },
                ],
                "tool_calls": [
                    {
                        "id": "abc123456",
                        "type": "function",
                        "function": { "name": "lookup", "arguments": "{\"query\":\"pi\"}" },
                        "index": 0,
                    },
                ],
            },
            {
                "role": "tool",
                "tool_call_id": "abc123456",
                "name": "lookup",
                "content": [
                    { "type": "text", "text": "found" },
                    { "type": "image_url", "image_url": "data:image/png;base64,aGVsbG8=" },
                ],
            },
        ])
    );
}

#[tokio::test]
async fn parses_native_thinking_text_tool_calls_and_cached_token_usage() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };
    let events = vec![
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": { "content": [{ "type": "thinking", "thinking": [{ "type": "text", "text": "reason" }] }] },
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": { "content": [{ "type": "text", "text": "answer" }] },
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": null,
                "delta": {
                    "tool_calls": [{
                        "id": "abc123456",
                        "index": 0,
                        "function": { "name": "lookup", "arguments": "{\"query\":" },
                    }],
                },
            }],
        }),
        json!({
            "id": "response-1",
            "model": model.id,
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "delta": {
                    "tool_calls": [{
                        "id": "abc123456",
                        "index": 0,
                        "function": { "name": "lookup", "arguments": "\"pi\"}" },
                    }],
                },
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "total_tokens": 14,
                "prompt_tokens_details": { "cached_tokens": 3 },
            },
        }),
    ];

    let fetch = MockFetch::new(vec![sse_response(&events)]);
    let message = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(message.response_id.as_deref(), Some("response-1"));
    assert_eq!(
        message.content,
        vec![
            Content::Thinking {
                thinking: "reason".to_string(),
                thinking_signature: None,
                redacted: None,
            },
            Content::text("answer"),
            Content::ToolCall {
                id: "abc123456".to_string(),
                name: "lookup".to_string(),
                arguments: json!({ "query": "pi" }),
                thought_signature: None,
                namespace: None,
            },
        ]
    );
    assert_eq!(message.usage.input, 7);
    assert_eq!(message.usage.output, 4);
    assert_eq!(message.usage.cache_read, 3);
    assert_eq!(message.usage.cache_write, 0);
    assert_eq!(message.usage.total_tokens, 14);
}

#[tokio::test]
async fn parses_sse_and_utf8_sequences_split_across_transport_chunks() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };
    let event = json!({
        "id": "response-bytewise",
        "model": model.id,
        "choices": [{ "index": 0, "finish_reason": "stop", "delta": { "content": "héllo 🌍" } }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 },
    });
    // Byte-split the SSE body so multibyte sequences straddle chunks.
    let body = format!("data: {}\r\n\r\ndata: [DONE]\r\n\r\n", event);
    let bytes = body.into_bytes();
    let chunks: Vec<Result<Vec<u8>, AiError>> = bytes.into_iter().map(|b| Ok(vec![b])).collect();

    let fetch = MockFetch::new(vec![FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(chunks)),
    }]);

    let message = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content, vec![Content::text("héllo 🌍")]);
}

#[tokio::test]
async fn honors_case_insensitive_header_overrides_and_explicit_affinity_suppression() {
    let mut model = mistral_model("mistral-large-latest");
    model.headers = {
        let mut h = ProviderHeaders::new();
        h.insert(
            "Authorization".to_string(),
            Some("Bearer model-key".to_string()),
        );
        h.insert("X-Affinity".to_string(), Some("model-affinity".to_string()));
        Some(h)
    };
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };

    let fetch = MockFetch::new(vec![sse_response(&[terminal_event("stop")])]);
    let options = MistralOptions {
        api_key: Some("request-key".to_string()),
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        session_id: Some("automatic-affinity".to_string()),
        headers: {
            let mut h = ProviderHeaders::new();
            h.insert("authorization".to_string(), None);
            h.insert("x-affinity".to_string(), None);
            h.insert("User-Agent".to_string(), Some("custom-agent".to_string()));
            Some(h)
        },
        ..Default::default()
    };

    let _ = pillar_ai::api::mistral_conversations::stream(model, context, Some(options))
        .result()
        .await;

    let request = fetch.last_request();
    assert!(!has_header(&request, "authorization"));
    assert!(!has_header(&request, "x-affinity"));
    assert_eq!(header_value(&request, "user-agent"), Some("custom-agent"));
}

#[tokio::test]
async fn aborts_while_waiting_for_an_sse_chunk() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };

    // A response body that never completes.
    let (_keep_tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, AiError>>(1);
    let body_stream: pillar_ai::transport::ByteStream =
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        }));

    let fetch = MockFetch::new(vec![FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: body_stream,
    }]);

    let controller = pillar_ai::AbortSignal::new();
    let result = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch as pillar_ai::SharedFetchFn),
            signal: Some(controller.clone()),
            ..Default::default()
        }),
    );
    controller.abort(Some(pillar_ai::AbortReason::Aborted));
    let message = result.result().await;

    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn applies_the_request_timeout_while_waiting_for_an_sse_chunk() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };

    // A response body that never completes.
    let (_tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, AiError>>(1);
    let body: pillar_ai::transport::ByteStream =
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        }));

    let fetch = MockFetch::new(vec![FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body,
    }]);

    let message = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch as pillar_ai::SharedFetchFn),
            timeout_ms: Some(5),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.expect("error message");
    // Upstream asserts /timeout/i against the browser DOMException text
    // ("The operation was aborted due to timeout"); the Rust transport
    // produces "Mistral request timed out" — same semantics, different string.
    assert!(error.to_lowercase().contains("timed out"), "{error}");
}

#[tokio::test]
async fn preserves_http_status_and_response_bodies_in_errors() {
    let model = mistral_model("mistral-large-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };
    let fetch = MockFetch::new(vec![error_response(
        403,
        "{\"message\":\"blocked by gateway\"}",
    )]);

    let message = pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch as pillar_ai::SharedFetchFn),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Mistral API error (403): {\"message\":\"blocked by gateway\"}")
    );
}

// --- mistral-raw-stop-reason.test.ts -----------------------------------------

async fn raw_stop_reason_case(finish_reason: &str) -> pillar_ai::types::AssistantMessage {
    let model = mistral_model("devstral-medium-latest");
    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("hello")],
    };
    let event = json!({
        "id": "mistral-response-id",
        "model": model.id,
        "choices": [{ "index": 0, "finish_reason": finish_reason, "delta": {} }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1 },
    });
    let fetch = MockFetch::new(vec![sse_response_raw(format!(
        "data: {}\n\ndata: [DONE]\n\n",
        event
    ))]);
    pillar_ai::api::mistral_conversations::stream(
        model,
        context,
        Some(MistralOptions {
            api_key: Some("test".to_string()),
            fetch: Some(fetch as pillar_ai::SharedFetchFn),
            ..Default::default()
        }),
    )
    .result()
    .await
}

#[tokio::test]
async fn preserves_raw_mistral_finish_reasons_for_successful_stops() {
    let message = raw_stop_reason_case("stop").await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("stop"));
    assert!(message.error_message.is_none());
}

#[tokio::test]
async fn preserves_raw_mistral_finish_reasons_for_provider_error_stops() {
    let message = raw_stop_reason_case("error").await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("error"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: error")
    );
}

#[tokio::test]
async fn treats_unknown_mistral_finish_reasons_as_provider_error_stops() {
    let message = raw_stop_reason_case("unmapped_error").await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("unmapped_error"));
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: unmapped_error")
    );
}

// --- mistral-reasoning-mode.test.ts --------------------------------------------

async fn capture_payload(model: Model, options: SimpleStreamOptions) -> Value {
    let mut model = model;
    model.base_url = "http://127.0.0.1:9".to_string();

    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_for_callback = Arc::clone(&captured);
    let mut options = options;
    options.api_key = Some("fake-key".to_string());
    options.on_payload = Some(Arc::new(move |_model, payload| {
        *captured_for_callback.lock().unwrap() = Some(payload.clone());
        Box::pin(async move { Some(payload) })
    }));

    let context = Context {
        system_prompt: None,
        tools: Vec::new(),
        messages: vec![user_message("Hello")],
    };
    let _ =
        pillar_ai::api::mistral_conversations::stream_simple_mistral(model, context, Some(options))
            .result()
            .await;

    captured
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured before request failure")
}

#[tokio::test]
async fn uses_reasoning_effort_for_mistral_small_4() {
    let mut model = mistral_model("mistral-small-2603");
    model.reasoning = true;
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            reasoning: Some(pillar_ai::types::ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["reasoningEffort"], json!("high"));
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn omits_reasoning_controls_for_mistral_small_4_when_thinking_is_off() {
    let mut model = mistral_model("mistral-small-2603");
    model.reasoning = true;
    let payload = capture_payload(model, SimpleStreamOptions::default()).await;
    assert!(payload.get("reasoningEffort").is_none());
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn uses_prompt_mode_for_magistral_reasoning_models() {
    let mut model = mistral_model("magistral-medium-latest");
    model.reasoning = true;
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            reasoning: Some(pillar_ai::types::ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["promptMode"], json!("reasoning"));
    assert!(payload.get("reasoningEffort").is_none());
}

#[tokio::test]
async fn uses_reasoning_effort_for_mistral_medium_3_5() {
    let mut model = mistral_model("mistral-medium-3.5");
    model.reasoning = true;
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            reasoning: Some(pillar_ai::types::ThinkingLevel::Medium),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["reasoningEffort"], json!("high"));
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn omits_reasoning_controls_for_mistral_medium_3_5_when_thinking_is_off() {
    let mut model = mistral_model("mistral-medium-3.5");
    model.reasoning = true;
    let payload = capture_payload(model, SimpleStreamOptions::default()).await;
    assert!(payload.get("reasoningEffort").is_none());
    assert!(payload.get("promptMode").is_none());
}

#[tokio::test]
async fn uses_the_session_id_as_prompt_cache_key() {
    let model = mistral_model("mistral-large-latest");
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            session_id: Some("session-123".to_string()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["promptCacheKey"], json!("session-123"));
}

#[tokio::test]
async fn omits_prompt_cache_key_when_cache_retention_is_disabled() {
    let model = mistral_model("mistral-large-latest");
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            session_id: Some("session-123".to_string()),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    )
    .await;
    assert!(payload.get("promptCacheKey").is_none());
}

// --- mistral-tool-schema.test.ts ------------------------------------------------

#[tokio::test]
async fn strips_typebox_symbol_keys_before_the_sdk_validates_tool_schemas() {
    // Rust port: schemas are plain serde_json Values (no symbol keys), so
    // this pins the strict flag + clean serialization path.
    let mut model = mistral_model("devstral-medium-latest");
    model.base_url = "http://127.0.0.1:9".to_string();

    let context = Context {
        system_prompt: None,
        tools: vec![Tool {
            name: "inspect_schema".to_string(),
            description: "Inspect the schema".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "nested": {
                        "type": "object",
                        "properties": { "value": { "type": "string" } },
                    },
                },
            }),
            constrained_sampling: Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
                strict: pillar_ai::types::ConstrainedStrictness::Require,
            }),
        }],
        messages: vec![user_message("Hi")],
    };

    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_for_callback = Arc::clone(&captured);
    let options = MistralOptions {
        api_key: Some("fake-key".to_string()),
        on_payload: Some(Arc::new(move |_model, payload| {
            *captured_for_callback.lock().unwrap() = Some(payload.clone());
            Box::pin(async move { Some(payload) })
        })),
        ..Default::default()
    };

    let response = pillar_ai::api::mistral_conversations::stream(model, context, Some(options))
        .result()
        .await;

    let payload = captured.lock().unwrap().clone().expect("payload captured");
    let tools = payload["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["strict"], json!(true));
    let parameters = &tools[0]["function"]["parameters"];
    assert!(parameters.is_object());
    assert!(parameters["properties"].is_object());
    assert!(parameters["properties"]["nested"].is_object());
    // The request must not fail schema validation.
    assert_eq!(response.stop_reason, StopReason::Error); // fetch fails (port 9)
    assert!(
        !response
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Input validation failed")
    );
}

// --- to_mistral_wire_payload unit coverage ----------------------------------------

#[test]
fn wire_payload_remaps_camel_case_keys() {
    let payload = json!({
        "model": "m",
        "stream": true,
        "maxTokens": 10,
        "toolCalls": [],
    });
    let wire = to_mistral_wire_payload(&payload);
    assert_eq!(wire["max_tokens"], json!(10));
    assert!(wire.get("maxTokens").is_none());
    // Messages without array content pass through.
    assert!(wire.get("messages").is_none());
}
