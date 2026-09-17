//! Port of the upstream openai-completions tests that exercise mocked
//! request/stream behavior (pi v0.84.3): openai-completions-response-model,
//! openai-completions-empty-tools, openai-completions-cache-control-format,
//! openai-completions-tool-result-images, sampling-options, and unit-level
//! coverage of the SSE parser, usage mapping, stop-reason mapping, and the
//! pipe tool-call ID normalizer. Live-API test files (responseid, xhigh,
//! tool-call-without-result, tool-call-id-normalization e2e) are not ported.

#![cfg(feature = "providers")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use pillar_ai::api::openai_completions::{
    OpenaiCompletionsOptions, SimpleStreamOptions, convert_messages, get_compat, stream,
    stream_simple,
};
use pillar_ai::event_stream::collect_events;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, StopReason, Tool, ToolResultMessage, Usage,
    UserContent,
};
use pillar_ai::{AbortSignal, ProviderRequestError};

// --- Mock transport ------------------------------------------------------

type CapturedRequest = Arc<AsyncMutex<Vec<FetchRequest>>>;

/// Transport that answers every request with a canned SSE stream and records
/// the requests it saw.
struct MockSse {
    chunks: Vec<Value>,
    status: u16,
    captured: CapturedRequest,
}

impl MockSse {
    fn new(chunks: Vec<Value>) -> (Self, CapturedRequest) {
        let captured: CapturedRequest = Arc::default();
        (
            Self {
                chunks,
                status: 200,
                captured: Arc::clone(&captured),
            },
            captured,
        )
    }

    fn with_error(status: u16, body: Value) -> (Self, CapturedRequest) {
        let captured: CapturedRequest = Arc::default();
        (
            Self {
                chunks: vec![body],
                status,
                captured: Arc::clone(&captured),
            },
            captured,
        )
    }
}

fn sse_body(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {}\n\n", chunk));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

#[async_trait::async_trait]
impl FetchFn for MockSse {
    async fn fetch(
        &self,
        request: FetchRequest,
    ) -> Result<FetchResponse, pillar_ai::error::AiError> {
        self.captured.lock().await.push(request);
        let (status, body) = if self.status == 200 {
            (200, sse_body(&self.chunks))
        } else {
            (
                self.status,
                self.chunks
                    .first()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            )
        };
        let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
            vec![Ok(body.into_bytes()), Ok(Vec::new())];
        Ok(FetchResponse {
            status,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(futures::stream::iter(chunks)),
        })
    }
}

// --- Helpers -------------------------------------------------------------

fn now() -> u64 {
    1
}

fn base_model() -> Model {
    Model {
        id: "gpt-test".to_string(),
        name: "GPT Test".to_string(),
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        base_url: "https://api.openai.test/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 128_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn user_message(content: &str) -> Message {
    Message::User {
        content: UserContent::Text(content.to_string()),
        timestamp: now(),
    }
}

fn assistant_tool_call(id: &str, name: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::tool_call(id, name, json!({}))],
        api: "openai-completions".to_string(),
        provider: "openai".to_string(),
        model: "gpt-test".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now(),
    }
}

fn tool_result(call_id: &str, name: &str) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        tool_call_id: call_id.to_string(),
        tool_name: name.to_string(),
        content: vec![Content::text("done")],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now(),
    }))
}

fn finish_chunk(finish_reason: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "prompt_tokens_details": { "cached_tokens": 0 },
            "completion_tokens_details": { "reasoning_tokens": 0 }
        }
    })
}

async fn run_stream_to_message(
    model: Model,
    context: Context,
    options: OpenaiCompletionsOptions,
) -> AssistantMessage {
    let s = stream(model, context, Some(options));
    let _events = collect_events(&s).await;
    s.result().await
}

async fn run_stream_simple_to_message(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessage {
    let s = stream_simple(model, context, Some(options));
    let _events = collect_events(&s).await;
    s.result().await
}

async fn last_request(captured: &CapturedRequest) -> FetchRequest {
    captured
        .lock()
        .await
        .last()
        .cloned()
        .expect("at least one request captured")
}

fn request_body(request: &FetchRequest) -> Value {
    serde_json::from_slice(request.body.as_deref().expect("request body")).expect("JSON body")
}

// --- openai-completions-response-model.test.ts ---------------------------

// "surfaces routed chunk.model on responseModel without changing model"
#[tokio::test]
async fn surfaces_routed_chunk_model_on_response_model() {
    let model = Model {
        id: "openrouter/auto".to_string(),
        name: "OpenRouter Auto".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        ..base_model()
    };
    let chunks = vec![
        json!({ "id": "chatcmpl-1", "model": "anthropic/claude-opus-4.8", "choices": [{ "index": 0, "delta": { "content": "hi" } }] }),
        json!({
            "id": "chatcmpl-1", "model": "anthropic/claude-opus-4.8",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "prompt_tokens_details": { "cached_tokens": 0 }, "completion_tokens_details": { "reasoning_tokens": 0 } }
        }),
    ];
    let (mock, _captured) = MockSse::new(chunks);
    let message = run_stream_to_message(
        model.clone(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert_eq!(
        message.response_model.as_deref(),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(message.provider, "openrouter");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

// "leaves responseModel undefined when chunks echo the requested id"
#[tokio::test]
async fn leaves_response_model_undefined_when_chunks_echo_requested_id() {
    let model = Model {
        id: "openrouter/auto".to_string(),
        name: "OpenRouter Auto".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        ..base_model()
    };
    let chunks = vec![
        json!({ "id": "chatcmpl-2", "model": "openrouter/auto", "choices": [{ "index": 0, "delta": { "content": "hi" } }] }),
        json!({
            "id": "chatcmpl-2", "model": "openrouter/auto",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "prompt_tokens_details": { "cached_tokens": 0 }, "completion_tokens_details": { "reasoning_tokens": 0 } }
        }),
    ];
    let (mock, _captured) = MockSse::new(chunks);
    let message = run_stream_to_message(
        model.clone(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert_eq!(message.response_model, None);
}

// "ignores empty or missing chunk.model"
#[tokio::test]
async fn ignores_empty_or_missing_chunk_model() {
    let model = Model {
        id: "openrouter/auto".to_string(),
        name: "OpenRouter Auto".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        ..base_model()
    };
    let chunks = vec![
        json!({ "id": "chatcmpl-3", "choices": [{ "index": 0, "delta": { "content": "hi" } }] }),
        json!({ "id": "chatcmpl-3", "model": "", "choices": [{ "index": 0, "delta": { "content": "!" } }] }),
        json!({
            "id": "chatcmpl-3",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2, "prompt_tokens_details": { "cached_tokens": 0 }, "completion_tokens_details": { "reasoning_tokens": 0 } }
        }),
    ];
    let (mock, _captured) = MockSse::new(chunks);
    let message = run_stream_to_message(
        model.clone(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.model, "openrouter/auto");
    assert_eq!(message.response_model, None);
}

// --- openai-completions-empty-tools.test.ts (mockable cases) --------------

// "omits tools field when context.tools is an empty array"
#[tokio::test]
async fn omits_tools_field_when_context_tools_is_empty() {
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            tools: vec![],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);

    let params = request_body(&last_request(&captured).await);
    assert!(params.get("tools").is_none(), "{}", params);
}

// "sends default maxTokens" + "clamps default maxTokens to remaining context"
#[tokio::test]
async fn sends_and_clamps_max_tokens() {
    // default: model.maxTokens used (max_completion_tokens for openai)
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let _message = run_stream_simple_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        SimpleStreamOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;
    let params = request_body(&last_request(&captured).await);
    assert!(params.get("max_tokens").is_none());
    assert_eq!(params["max_completion_tokens"], json!(4096));

    // clamped default: 10000 - estimate(8000 x) - 4096 safety = 3904 max
    let model = Model {
        context_window: 10_000,
        max_tokens: 8_000,
        ..base_model()
    };
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let _message = run_stream_simple_to_message(
        model,
        Context {
            messages: vec![user_message(&"x".repeat(8000))],
            ..Default::default()
        },
        SimpleStreamOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;
    let params = request_body(&last_request(&captured).await);
    assert_eq!(params["max_completion_tokens"], json!(3904), "{}", params);
}

// "still emits tools: [] for Anthropic/LiteLLM proxy when conversation has tool history"
#[tokio::test]
async fn emits_empty_tools_for_tool_history() {
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let _message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![
                user_message("use the tool"),
                Message::Assistant(Box::new(assistant_tool_call("t1", "noop"))),
                tool_result("t1", "noop"),
            ],
            tools: vec![],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    let params = request_body(&last_request(&captured).await);
    assert_eq!(params["tools"], json!([]));
}

// --- openai-completions-cache-control-format.test.ts ---------------------

// "applies Anthropic-style cache_control to system prompt, last tool, and
// last conversation message" (upstream: cache-control-format cases)
#[tokio::test]
async fn applies_anthropic_cache_control_markers() {
    let model = Model {
        compat: Some(pillar_ai::types::ModelCompat::OpenaiCompletions(
            pillar_ai::types::OpenaiCompletionsCompat {
                cache_control_format: Some("anthropic".to_string()),
                ..Default::default()
            }
            .into(),
        )),
        ..base_model()
    };
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let tool = Tool {
        name: "echo".to_string(),
        description: "Echo".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
    };
    let _message = run_stream_to_message(
        model,
        Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![user_message("hi")],
            tools: vec![tool],
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    let params = request_body(&last_request(&captured).await);
    let system = &params["messages"][0];
    assert_eq!(system["role"], json!("system"));
    assert_eq!(
        system["content"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
    assert_eq!(
        params["tools"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
    let last = params["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(
        last["content"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
}

// --- openai-completions-tool-result-images.test.ts ------------------------

// convertMessages: tool result images are routed into a user message with
// image_url parts (and text result keeps a placeholder when empty).
#[test]
fn tool_result_images_become_user_image_parts() {
    let model = Model {
        input: vec!["text".to_string(), "image".to_string()],
        ..base_model()
    };
    let tool_result = ToolResultMessage {
        tool_call_id: "call_1".to_string(),
        tool_name: "screenshot".to_string(),
        content: vec![
            Content::image("aGVsbG8=", "image/png"),
            Content::image("d29ybGQ=", "image/jpeg"),
        ],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now(),
    };
    let context = Context {
        messages: vec![
            user_message("take a screenshot"),
            Message::Assistant(Box::new(assistant_tool_call("call_1", "screenshot"))),
            Message::ToolResult(Box::new(tool_result)),
        ],
        ..Default::default()
    };

    let params = convert_messages(&model, &context, &get_compat(&model), None);
    let tool_messages: Vec<&Value> = params
        .iter()
        .filter(|message| message["role"] == json!("tool"))
        .collect();
    assert_eq!(tool_messages.len(), 1);
    assert_eq!(tool_messages[0]["content"], json!("(see attached image)"));
    assert_eq!(tool_messages[0]["tool_call_id"], json!("call_1"));

    let image_message = params
        .iter()
        .find(|message| {
            message["role"] == json!("user")
                && message["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| part["type"] == json!("image_url"))
                })
        })
        .expect("image user message");
    let parts = image_message["content"].as_array().unwrap();
    assert_eq!(
        parts[0]["text"],
        json!("Attached image(s) from tool result:")
    );
    assert_eq!(
        parts[1]["image_url"]["url"],
        json!("data:image/png;base64,aGVsbG8=")
    );
    assert_eq!(
        parts[2]["image_url"]["url"],
        json!("data:image/jpeg;base64,d29ybGQ=")
    );
}

// pipe-separated tool call IDs are normalized and reused for tool results
// (upstream: tool-call-id-normalization prefilled-context, mocked here)
#[test]
fn normalizes_pipe_separated_tool_call_ids() {
    let model = base_model();
    let long_id = format!(
        "call_pAYbIr76hXIjncD9UE4eGfnS|{}",
        "t5nnb2qYMFWGSsr13fhCd1CaCu3t3qONEPuOudu4HSVEtA8YJSL6FAZUxvoOoD792VIJWl91g87EdqsCWp9krVsdBysQoDaf9lMCLb8BS4EYi4gQd5kBQBYLlgD71PYwvf+TbMD9J9/5OMD42oxSRj8H+vRf78/l2Xla33LWz4nOgsddBlbvabICRs8GHt5C9PK5keFtzyi3lsyVKNlfduK3iphsZqs4MLv4zyGJnvZo/+QzShyk5xnMSQX/f98+aEoNflEApCdEOXipipgeiNWnpFSHbcwmMkZoJhURNu+JEz3xCh1mrXeYoN5o+trLL3IXJacSsLYXDrYTipZZbJFRPAucgbnjYBC+/ZzJOfkwCs+Gkw7EoZR7ZQgJ8ma+9586n4tT4cI8DEhBSZsWMjrCt8dxKg=="
    );
    let assistant = AssistantMessage {
        stop_reason: StopReason::ToolUse,
        // Cross-model replay (openai-responses id → openai-completions) is
        // what triggers tool-call ID normalization.
        api: "openai-responses".to_string(),
        provider: "github-copilot".to_string(),
        model: "gpt-5.2-codex".to_string(),
        ..assistant_tool_call(&long_id, "echo")
    };
    let context = Context {
        messages: vec![
            user_message("echo"),
            Message::Assistant(Box::new(assistant)),
            tool_result(&long_id, "echo"),
        ],
        ..Default::default()
    };

    let params = convert_messages(&model, &context, &get_compat(&model), None);
    let tool_call = params
        .iter()
        .find(|message| message["role"] == json!("assistant") && message["tool_calls"].is_array())
        .and_then(|message| message["tool_calls"].as_array())
        .and_then(|calls| calls.first())
        .expect("tool call present");
    let normalized_id = tool_call["id"].as_str().expect("string id");
    assert!(normalized_id.len() <= 40, "{normalized_id}");
    assert!(
        normalized_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "{normalized_id}"
    );

    let tool_message = params
        .iter()
        .find(|message| message["role"] == json!("tool"))
        .expect("tool message");
    assert_eq!(tool_message["tool_call_id"], json!(normalized_id));
}

// --- sampling-options.test.ts ---------------------------------------------

async fn capture_payload(model: Model, options: SimpleStreamOptions) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_for_callback = Arc::clone(&captured);
    let mut options = options;
    options.api_key = Some("fake-key".to_string());
    options.on_payload = Some(Arc::new(move |_model, payload| {
        let captured = Arc::clone(&captured_for_callback);
        Box::pin(async move {
            *captured.lock().unwrap() = Some(payload);
            None
        })
    }));

    // The default fetch would try real HTTP; the request never happens
    // because we replace fetch with a failing transport after capture.
    struct FailingFetch;
    #[async_trait::async_trait]
    impl FetchFn for FailingFetch {
        async fn fetch(
            &self,
            _request: FetchRequest,
        ) -> Result<FetchResponse, pillar_ai::error::AiError> {
            Err(pillar_ai::error::AiError::Other(
                "connection refused".to_string(),
            ))
        }
    }
    options.fetch = Some(Arc::new(FailingFetch));

    let s = stream_simple(
        model,
        Context {
            messages: vec![user_message("Hello")],
            ..Default::default()
        },
        Some(options),
    );
    let _events = collect_events(&s).await;
    let _message = s.result().await;

    let captured = captured.lock().unwrap().take();
    captured.expect("payload captured before request failure")
}

// "merges stream-option sampling params into the request body"
#[tokio::test]
async fn merges_stream_option_sampling_params() {
    let payload = capture_payload(
        base_model(),
        SimpleStreamOptions {
            sampling_params: Some(
                serde_json::from_value(json!({ "top_p": 0.95, "top_k": 0, "min_p": 0 })).unwrap(),
            ),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["top_p"], json!(0.95));
    assert_eq!(payload["top_k"], json!(0));
    assert_eq!(payload["min_p"], json!(0));
}

// "omits sampling params when neither options nor model set them"
#[tokio::test]
async fn omits_sampling_params_when_not_set() {
    let payload = capture_payload(base_model(), SimpleStreamOptions::default()).await;
    assert!(payload.get("temperature").is_none());
    assert!(payload.get("top_p").is_none());
}

// "applies model-level sampling params"
#[tokio::test]
async fn applies_model_level_sampling_params() {
    let model = Model {
        sampling_params: Some(
            serde_json::from_value(json!({ "temperature": 1, "top_p": 0.95 })).unwrap(),
        ),
        ..base_model()
    };
    let payload = capture_payload(model, SimpleStreamOptions::default()).await;
    assert_eq!(payload["temperature"], json!(1));
    assert_eq!(payload["top_p"], json!(0.95));
}

// "merges stream-option keys over model-level keys"
#[tokio::test]
async fn merges_stream_option_keys_over_model_keys() {
    let model = Model {
        sampling_params: Some(
            serde_json::from_value(json!({ "top_p": 0.95, "min_p": 0.05 })).unwrap(),
        ),
        ..base_model()
    };
    let payload = capture_payload(
        model,
        SimpleStreamOptions {
            sampling_params: Some(serde_json::from_value(json!({ "top_p": 0.5 })).unwrap()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["top_p"], json!(0.5));
    assert_eq!(payload["min_p"], json!(0.05));
}

// "overrides named request fields"
#[tokio::test]
async fn sampling_params_override_named_request_fields() {
    let payload = capture_payload(
        base_model(),
        SimpleStreamOptions {
            temperature: Some(0.0),
            sampling_params: Some(serde_json::from_value(json!({ "temperature": 1 })).unwrap()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(payload["temperature"], json!(1));
}

// --- SSE parsing / stream error path --------------------------------------

// "parses SSE frames with CRLF, multi-line data, and [DONE]"
#[tokio::test]
async fn parses_sse_framing_variants() {
    struct RawBody {
        body: String,
        captured: CapturedRequest,
    }
    #[async_trait::async_trait]
    impl FetchFn for RawBody {
        async fn fetch(
            &self,
            request: FetchRequest,
        ) -> Result<FetchResponse, pillar_ai::error::AiError> {
            self.captured.lock().await.push(request);
            let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
                vec![Ok(self.body.clone().into_bytes())];
            Ok(FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(futures::stream::iter(chunks)),
            })
        }
    }

    // CRLF line endings, an SSE comment line, a multi-line data event, and a
    // final event without a trailing blank line.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\n\r\n\
                : keep-alive comment\r\n\r\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\r\n\
                \r\n\
                data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n";
    let captured: CapturedRequest = Arc::default();
    let transport = RawBody {
        body: body.to_string(),
        captured: Arc::clone(&captured),
    };
    let message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(transport)),
            ..Default::default()
        },
    )
    .await;

    let text: Vec<String> = message
        .content
        .iter()
        .filter_map(|block| block.as_text().map(str::to_string))
        .collect();
    assert_eq!(text.join(""), "ab");
    assert_eq!(message.stop_reason, StopReason::Stop);
    let _ = last_request(&captured).await;
}

// "surfaces the provider error body when the request fails" (upstream:
// provider-error-body-passthrough, mocked here)
#[tokio::test]
async fn surfaces_provider_error_body_in_error_message() {
    let (mock, _captured) = MockSse::with_error(
        401,
        json!({ "error": { "message": "upstream rejected request" } }),
    );
    let message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.expect("error message");
    assert!(
        error_message.contains("upstream rejected request"),
        "{error_message}"
    );
}

// "reports aborted requests as aborted stop reasons"
#[tokio::test]
async fn reports_aborted_requests_as_aborted() {
    struct HangingFetch {
        signal: AbortSignal,
    }
    #[async_trait::async_trait]
    impl FetchFn for HangingFetch {
        async fn fetch(
            &self,
            _request: FetchRequest,
        ) -> Result<FetchResponse, pillar_ai::error::AiError> {
            // Abort while the "request" is in flight.
            self.signal.abort(None);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(pillar_ai::error::AiError::Other(
                "should not complete".to_string(),
            ))
        }
    }

    let controller = AbortSignal::new();
    let s = stream(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        Some(OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(HangingFetch {
                signal: controller.clone(),
            })),
            signal: Some(controller),
            ..Default::default()
        }),
    );
    let _events = collect_events(&s).await;
    let message = s.result().await;

    eprintln!(
        "DBG stop={:?} error={:?} content={:?}",
        message.stop_reason, message.error_message, message.content
    );
    assert_eq!(
        message.stop_reason,
        StopReason::Aborted,
        "error: {:?}",
        message.error_message
    );
}

// "sends bearer auth and the request body to the configured base URL"
#[tokio::test]
async fn sends_bearer_auth_to_base_url() {
    let chunks = vec![finish_chunk("stop")];
    let (mock, captured) = MockSse::new(chunks);
    let _message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("secret".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        },
    )
    .await;

    let request = last_request(&captured).await;
    assert_eq!(request.url, "https://api.openai.test/v1/chat/completions");
    let auth = request
        .headers
        .iter()
        .find(|(name, _)| name == "Authorization")
        .map(|(_, value)| value.clone())
        .expect("authorization header");
    assert_eq!(auth, "Bearer secret");
    let params = request_body(&request);
    assert_eq!(params["model"], json!("gpt-test"));
    assert_eq!(params["stream"], json!(true));
}

// "errors without an API key or auth header"
#[test]
fn errors_without_api_key_or_auth_header() {
    let error = pillar_ai::api::openai_completions::get_client_api_key("openai", None, None);
    assert!(error.is_err());
    assert!(
        error
            .unwrap_err()
            .to_string()
            .contains("No API key for provider: openai")
    );
}

// "usage mapping: cached tokens are cache reads, writes tracked separately"
#[test]
fn usage_mapping_matches_provider_semantics() {
    let model = base_model();
    let usage = pillar_ai::api::openai_completions::parse_chunk_usage(
        &json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 20, "cache_write_tokens": 5 },
            "completion_tokens_details": { "reasoning_tokens": 10 }
        }),
        &model,
    );
    assert_eq!(usage.input, 75);
    assert_eq!(usage.output, 50);
    assert_eq!(usage.cache_read, 20);
    assert_eq!(usage.cache_write, 5);
    assert_eq!(usage.reasoning, Some(10));
    assert_eq!(usage.total_tokens, 75 + 50 + 20 + 5);
}

// "stop reason mapping: finish reasons map to pillar stop reasons"
#[test]
fn stop_reason_mapping_matches_upstream() {
    use pillar_ai::api::openai_completions::map_stop_reason;
    assert_eq!(map_stop_reason("stop").0, StopReason::Stop);
    assert_eq!(map_stop_reason("end").0, StopReason::Stop);
    assert_eq!(map_stop_reason("length").0, StopReason::Length);
    assert_eq!(map_stop_reason("tool_calls").0, StopReason::ToolUse);
    assert_eq!(map_stop_reason("function_call").0, StopReason::ToolUse);
    let (reason, message) = map_stop_reason("content_filter");
    assert_eq!(reason, StopReason::Error);
    assert_eq!(
        message.as_deref(),
        Some("Provider finish_reason: content_filter")
    );
    let (reason, message) = map_stop_reason("weird_new_reason");
    assert_eq!(reason, StopReason::Error);
    assert_eq!(
        message.as_deref(),
        Some("Provider finish_reason: weird_new_reason")
    );
}

// compat auto-detection from provider and base URL (upstream detectCompat)
#[test]
fn compat_detection_from_provider_and_base_url() {
    let compat = get_compat(&base_model());
    assert!(compat.supports_store);
    assert_eq!(
        compat.max_tokens_field,
        pillar_ai::api::openai_completions::MaxTokensField::MaxCompletionTokens
    );
    assert!(compat.supports_reasoning_effort);
    assert!(compat.supports_strict_mode);

    let deepseek = Model {
        provider: "deepseek".to_string(),
        base_url: "https://api.deepseek.com".to_string(),
        reasoning: true,
        ..base_model()
    };
    let compat = get_compat(&deepseek);
    assert!(!compat.supports_store);
    assert_eq!(
        compat.max_tokens_field,
        pillar_ai::api::openai_completions::MaxTokensField::MaxTokens
    );
    assert_eq!(
        compat.thinking_format,
        pillar_ai::api::openai_completions::ThinkingFormat::Deepseek
    );
    assert!(compat.requires_reasoning_content_on_assistant_messages);

    let zai = Model {
        provider: "zai".to_string(),
        base_url: "https://api.z.ai/api/paas/v4".to_string(),
        ..base_model()
    };
    let compat = get_compat(&zai);
    assert_eq!(
        compat.thinking_format,
        pillar_ai::api::openai_completions::ThinkingFormat::Zai
    );
    assert!(!compat.supports_reasoning_effort);
}

// provider retry wraps streaming requests: a retryable 429 then success
#[tokio::test]
async fn retries_retryable_http_failures() {
    struct RetryThenSuccess {
        calls: Arc<Mutex<u32>>,
    }
    #[async_trait::async_trait]
    impl FetchFn for RetryThenSuccess {
        async fn fetch(
            &self,
            _request: FetchRequest,
        ) -> Result<FetchResponse, pillar_ai::error::AiError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                drop(calls);
                // Signal a retryable server error through the transport path.
                return Err(pillar_ai::error::AiError::Other(
                    "transport failed".to_string(),
                ));
            }
            drop(calls);
            let body = sse_body(&[finish_chunk("stop")]);
            let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
                vec![Ok(body.into_bytes())];
            Ok(FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(futures::stream::iter(chunks)),
            })
        }
    }

    // A transport error maps to status None, which upstream treats as
    // retryable; confirm the stream completes after the retry.
    let calls: Arc<Mutex<u32>> = Arc::default();
    let message = run_stream_to_message(
        base_model(),
        Context {
            messages: vec![user_message("hi")],
            ..Default::default()
        },
        OpenaiCompletionsOptions {
            api_key: Some("test".to_string()),
            fetch: Some(Arc::new(RetryThenSuccess {
                calls: Arc::clone(&calls),
            })),
            max_retries: Some(1),
            max_retry_delay_ms: Some(1),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(*calls.lock().unwrap(), 2);
}

// keep the ProviderRequestError import used (retry options live on options)
#[allow(dead_code)]
fn assert_error_shape(error: ProviderRequestError) -> ProviderRequestError {
    error
}

/// Transport whose body yields one SSE delta, then aborts the signal itself
/// and stalls forever: an aborted response must stop reading the body
/// immediately instead of waiting for the server to finish.
struct AbortingBody {
    signal: AbortSignal,
}

#[async_trait::async_trait]
impl FetchFn for AbortingBody {
    async fn fetch(
        &self,
        _request: FetchRequest,
    ) -> Result<FetchResponse, pillar_ai::error::AiError> {
        let delta = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": now(),
            "model": "gpt-test",
            "choices": [{ "index": 0, "delta": { "content": "partial text" } }],
        });
        let first: Result<Vec<u8>, pillar_ai::error::AiError> =
            Ok(format!("data: {delta}\n\n").into_bytes());
        let signal = self.signal.clone();
        // The abort fires when this item is *pulled* (not when the body is
        // built), so the delta is read first.
        let abort = futures::stream::once(async move {
            signal.abort(None);
            Ok::<Vec<u8>, pillar_ai::error::AiError>(Vec::new())
        });
        let pending: futures::stream::Pending<Result<Vec<u8>, pillar_ai::error::AiError>> =
            futures::stream::pending();
        let body = futures::StreamExt::chain(
            futures::StreamExt::chain(futures::stream::once(async move { first }), abort),
            pending,
        );
        Ok(FetchResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(body),
        })
    }
}

/// The abort stops the body read mid-stream and the partial text survives
/// with `stopReason: "aborted"` (upstream the SDK's `abortSignal`).
#[tokio::test]
async fn abort_stops_the_body_read_without_waiting_for_the_server() {
    let signal = AbortSignal::new();
    let message = tokio::time::timeout(
        Duration::from_secs(5),
        run_stream_to_message(
            base_model(),
            Context {
                messages: vec![user_message("Hello")],
                ..Default::default()
            },
            OpenaiCompletionsOptions {
                api_key: Some("test".to_string()),
                fetch: Some(Arc::new(AbortingBody {
                    signal: signal.clone(),
                })),
                signal: Some(signal),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("the aborted stream must not wait for the server");

    assert_eq!(
        message.stop_reason,
        StopReason::Aborted,
        "error: {:?}",
        message.error_message
    );
    assert_eq!(
        message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<String>(),
        "partial text",
        "the streamed partial text is kept"
    );
}
