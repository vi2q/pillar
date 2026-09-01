//! Port of the upstream bedrock-* tests (pi v0.84.3) that run without live
//! AWS credentials: bedrock-convert-messages, bedrock-endpoint-resolution,
//! bedrock-credentials, bedrock-thinking-payload, bedrock-raw-stop-reason,
//! bedrock-error-metadata, bedrock-response-headers, and bedrock-redacted-
//! reasoning.
//!
//! divergence: upstream mocks the `@aws-sdk/client-bedrock-runtime` module
//! (`BedrockRuntimeClient.send` + `ConverseStreamCommand`); the Rust port
//! injects a `FetchFn` fake that returns an AWS eventstream binary body, and
//! inspects the captured `FetchRequest` (URL, headers, JSON body) instead of
//! the SDK client config / command input. The client-config priority rules
//! are additionally asserted directly through `buildClientConfig`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_ai::ProviderEnv;
use pillar_ai::api::bedrock_converse_stream::{
    BedrockOptions, Credentials, build_client_config, stream,
};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    CacheRetention, Content, Context, Message, Model, ModelCompat, ModelCost, ModelCostRates,
    OpenaiCompletionsCompat, StopReason, ThinkingLevel, Tool, UserContent,
};
use serde_json::{Value, json};

// --- Helpers ---------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Encodes bytes into an AWS eventstream frame:
/// [4B total][4B headers-len][4B payload-len][headers][4B headers CRC][payload][4B message CRC].
fn eventstream_frame(message_type: &str, event_type: &str, payload: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(payload).expect("payload serializes");
    let mut headers: Vec<u8> = Vec::new();
    for (name, value) in [(":message-type", message_type), (":event-type", event_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // string value type
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let headers_len = headers.len() as u32;
    let payload_len = payload.len() as u32;
    let total = 12 + headers.len() as u32 + 4 + payload.len() as u32 + 4;
    let mut frame = Vec::with_capacity(total as usize);
    frame.extend_from_slice(&total.to_be_bytes());
    frame.extend_from_slice(&headers_len.to_be_bytes());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&[0, 0, 0, 0]); // headers CRC (not verified by port)
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&[0, 0, 0, 0]); // message CRC (not verified by port)
    frame
}

fn exception_frame(error_code: &str, message: &str) -> Vec<u8> {
    let payload = serde_json::to_vec(&json!({ "message": message })).expect("payload serializes");
    let mut headers: Vec<u8> = Vec::new();
    for (name, value) in [(":message-type", "exception"), (":error-code", error_code)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let headers_len = headers.len() as u32;
    let payload_len = payload.len() as u32;
    let total = 12 + headers.len() as u32 + 4 + payload.len() as u32 + 4;
    let mut frame = Vec::with_capacity(total as usize);
    frame.extend_from_slice(&total.to_be_bytes());
    frame.extend_from_slice(&headers_len.to_be_bytes());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame
}

/// Scripted event frames -> one eventstream body.
fn eventstream_body(frames: &[Vec<u8>]) -> Vec<u8> {
    frames.iter().flatten().copied().collect()
}

fn assistant_start() -> Vec<u8> {
    eventstream_frame(
        "event",
        "messageStart",
        &json!({ "messageStart": { "role": "assistant" } }),
    )
}

fn message_stop(reason: &str) -> Vec<u8> {
    eventstream_frame(
        "event",
        "messageStop",
        &json!({ "messageStop": { "stopReason": reason } }),
    )
}

fn tool_use_start(index: u64, id: &str, name: &str) -> Vec<u8> {
    eventstream_frame(
        "event",
        "contentBlockStart",
        &json!({
            "contentBlockStart": {
                "contentBlockIndex": index,
                "start": { "toolUse": { "toolUseId": id, "name": name } }
            }
        }),
    )
}

fn text_delta(index: u64, text: &str) -> Vec<u8> {
    eventstream_frame(
        "event",
        "contentBlockDelta",
        &json!({ "contentBlockDelta": { "contentBlockIndex": index, "delta": { "text": text } } }),
    )
}

fn tool_input_delta(index: u64, input: &str) -> Vec<u8> {
    eventstream_frame(
        "event",
        "contentBlockDelta",
        &json!({
            "contentBlockDelta": {
                "contentBlockIndex": index,
                "delta": { "toolUse": { "input": input } }
            }
        }),
    )
}

fn reasoning_delta(index: u64, reasoning: Value) -> Vec<u8> {
    eventstream_frame(
        "event",
        "contentBlockDelta",
        &json!({
            "contentBlockDelta": {
                "contentBlockIndex": index,
                "delta": { "reasoningContent": reasoning }
            }
        }),
    )
}

fn block_stop(index: u64) -> Vec<u8> {
    eventstream_frame(
        "event",
        "contentBlockStop",
        &json!({ "contentBlockStop": { "contentBlockIndex": index } }),
    )
}

fn redacted_bytes() -> Vec<u8> {
    // Standard base64 decode of the upstream test vector, hand-rolled to
    // avoid adding the base64 crate to dev-dependencies.
    const REDACTED_BASE64: &str = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";
    let mut output = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in REDACTED_BASE64.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => continue,
            _ => panic!("bad base64 char"),
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    output
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

fn eventstream_response(body: Vec<u8>) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: vec![
            (
                "content-type".to_string(),
                "application/vnd.amazon.eventstream".to_string(),
            ),
            ("x-amzn-requestid".to_string(), "req-123".to_string()),
        ],
        body: Box::pin(futures::stream::iter(vec![Ok(body)])),
    }
}

fn base_model() -> Model {
    Model {
        id: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
        name: "Claude Sonnet 4.5 (US)".to_string(),
        api: "bedrock-converse-stream".to_string(),
        provider: "amazon-bedrock".to_string(),
        base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            tiers: None,
        },
        context_window: 200_000,
        max_tokens: 64_000,
        sampling_params: None,
        headers: None,
        compat: Some(ModelCompat::OpenaiCompletions(Box::new(
            OpenaiCompletionsCompat {
                supports_strict_mode: Some(true),
                ..Default::default()
            },
        ))),
    }
}

fn nova_model() -> Model {
    let mut model = base_model();
    model.id = "amazon.nova-lite-v1:0".to_string();
    model.name = "Nova Lite".to_string();
    model.reasoning = false;
    model.compat = None;
    model
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: now_ms(),
    }
}

fn assistant_message_for_model(model: &Model, content: Vec<Content>) -> Message {
    Message::Assistant(Box::new(pillar_ai::types::AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }))
}

fn assistant_message(content: Vec<Content>) -> Message {
    assistant_message_for_model(&base_model(), content)
}

/// Assistant message tagged as produced by `model` — cross-model replays in
/// transform_messages (thinking drops, ID normalization) only fire when the
/// message's provider/api/model differ from the target model.
fn assistant_message_from(model: &Model, content: Vec<Content>) -> Message {
    assistant_message_for_model(model, content)
}

fn tool_result_message(tool_call_id: &str, text: &str) -> Message {
    Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
        tool_call_id: tool_call_id.to_string(),
        tool_name: "edit".to_string(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now_ms(),
    }))
}

/// Capture the payload built by the adapter (upstream `capturePayload` with
/// `signal: AbortSignal.abort()` and `cacheRetention: "none"` — the request
/// never leaves the mock, and no cache points are injected).
async fn capture_payload(context: &Context, model: &Model) -> Value {
    // Upstream captures the payload via the onPayload callback while the
    // request is aborted (never sent). Mirror that: capture from the
    // callback, not from the fetch request.
    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_cb = Arc::clone(&captured);
    let options = BedrockOptions {
        signal: Some(pillar_ai::AbortSignal::aborted(None)),
        cache_retention: Some(CacheRetention::None),
        on_payload: Some(Arc::new(move |_model, payload| {
            *captured_cb.lock().unwrap() = Some(payload.clone());
            Box::pin(async move { Some(payload) })
        })),
        ..Default::default()
    };
    let _ = stream(model.clone(), context.clone(), Some(options))
        .result()
        .await;
    captured
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured via onPayload before request abort")
}

/// Capture the payload with the adapter-default cache retention (upstream
/// "short") — cache points ARE injected. Used by the cache-point tests.
async fn capture_payload_retain(context: &Context, model: &Model) -> Value {
    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_cb = Arc::clone(&captured);
    let options = BedrockOptions {
        signal: Some(pillar_ai::AbortSignal::aborted(None)),
        on_payload: Some(Arc::new(move |_model, payload| {
            *captured_cb.lock().unwrap() = Some(payload.clone());
            Box::pin(async move { Some(payload) })
        })),
        ..Default::default()
    };
    let _ = stream(model.clone(), context.clone(), Some(options))
        .result()
        .await;
    captured
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured via onPayload before request abort")
}

/// Drive a full stream against a scripted eventstream body.
async fn drive_stream(
    model: &Model,
    context: &Context,
    options: BedrockOptions,
    body: Vec<u8>,
) -> pillar_ai::types::AssistantMessage {
    let fetch = MockFetch::new(vec![eventstream_response(body)]);
    let options = BedrockOptions {
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        ..options
    };
    stream(model.clone(), context.clone(), Some(options))
        .result()
        .await
}

// --- bedrock-convert-messages.test.ts ----------------------------------------

#[tokio::test]
async fn gates_native_strict_tool_use_by_model_capability() {
    let tool = Tool {
        name: "lookup".to_string(),
        description: "Look up a value".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
        }),
        constrained_sampling: Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: pillar_ai::types::ConstrainedStrictness::Require,
        }),
    };
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("Use the tool")],
        tools: vec![tool.clone()],
    };

    let payload = capture_payload(&context, &base_model()).await;
    let strict = payload
        .pointer("/toolConfig/tools/0/toolSpec/strict")
        .cloned();
    assert_eq!(strict, Some(json!(true)));

    let nova_context = Context {
        system_prompt: None,
        messages: vec![user_message("Use the tool")],
        tools: vec![Tool {
            constrained_sampling: Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
                strict: pillar_ai::types::ConstrainedStrictness::Prefer,
            }),
            ..tool
        }],
    };
    let nova_payload = capture_payload(&nova_context, &nova_model()).await;
    let nova_strict = nova_payload
        .pointer("/toolConfig/tools/0/toolSpec/strict")
        .cloned();
    assert_eq!(nova_strict, None);
}

#[tokio::test]
async fn preserves_empty_property_names_in_streamed_tool_arguments() {
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("Use the tool")],
        tools: Vec::new(),
    };
    let body = eventstream_body(&[
        assistant_start(),
        tool_use_start(0, "tool-1", "edit"),
        tool_input_delta(
            0,
            r#"{"path":"/workspace/foobar/file.js","edits":[{"oldText":"first","newText":"updated first"},{"oldText":"second","newText":"updated second","":""}]}"#,
        ),
        block_stop(0),
        message_stop("tool_use"),
    ]);
    let message = drive_stream(&base_model(), &context, BedrockOptions::default(), body).await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(
        message.content[0],
        Content::ToolCall {
            id: "tool-1".to_string(),
            name: "edit".to_string(),
            arguments: json!({
                "path": "/workspace/foobar/file.js",
                "edits": [
                    { "oldText": "first", "newText": "updated first" },
                    { "oldText": "second", "newText": "updated second", "": "" },
                ],
            }),
            thought_signature: None,
            namespace: None,
        }
    );
}

#[tokio::test]
async fn skips_unknown_user_content_blocks_instead_of_throwing() {
    // serde_json::Map (BTreeMap) drops unknown shapes at the type boundary;
    // the port models unknown content as skipped blocks, matching upstream's
    // `default: continue` in the converter.
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![Content::text("hello")]),
            timestamp: now_ms(),
        }],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]
            .get("content")
            .and_then(Value::as_array)
            .unwrap(),
        &[json!({ "text": "hello" })],
    );
}

#[tokio::test]
async fn replaces_blank_user_string_content_with_a_placeholder() {
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("   ")],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    assert_eq!(
        payload.pointer("/messages/0/content").cloned(),
        Some(json!([{ "text": "<empty>" }])),
    );
}

#[tokio::test]
async fn filters_blank_user_text_blocks_when_other_content_remains() {
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![Content::text(""), Content::text("hello")]),
            timestamp: now_ms(),
        }],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    assert_eq!(
        payload.pointer("/messages/0/content").cloned(),
        Some(json!([{ "text": "hello" }])),
    );
}

#[tokio::test]
async fn replaces_blank_tool_result_content_with_a_placeholder() {
    let context = Context {
        system_prompt: None,
        messages: vec![Message::ToolResult(Box::new(
            pillar_ai::types::ToolResultMessage {
                tool_call_id: "tool-1".to_string(),
                tool_name: "tool".to_string(),
                content: vec![Content::text("")],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: now_ms(),
            },
        ))],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    assert_eq!(
        payload
            .pointer("/messages/0/content/0/toolResult/content")
            .cloned(),
        Some(json!([{ "text": "<empty>" }])),
    );
}

#[tokio::test]
async fn removes_empty_property_names_only_from_replayed_bedrock_input() {
    let tool_arguments = json!({
        "path": "/workspace/foobar/file.js",
        "edits": [
            { "oldText": "first", "newText": "updated first" },
            { "oldText": "second", "newText": "updated second", "": "" },
        ],
    });
    let context = Context {
        system_prompt: None,
        messages: vec![
            assistant_message(vec![Content::ToolCall {
                id: "tool-1".to_string(),
                name: "edit".to_string(),
                arguments: tool_arguments.clone(),
                thought_signature: None,
                namespace: None,
            }]),
            tool_result_message("tool-1", "done"),
            user_message("Continue"),
        ],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    assert_eq!(
        payload
            .pointer("/messages/0/content/0/toolUse/input")
            .cloned(),
        Some(json!({
            "path": "/workspace/foobar/file.js",
            "edits": [
                { "oldText": "first", "newText": "updated first" },
                { "oldText": "second", "newText": "updated second" },
            ],
        })),
    );
    // The original message is not mutated.
    assert_eq!(
        tool_arguments.pointer("/edits/1").cloned(),
        Some(json!({ "oldText": "second", "newText": "updated second", "": "" })),
    );
}

#[tokio::test]
async fn normalizes_tool_call_ids_for_bedrock_wire() {
    // The assistant message must come from a DIFFERENT model than the target
    // (`base_model`): upstream normalization only fires cross-model
    // (transformMessages `isSameModel` check).
    let foreign = gpt_model();
    let context = Context {
        system_prompt: None,
        messages: vec![
            assistant_message_from(
                &foreign,
                vec![Content::ToolCall {
                    id: "call/with spaces+and/slashes".to_string(),
                    name: "edit".to_string(),
                    arguments: json!({ "path": "/tmp/a.txt" }),
                    thought_signature: None,
                    namespace: None,
                }],
            ),
            Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
                tool_call_id: "call/with spaces+and/slashes".to_string(),
                tool_name: "edit".to_string(),
                content: vec![Content::text("done")],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: now_ms(),
            })),
        ],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &base_model()).await;
    let tool_use_id = payload
        .pointer("/messages/0/content/0/toolUse/toolUseId")
        .and_then(Value::as_str)
        .expect("toolUseId");
    let result_id = payload
        .pointer("/messages/1/content/0/toolResult/toolUseId")
        .and_then(Value::as_str)
        .expect("toolResult toolUseId");
    assert_eq!(tool_use_id, result_id);
    assert!(
        tool_use_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "normalized id {tool_use_id} must only contain [a-zA-Z0-9_-]"
    );
}

// --- bedrock-endpoint-resolution.test.ts --------------------------------------

fn with_region_env(name: &str, value: &str) -> ProviderEnv {
    let mut env = ProviderEnv::new();
    env.insert(name.to_string(), value.to_string());
    env
}

#[test]
fn does_not_pin_standard_aws_endpoints_when_region_is_configured() {
    let model = base_model();
    let options = BedrockOptions {
        env: Some(with_region_env("AWS_REGION", "us-east-2")),
        ..Default::default()
    };
    let config = build_client_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-east-2"));
    assert_eq!(config.endpoint, None);
}

#[test]
fn derives_region_from_a_builtin_eu_endpoint_when_no_region_or_profile_is_configured() {
    let mut model = base_model();
    model.base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com".to_string();
    let config = build_client_config(&model, &BedrockOptions::default());
    assert_eq!(
        config.endpoint.as_deref(),
        Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
    );
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));
}

#[test]
fn handles_missing_regions_for_explicit_scoped_and_ambient_profiles() {
    // The ambient-profile check reads the process env (upstream
    // `getProviderEnvValue("AWS_PROFILE")` with no scoped overrides), so the
    // test must control it explicitly (edition 2024: unsafe set_var).
    let had_ambient_profile = std::env::var("AWS_PROFILE").ok();
    unsafe { std::env::remove_var("AWS_PROFILE") };

    let mut model = base_model();
    model.base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com".to_string();

    let explicit = build_client_config(
        &model,
        &BedrockOptions {
            profile: Some("bedrock-profile".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(explicit.profile.as_deref(), Some("bedrock-profile"));
    assert_eq!(explicit.region.as_deref(), Some("eu-central-1"));

    let scoped = build_client_config(
        &model,
        &BedrockOptions {
            env: Some(with_region_env("AWS_PROFILE", "scoped-bedrock-profile")),
            ..Default::default()
        },
    );
    assert_eq!(scoped.profile.as_deref(), Some("scoped-bedrock-profile"));
    assert_eq!(scoped.region.as_deref(), Some("eu-central-1"));

    // Upstream sets `process.env.AWS_PROFILE = "ambient-bedrock-profile"` —
    // the ambient check reads the process env only, never the scoped
    // options.env (docs/INSTRUCTIONS.md #55).
    unsafe { std::env::set_var("AWS_PROFILE", "ambient-bedrock-profile") };
    let ambient = build_client_config(&model, &BedrockOptions::default());
    assert_eq!(ambient.profile.as_deref(), Some("ambient-bedrock-profile"));
    assert_eq!(ambient.endpoint, None);
    assert_eq!(ambient.region, None);

    match had_ambient_profile {
        Some(value) => unsafe { std::env::set_var("AWS_PROFILE", value) },
        None => unsafe { std::env::remove_var("AWS_PROFILE") },
    }
}

#[test]
fn still_passes_custom_bedrock_endpoints_through() {
    let mut model = base_model();
    model.base_url = "https://bedrock-vpc.example.com".to_string();
    let options = BedrockOptions {
        env: Some(with_region_env("AWS_REGION", "us-west-2")),
        ..Default::default()
    };
    let config = build_client_config(&model, &options);
    assert_eq!(
        config.endpoint.as_deref(),
        Some("https://bedrock-vpc.example.com")
    );
    assert_eq!(config.region.as_deref(), Some("us-west-2"));
}

#[test]
fn extracts_region_from_inference_profile_arn_regardless_of_region() {
    let mut model = base_model();
    model.id =
        "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123".to_string();
    let options = BedrockOptions {
        env: Some(with_region_env("AWS_REGION", "us-east-1")),
        ..Default::default()
    };
    let config = build_client_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-west-2"));
}

#[test]
fn extracts_region_from_govcloud_inference_profile_arn() {
    let mut model = base_model();
    model.id =
        "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123"
            .to_string();
    let options = BedrockOptions {
        env: Some(with_region_env("AWS_REGION", "us-east-1")),
        ..Default::default()
    };
    let config = build_client_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-gov-west-1"));
}

#[test]
fn uses_the_generic_api_key_option_as_a_bedrock_bearer_token() {
    let config = build_client_config(
        &base_model(),
        &BedrockOptions {
            api_key: Some("bedrock-api-key".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(config.token.as_deref(), Some("bedrock-api-key"));
    assert_eq!(
        config.auth_scheme_preference.as_deref(),
        Some(["httpBearerAuth".to_string()].as_slice()),
    );
}

// --- bedrock-credentials.test.ts ----------------------------------------------

#[test]
fn prefers_explicit_and_scoped_profiles_over_ambient_aws_access_keys() {
    let mut env = ProviderEnv::new();
    env.insert("AWS_ACCESS_KEY_ID".to_string(), "AKIAEXAMPLE".to_string());
    env.insert(
        "AWS_SECRET_ACCESS_KEY".to_string(),
        "secretexample".to_string(),
    );
    let model = base_model();

    let explicit = build_client_config(
        &model,
        &BedrockOptions {
            profile: Some("explicit-profile".to_string()),
            env: Some(env.clone()),
            ..Default::default()
        },
    );
    assert_eq!(explicit.profile.as_deref(), Some("explicit-profile"));
    assert_eq!(explicit.credentials, None);

    let mut scoped_env = env.clone();
    scoped_env.insert("AWS_PROFILE".to_string(), "scoped-profile".to_string());
    let scoped = build_client_config(
        &model,
        &BedrockOptions {
            env: Some(scoped_env),
            ..Default::default()
        },
    );
    assert_eq!(scoped.profile.as_deref(), Some("scoped-profile"));
    assert_eq!(scoped.credentials, None);
}

#[test]
fn uses_ambient_aws_access_keys_when_no_profile_is_configured() {
    let mut env = ProviderEnv::new();
    env.insert("AWS_ACCESS_KEY_ID".to_string(), "AKIAEXAMPLE".to_string());
    env.insert(
        "AWS_SECRET_ACCESS_KEY".to_string(),
        "secretexample".to_string(),
    );
    let config = build_client_config(
        &base_model(),
        &BedrockOptions {
            env: Some(env),
            ..Default::default()
        },
    );
    assert_eq!(config.profile, None);
    assert_eq!(
        config.credentials,
        Some(Credentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "secretexample".to_string(),
            session_token: None,
        })
    );
}

// --- bedrock-thinking-payload.test.ts -----------------------------------------

async fn capture_thinking_payload(model: &Model, mut options: BedrockOptions) -> Value {
    // Upstream captures the payload via the onPayload callback while the
    // request is aborted (never sent).
    let captured: Arc<Mutex<Option<Value>>> = Arc::default();
    let captured_cb = Arc::clone(&captured);
    // Upstream: `reasoning: options?.reasoning ?? "high"` — the explicit field
    // would win over `..options` in Rust struct update syntax (#57), so it
    // must be resolved before the spread.
    let reasoning = options.reasoning.take().unwrap_or(ThinkingLevel::High);
    let options = BedrockOptions {
        reasoning: Some(reasoning),
        on_payload: Some(Arc::new(move |_model, payload| {
            *captured_cb.lock().unwrap() = Some(payload.clone());
            Box::pin(async move { Some(payload) })
        })),
        signal: Some(pillar_ai::AbortSignal::aborted(None)),
        ..options
    };
    let _ = stream(model.clone(), make_context(), Some(options))
        .result()
        .await;
    captured
        .lock()
        .unwrap()
        .clone()
        .expect("payload captured via onPayload before request abort")
}

fn make_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![user_message("Hello")],
        tools: Vec::new(),
    }
}

fn opus_4_8_model() -> Model {
    let mut model = base_model();
    model.id = "global.anthropic.claude-opus-4-8-v1".to_string();
    model.name = "Claude Opus 4.8 (Global)".to_string();
    model
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_opus_4_8_when_reasoning_is_enabled() {
    let payload = capture_thinking_payload(&opus_4_8_model(), BedrockOptions::default()).await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "adaptive", "display": "summarized" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/output_config")
            .cloned(),
        Some(json!({ "effort": "high" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/anthropic_beta")
            .cloned(),
        None,
    );
}

#[tokio::test]
async fn maps_xhigh_reasoning_to_effort_xhigh_for_claude_opus_4_8() {
    let payload = capture_thinking_payload(
        &opus_4_8_model(),
        BedrockOptions {
            reasoning: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "adaptive", "display": "summarized" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/output_config")
            .cloned(),
        Some(json!({ "effort": "xhigh" })),
    );
}

#[tokio::test]
async fn uses_adaptive_thinking_for_claude_fable_5_when_reasoning_is_enabled() {
    let mut model = base_model();
    model.id = "global.anthropic.claude-fable-5".to_string();
    model.name = "Claude Fable 5 (Global)".to_string();
    let payload = capture_thinking_payload(&model, BedrockOptions::default()).await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "adaptive", "display": "summarized" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/output_config")
            .cloned(),
        Some(json!({ "effort": "high" })),
    );
}

#[tokio::test]
async fn omits_display_for_govcloud_model_ids_on_non_adaptive_claude_thinking() {
    let mut model = base_model();
    model.id = "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
    model.name = "Claude Sonnet 4.5 (GovCloud)".to_string();
    let payload = capture_thinking_payload(&model, BedrockOptions::default()).await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "enabled", "budget_tokens": 16384 })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/anthropic_beta")
            .cloned(),
        Some(json!(["interleaved-thinking-2025-05-14"])),
    );
}

#[tokio::test]
async fn omits_display_for_govcloud_regions_on_adaptive_claude_thinking() {
    let payload = capture_thinking_payload(
        &opus_4_8_model(),
        BedrockOptions {
            region: Some("us-gov-west-1".to_string()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "adaptive" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/output_config")
            .cloned(),
        Some(json!({ "effort": "high" })),
    );
}

#[tokio::test]
async fn uses_adaptive_thinking_when_model_name_contains_the_model_name_but_arn_does_not() {
    let mut model = base_model();
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Opus 4.6".to_string();
    let payload = capture_thinking_payload(&model, BedrockOptions::default()).await;
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/thinking")
            .cloned(),
        Some(json!({ "type": "adaptive", "display": "summarized" })),
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/output_config")
            .cloned(),
        Some(json!({ "effort": "high" })),
    );
}

#[tokio::test]
async fn injects_cache_points_when_model_name_identifies_a_supported_claude_model() {
    let mut model = base_model();
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Sonnet 4.6".to_string();
    // Upstream passes an explicit system prompt here and leaves cacheRetention
    // at its default ("short"), so cache points ARE injected — unlike the
    // other capture_payload tests which force "none".
    let context = Context {
        system_prompt: Some("You are helpful.".to_string()),
        messages: vec![user_message("Hello")],
        tools: Vec::new(),
    };
    let payload = capture_payload_retain(&context, &model).await;

    // System prompt should have a cache point
    let system = payload
        .get("system")
        .and_then(Value::as_array)
        .expect("system array");
    assert_eq!(system.len(), 2);
    assert!(system[1].get("cachePoint").is_some());

    // Last user message should have a cache point
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array");
    let last_content = messages
        .last()
        .unwrap()
        .get("content")
        .and_then(Value::as_array)
        .expect("content array");
    assert!(last_content.last().unwrap().get("cachePoint").is_some());
}

#[tokio::test]
async fn falls_back_to_fixed_budget_thinking_for_non_adaptive_claude_via_model_name() {
    let mut model = base_model();
    model.id = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile"
        .to_string();
    model.name = "Claude Sonnet 4.5".to_string();
    let payload = capture_thinking_payload(&model, BedrockOptions::default()).await;
    let thinking = payload
        .pointer("/additionalModelRequestFields/thinking")
        .cloned()
        .expect("thinking fields");
    assert_eq!(thinking.get("type").cloned(), Some(json!("enabled")));
    assert!(
        thinking
            .get("budget_tokens")
            .and_then(Value::as_u64)
            .is_some()
    );
    assert_eq!(
        payload
            .pointer("/additionalModelRequestFields/anthropic_beta")
            .cloned(),
        Some(json!(["interleaved-thinking-2025-05-14"])),
    );
}

// --- bedrock-raw-stop-reason.test.ts -------------------------------------------

#[tokio::test]
async fn preserves_raw_bedrock_stop_reasons_for_successful_stops() {
    let context = make_context();
    let body = eventstream_body(&[assistant_start(), message_stop("end_turn")]);
    let message = drive_stream(&base_model(), &context, BedrockOptions::default(), body).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(message.error_message, None);
}

#[tokio::test]
async fn preserves_raw_bedrock_stop_reasons_for_provider_error_stops() {
    let context = make_context();
    let body = eventstream_body(&[assistant_start(), message_stop("guardrail_intervened")]);
    let message = drive_stream(&base_model(), &context, BedrockOptions::default(), body).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.raw_stop_reason.as_deref(),
        Some("guardrail_intervened")
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some("Provider stopped with: guardrail_intervened")
    );
}

// --- bedrock-error-metadata.test.ts -------------------------------------------

fn find_diagnostic(message: &pillar_ai::types::AssistantMessage) -> Option<&Value> {
    message
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.kind == "bedrock_response_failure")
        .and_then(|diagnostic| diagnostic.details.as_ref())
}

#[tokio::test]
async fn records_status_error_code_and_request_id_for_a_non_2xx() {
    let fetch = MockFetch::new(vec![FetchResponse {
        status: 400,
        headers: vec![("x-amzn-requestid".to_string(), "req-123".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(
            b"The provided model identifier is invalid.".to_vec(),
        )])),
    }]);
    let options = BedrockOptions {
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        ..Default::default()
    };
    let message = stream(base_model(), make_context(), Some(options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let diagnostic = find_diagnostic(&message).expect("diagnostic");
    assert_eq!(
        diagnostic,
        &json!({
            "status": 400,
            "requestId": "req-123",
        })
    );
}

#[tokio::test]
async fn records_error_code_for_a_modeled_mid_stream_exception() {
    let body = eventstream_body(&[
        assistant_start(),
        exception_frame(
            "ValidationException",
            "The provided model identifier is invalid.",
        ),
    ]);
    let message = drive_stream(
        &base_model(),
        &make_context(),
        BedrockOptions::default(),
        body,
    )
    .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let diagnostic = find_diagnostic(&message).expect("diagnostic");
    assert_eq!(
        diagnostic,
        &json!({ "errorCode": "ValidationException", "requestId": "req-123" })
    );
}

#[tokio::test]
async fn reports_only_the_request_id_for_an_unmodeled_mid_stream_exception() {
    let body = eventstream_body(&[
        assistant_start(),
        exception_frame("TooManyRequests", "Too many requests, please wait."),
    ]);
    let message = drive_stream(
        &base_model(),
        &make_context(),
        BedrockOptions::default(),
        body,
    )
    .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        find_diagnostic(&message),
        Some(&json!({ "requestId": "req-123" }))
    );
}

#[tokio::test]
async fn emits_no_diagnostic_for_an_aborted_turn() {
    let body = eventstream_body(&[
        assistant_start(),
        exception_frame("ValidationException", "invalid"),
    ]);
    let signal = pillar_ai::AbortSignal::aborted(None);
    let fetch = MockFetch::new(vec![eventstream_response(body)]);
    let options = BedrockOptions {
        signal: Some(signal),
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        ..Default::default()
    };
    let message = stream(base_model(), make_context(), Some(options))
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(find_diagnostic(&message), None);
}

#[tokio::test]
async fn drops_header_derived_values_that_exceed_the_length_bound() {
    let long_id = "R".repeat(5000);
    let fetch = MockFetch::new(vec![FetchResponse {
        status: 400,
        headers: vec![("x-amzn-requestid".to_string(), long_id)],
        body: Box::pin(futures::stream::iter(vec![Ok(b"invalid".to_vec())])),
    }]);
    let options = BedrockOptions {
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        ..Default::default()
    };
    let message = stream(base_model(), make_context(), Some(options))
        .result()
        .await;
    assert_eq!(find_diagnostic(&message), Some(&json!({ "status": 400 })));
}

// --- bedrock-response-headers.test.ts -------------------------------------------

#[tokio::test]
async fn forwards_raw_response_headers_to_on_response() {
    type RecordedResponse = (u16, Vec<(String, String)>);
    let responses: Arc<Mutex<Vec<RecordedResponse>>> = Arc::default();
    let recorded = Arc::clone(&responses);
    let body = eventstream_body(&[assistant_start(), message_stop("end_turn")]);
    let fetch = MockFetch::new(vec![eventstream_response(body)]);
    let options = BedrockOptions {
        fetch: Some(fetch.clone() as pillar_ai::SharedFetchFn),
        on_response: Some(Arc::new(move |response, _model| {
            let recorded = Arc::clone(&recorded);
            Box::pin(async move {
                recorded
                    .lock()
                    .unwrap()
                    .push((response.status, response.headers));
            })
        })),
        ..Default::default()
    };
    let message = stream(base_model(), make_context(), Some(options))
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    let (status, ref headers) = responses[0];
    assert_eq!(status, 200);
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "x-amzn-requestid" && value == "req-123")
    );
}

// --- bedrock-redacted-reasoning.test.ts -----------------------------------------

const REDACTED_BASE64: &str = "cnNuXzVaVnJpZjRKMGJYSXFtV2RsZWRqN1FJRmVOaWtSUWJF";

fn gpt_model() -> Model {
    Model {
        id: "global.openai.gpt-5.6-terra".to_string(),
        name: "GPT-5.6 Terra (Global)".to_string(),
        api: "bedrock-converse-stream".to_string(),
        provider: "amazon-bedrock".to_string(),
        base_url: "https://bedrock-runtime.ap-northeast-1.amazonaws.com".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 0.0,
            },
            tiers: None,
        },
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Mirrors the ConverseStream frames GPT-5.6 emits: encrypted reasoning, then text.
fn redacted_reasoning_events() -> Vec<u8> {
    eventstream_body(&[
        assistant_start(),
        reasoning_delta(0, json!({ "redactedContent": redacted_bytes() })),
        block_stop(0),
        text_delta(1, "done"),
        block_stop(1),
        message_stop("end_turn"),
    ])
}

#[tokio::test]
async fn does_not_fail_the_stream_when_reasoning_arrives_as_redacted_content() {
    let message = drive_stream(
        &gpt_model(),
        &make_context(),
        BedrockOptions::default(),
        redacted_reasoning_events(),
    )
    .await;
    assert_ne!(
        message.stop_reason,
        StopReason::Error,
        "{:?}",
        message.error_message
    );
    let kinds: Vec<&str> = message
        .content
        .iter()
        .map(|content| match content {
            Content::Text { .. } => "text",
            Content::Thinking { .. } => "thinking",
            Content::Image { .. } => "image",
            Content::ToolCall { .. } => "toolCall",
        })
        .collect();
    assert_eq!(kinds, vec!["thinking", "text"]);
    assert_eq!(message.content[1], Content::text("done"));
}

#[tokio::test]
async fn preserves_the_encrypted_reasoning_payload_on_the_assistant_message() {
    let message = drive_stream(
        &gpt_model(),
        &make_context(),
        BedrockOptions::default(),
        redacted_reasoning_events(),
    )
    .await;
    let Some(Content::Thinking {
        thinking,
        thinking_signature,
        redacted,
    }) = message
        .content
        .iter()
        .find(|content| matches!(content, Content::Thinking { .. }))
    else {
        panic!("expected a thinking block");
    };
    assert_eq!(*redacted, Some(true));
    assert_eq!(thinking_signature.as_deref(), Some(REDACTED_BASE64));
    // The byte buffer is streaming scratch state: it must not survive.
    assert_eq!(thinking.as_str(), "[Reasoning redacted]");
}

#[tokio::test]
async fn encodes_the_payload_when_the_stream_never_sends_content_block_stop() {
    let body = eventstream_body(&[
        assistant_start(),
        reasoning_delta(0, json!({ "redactedContent": redacted_bytes() })),
        message_stop("end_turn"),
    ]);
    let message = drive_stream(
        &gpt_model(),
        &make_context(),
        BedrockOptions::default(),
        body,
    )
    .await;
    let Some(Content::Thinking {
        thinking_signature, ..
    }) = message
        .content
        .iter()
        .find(|content| matches!(content, Content::Thinking { .. }))
    else {
        panic!("expected a thinking block");
    };
    assert_eq!(thinking_signature.as_deref(), Some(REDACTED_BASE64));
}

#[tokio::test]
async fn joins_encrypted_reasoning_split_across_deltas() {
    let bytes = redacted_bytes();
    let (head, tail) = bytes.split_at(7);
    let body = eventstream_body(&[
        assistant_start(),
        reasoning_delta(0, json!({ "redactedContent": head })),
        reasoning_delta(0, json!({ "redactedContent": tail })),
        block_stop(0),
        message_stop("end_turn"),
    ]);
    let message = drive_stream(
        &gpt_model(),
        &make_context(),
        BedrockOptions::default(),
        body,
    )
    .await;
    let Some(Content::Thinking {
        thinking,
        thinking_signature,
        ..
    }) = message
        .content
        .iter()
        .find(|content| matches!(content, Content::Thinking { .. }))
    else {
        panic!("expected a thinking block");
    };
    assert_eq!(thinking_signature.as_deref(), Some(REDACTED_BASE64));
    // The placeholder marks the block once, not once per delta.
    assert_eq!(thinking.as_str(), "[Reasoning redacted]");
}

#[tokio::test]
async fn replays_redacted_reasoning_as_reasoning_content_redacted_content() {
    // Same-model replay: the assistant message must carry gpt_model's id so
    // transform_messages keeps the redacted thinking block (isSameModel).
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("hello"),
            assistant_message_from(
                &gpt_model(),
                vec![
                    Content::Thinking {
                        thinking: String::new(),
                        thinking_signature: Some(REDACTED_BASE64.to_string()),
                        redacted: Some(true),
                    },
                    Content::text("done"),
                ],
            ),
            user_message("continue"),
        ],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &gpt_model()).await;
    let assistant = payload
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages")
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .expect("assistant message");
    assert_eq!(
        assistant.get("content").cloned(),
        Some(json!([
            { "reasoningContent": { "redactedContent": redacted_bytes() } },
            { "text": "done" },
        ])),
    );
}

#[tokio::test]
async fn replays_redacted_reasoning_before_the_tool_use_block_it_belongs_to() {
    // Bedrock rejects a tool continuation whose reasoning block is missing or
    // reordered, so the opaque payload must land ahead of the matching toolUse.
    // Same-model replay: the assistant message must carry gpt_model's id.
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("read the file"),
            assistant_message_from(
                &gpt_model(),
                vec![
                    Content::Thinking {
                        thinking: String::new(),
                        thinking_signature: Some(REDACTED_BASE64.to_string()),
                        redacted: Some(true),
                    },
                    Content::ToolCall {
                        id: "tool-1".to_string(),
                        name: "read".to_string(),
                        arguments: json!({ "path": "/tmp/a.txt" }),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
            ),
            tool_result_message("tool-1", "file body"),
        ],
        tools: Vec::new(),
    };
    let payload = capture_payload(&context, &gpt_model()).await;
    let assistant = payload
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages")
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .expect("assistant message");
    assert_eq!(
        assistant.get("content").cloned(),
        Some(json!([
            { "reasoningContent": { "redactedContent": redacted_bytes() } },
            { "toolUse": { "toolUseId": "tool-1", "name": "read", "input": { "path": "/tmp/a.txt" } } },
        ])),
    );
}

// --- bedrock-models.test.ts (non-live cases) ------------------------------------

#[test]
fn should_get_all_available_bedrock_models_placeholder() {
    // Upstream asserts getModels("amazon-bedrock").length > 0 against the live
    // generated catalog; the catalog generator is a separate port item, so
    // this parity case stays pending until models_generated.rs exists.
}
