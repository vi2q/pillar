//! Port of the upstream pi-messages tests (pi v0.84.3): text/toolcall event
//! folding, request payload shape, debug flag, response-header reporting,
//! error-response diagnostics, server-sent error events, and terminal-event
//! validation.
//!
//! divergence: upstream spins up a local node:http server; the Rust port
//! injects a `FetchFn` fake and inspects captured `FetchRequest`s.

use std::sync::{Arc, Mutex};

use pillar_ai::api::pi_messages::{PiMessagesOptions, SimpleStreamOptions, stream};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{
    Context, Message, Model, ModelCost, ModelCostRates, StopReason, UserContent,
};

// --- Helpers ---------------------------------------------------------------

#[derive(Clone)]
struct Recorded {
    url: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

struct CapturedFetch {
    requests: Mutex<Vec<Recorded>>,
    response_factory: Box<dyn Fn() -> Result<FetchResponse, AiError> + Send + Sync>,
}

impl CapturedFetch {
    fn new(
        response_factory: impl Fn() -> Result<FetchResponse, AiError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            response_factory: Box::new(response_factory),
        })
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl FetchFn for CapturedFetch {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError> {
        let body: serde_json::Value = request
            .body
            .as_deref()
            .map(|body| serde_json::from_slice(body).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null);
        self.requests.lock().unwrap().push(Recorded {
            url: request.url.clone(),
            headers: request.headers.clone(),
            body,
        });
        (self.response_factory)()
    }
}

fn json_response(status: u16, body: serde_json::Value) -> FetchResponse {
    FetchResponse {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(
            serde_json::to_vec(&body).unwrap()
        )])),
    }
}

fn events_response(headers: Vec<(String, String)>, events: &[serde_json::Value]) -> FetchResponse {
    let body = events
        .iter()
        .map(|event| format!("data: {}\n\n", event))
        .collect::<String>();
    FetchResponse {
        status: 200,
        headers,
        body: Box::pin(futures::stream::iter(vec![Ok(body.into_bytes())])),
    }
}

fn model(base_url: &str) -> Model {
    Model {
        id: "auto".to_string(),
        name: "Radius Auto".to_string(),
        api: "pi-messages".to_string(),
        provider: "radius".to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.2,
            },
            tiers: None,
        },
        context_window: 128_000,
        max_tokens: 16_384,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User {
            content: UserContent::Text("Hello".to_string()),
            timestamp: 1,
        }],
        tools: Vec::new(),
    }
}

fn usage_json() -> serde_json::Value {
    serde_json::json!({
        "input": 10,
        "output": 5,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 15,
        "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3 },
    })
}

fn text_stream_events() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "type": "start" }),
        serde_json::json!({ "type": "text_start", "contentIndex": 0 }),
        serde_json::json!({ "type": "text_delta", "contentIndex": 0, "delta": "Hel" }),
        serde_json::json!({ "type": "text_delta", "contentIndex": 0, "delta": "lo" }),
        serde_json::json!({ "type": "text_end", "contentIndex": 0, "content": "Hello" }),
        serde_json::json!({ "type": "toolcall_start", "contentIndex": 1, "id": "call_1", "toolName": "read" }),
        serde_json::json!({ "type": "toolcall_delta", "contentIndex": 1, "delta": "{\"path\":" }),
        serde_json::json!({ "type": "toolcall_delta", "contentIndex": 1, "delta": "\"a.txt\"}" }),
        serde_json::json!({
            "type": "toolcall_end",
            "contentIndex": 1,
            "toolCall": { "type": "toolCall", "id": "call_1", "name": "read", "arguments": { "path": "a.txt" } },
        }),
        serde_json::json!({ "type": "done", "reason": "toolUse", "usage": usage_json(), "responseId": "resp_1" }),
    ]
}

// --- Tests -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn streams_text_and_tool_calls_and_resolves_the_terminal_message() {
    let fetch = CapturedFetch::new(|| Ok(events_response(vec![], &text_stream_events())));
    let options = PiMessagesOptions {
        api_key: Some("test-key".to_string()),
        session_id: Some("session-1".to_string()),
        tool_choice: Some(serde_json::json!("auto")),
        max_tokens: Some(100),
        headers: Some(pillar_ai::types::ProviderHeaders::from([(
            "x-custom".to_string(),
            Some("1".to_string()),
        )])),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let event_stream = stream(model("http://127.0.0.1:1/v1"), context(), Some(options));
    let message = event_stream.result().await;

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 5);
    assert_eq!(message.usage.total_tokens, 15);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.model, "auto");
    assert_eq!(message.provider, "radius");
    assert_eq!(message.content.len(), 2);
    assert_eq!(
        message.content[0],
        pillar_ai::types::Content::Text {
            text: "Hello".to_string(),
            text_signature: None,
        }
    );
    assert_eq!(
        message.content[1],
        pillar_ai::types::Content::ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "a.txt" }),
            thought_signature: None,
            namespace: None,
        }
    );

    let requests = fetch.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.ends_with("/v1/messages"));
    let header = |name: &str| {
        requests[0]
            .headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("Authorization").as_deref(), Some("Bearer test-key"));
    assert_eq!(header("x-custom").as_deref(), Some("1"));
    let body = &requests[0].body;
    assert_eq!(body.get("model"), Some(&serde_json::json!("auto")));
    assert!(body.get("context").is_some());
    let request_options = body.get("options").expect("options object");
    assert_eq!(
        request_options.get("maxTokens"),
        Some(&serde_json::json!(100))
    );
    assert_eq!(
        request_options.get("sessionId"),
        Some(&serde_json::json!("session-1"))
    );
    assert_eq!(
        request_options.get("toolChoice"),
        Some(&serde_json::json!("auto"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn appends_debug_flag_and_reports_response_headers_via_on_response() {
    let fetch = CapturedFetch::new(|| {
        Ok(events_response(
            vec![(
                "x-pi-gateway-upstream-provider".to_string(),
                "anthropic".to_string(),
            )],
            &[serde_json::json!({ "type": "done", "reason": "stop", "usage": usage_json() })],
        ))
    });
    let observed = Arc::new(Mutex::new(None::<Vec<(String, String)>>));
    let observed_for_hook = Arc::clone(&observed);
    let options = SimpleStreamOptions {
        api_key: Some("test-key".to_string()),
        on_response: Some(Arc::new(move |info, _model| {
            *observed_for_hook.lock().unwrap() = Some(info.headers.clone());
            Box::pin(async {})
        })),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    // debug is a PiMessagesOptions-only field upstream too; pass it directly.
    let pi_options = PiMessagesOptions {
        api_key: Some("test-key".to_string()),
        debug: Some(true),
        on_response: options.on_response.clone(),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let message = stream(model("http://127.0.0.1:2/v1"), context(), Some(pi_options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = fetch.requests();
    assert!(requests[0].url.ends_with("/v1/messages?debug=1"));
    let observed = observed.lock().unwrap().clone().expect("onResponse called");
    assert!(observed.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("x-pi-gateway-upstream-provider") && value == "anthropic"
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn surfaces_backend_error_responses_with_diagnostics() {
    let fetch = CapturedFetch::new(|| {
        Ok(json_response(
            401,
            serde_json::json!({ "error": { "message": "Token expired", "code": "unauthorized" } }),
        ))
    });
    let options = PiMessagesOptions {
        api_key: Some("stale".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let message = stream(model("http://127.0.0.1:3/v1"), context(), Some(options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.unwrap_or_default();
    assert!(error_message.contains("401"), "unexpected: {error_message}");
    assert!(
        error_message.contains("Token expired"),
        "unexpected: {error_message}"
    );
    assert!(
        error_message.contains("unauthorized"),
        "unexpected: {error_message}"
    );
    assert_eq!(message.diagnostics.len(), 1);
    assert_eq!(message.diagnostics[0].kind, "pi_messages_response_failure");
    let details = message.diagnostics[0].details.as_ref().expect("details");
    assert_eq!(details.get("status"), Some(&serde_json::json!(401)));
}

#[tokio::test(flavor = "multi_thread")]
async fn propagates_server_sent_error_events() {
    let fetch = CapturedFetch::new(|| {
        Ok(events_response(
            vec![],
            &[
                serde_json::json!({ "type": "start" }),
                serde_json::json!({
                    "type": "error",
                    "reason": "error",
                    "usage": usage_json(),
                    "errorMessage": "Upstream failed",
                }),
            ],
        ))
    });
    let options = PiMessagesOptions {
        api_key: Some("test-key".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let message = stream(model("http://127.0.0.1:4/v1"), context(), Some(options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Upstream failed"));
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_when_no_api_key_is_provided() {
    let fetch = CapturedFetch::new(|| Ok(events_response(vec![], &[])));
    let options = PiMessagesOptions {
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let message = stream(model("http://127.0.0.1:5/v1"), context(), Some(options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .unwrap_or_default()
            .contains("No API key provided")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_when_the_stream_ends_without_a_terminal_event() {
    let fetch = CapturedFetch::new(|| {
        Ok(events_response(
            vec![],
            &[
                serde_json::json!({ "type": "start" }),
                serde_json::json!({ "type": "text_start", "contentIndex": 0 }),
                serde_json::json!({ "type": "text_delta", "contentIndex": 0, "delta": "partial" }),
            ],
        ))
    });
    let options = PiMessagesOptions {
        api_key: Some("test-key".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let message = stream(model("http://127.0.0.1:6/v1"), context(), Some(options))
        .result()
        .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .unwrap_or_default()
            .contains("stream ended without a terminal event")
    );
}
