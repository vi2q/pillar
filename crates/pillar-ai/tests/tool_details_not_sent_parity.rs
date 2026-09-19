//! Wire-level guard: a tool result's `details` must never reach a provider
//! request body.
//!
//! `ToolResultMessage` carries `details` — the edit tool's diff/patch, the read
//! tool's truncation metadata — which the session stores and the TUI renders.
//! Those payloads are large (a diff of the whole file, per edit), so a provider
//! that serialized them would inflate every following request.
//!
//! `docs/TOOL-EFFICIENCY-DESIGN.md` §6.2 asks for this to be checked by
//! capturing the real request rather than by inferring from the field name:
//! every assertion below puts a sentinel in `details` and looks for it in the
//! provider's own request output.
//!
//! Coverage: openai-completions (the shape the OpenAI-compatible providers
//! actually use) is checked against the captured HTTP body; anthropic-messages
//! against `build_params`, the function that produces its body. The remaining
//! provider modules were inspected for `details` use and reference it nowhere.
//! `crates/pillar-agent/tests/tool_details_not_sent_parity.rs` covers the other
//! path that turns history into model input (the compaction serializer).

#![cfg(feature = "providers")]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use pillar_ai::api::anthropic_messages::{AnthropicOptions, build_params};
use pillar_ai::api::openai_completions::{OpenaiCompletionsOptions, stream};
use pillar_ai::event_stream::collect_events;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, ModelCost, StopReason, ToolResultMessage,
    Usage, UserContent,
};

/// A string that only ever exists inside `details`.
const SENTINEL: &str = "pillar-details-sentinel-9f2c";

/// The tool result text the model is supposed to see instead.
const RESULT_TEXT: &str = "Successfully replaced 1 block(s) in f.txt.";

type Captured = Arc<AsyncMutex<Vec<FetchRequest>>>;

/// Transport that records every request and answers with a canned
/// openai-completions stream: the request is what this file asserts on.
struct MockFetch {
    captured: Captured,
}

impl MockFetch {
    fn new() -> (Self, Captured) {
        let captured: Captured = Arc::default();
        (
            Self {
                captured: Arc::clone(&captured),
            },
            captured,
        )
    }
}

#[async_trait]
impl FetchFn for MockFetch {
    async fn fetch(
        &self,
        request: FetchRequest,
    ) -> Result<FetchResponse, pillar_ai::error::AiError> {
        self.captured.lock().await.push(request);
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},",
            "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,",
            "\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
        let chunks: Vec<Result<Vec<u8>, pillar_ai::error::AiError>> =
            vec![Ok(body.as_bytes().to_vec()), Ok(Vec::new())];
        Ok(FetchResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(futures::stream::iter(chunks)),
        })
    }
}

fn base_model(api: &str, provider: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// The conversation an edit leaves behind: the assistant asked for the edit,
/// and the tool result's `content` is what the model sees while its `details`
/// carry the diff the TUI renders.
fn edit_conversation() -> Context {
    Context {
        system_prompt: None,
        messages: vec![
            Message::User {
                content: UserContent::Text("replace the block".to_string()),
                timestamp: 1,
            },
            Message::Assistant(Box::new(AssistantMessage {
                content: vec![Content::tool_call(
                    "call-1",
                    "edit",
                    json!({"path": "f.txt"}),
                )],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1,
            })),
            Message::ToolResult(Box::new(ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                content: vec![Content::text(RESULT_TEXT)],
                details: Some(json!({
                    "diff": format!("-old\n+{SENTINEL}"),
                    "patch": format!("--- f.txt\n+++ f.txt\n+{SENTINEL}"),
                    "firstChangedLine": 1,
                })),
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 1,
            })),
        ],
        tools: vec![],
    }
}

fn assert_details_absent(request: &str) {
    assert!(
        request.contains(RESULT_TEXT),
        "the tool result content must reach the request: {request}"
    );
    assert!(request.contains("call-1"), "{request}");
    assert!(
        !request.contains(SENTINEL),
        "tool result details reached the provider request: {request}"
    );
}

#[tokio::test]
async fn openai_completions_request_body_excludes_tool_result_details() {
    let (mock, captured) = MockFetch::new();
    let model = base_model("openai-completions", "openai", "https://api.openai.test/v1");

    let s = stream(
        model,
        edit_conversation(),
        Some(OpenaiCompletionsOptions {
            api_key: Some("test-key".to_string()),
            fetch: Some(Arc::new(mock)),
            ..Default::default()
        }),
    );
    let _ = collect_events(&s).await;

    let requests = captured.lock().await;
    let request = requests.first().expect("the provider sent a request");
    let body = String::from_utf8_lossy(request.body.as_deref().expect("request body"));
    assert_details_absent(&body);
}

#[tokio::test]
async fn anthropic_messages_request_params_exclude_tool_result_details() {
    let model = base_model(
        "anthropic-messages",
        "anthropic",
        "https://api.anthropic.test",
    );
    let params = build_params(
        &model,
        &edit_conversation(),
        false,
        &AnthropicOptions::default(),
    );

    assert_details_absent(&params.to_string());
}
