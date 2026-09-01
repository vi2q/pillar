//! Port of the upstream openai-codex-stream tests (pi v0.84.3) that run
//! without live ChatGPT credentials: SSE payload decoding, header/session
//! handling, retry behavior, zstd compression, and WebSocket transport
//! fallback/continuation logic.
//!
//! divergence: upstream stubs the global `fetch` and `WebSocket` globals;
//! the Rust port injects a `FetchFn` fake and a `WsConnFactory` fake, and
//! inspects captured `FetchRequest`s and WebSocket frames instead. Live
//! cases (oauth, cache-affinity e2e) are not portable.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pillar_ai::api::openai_codex_responses::{
    CodexSimpleStreamOptions, OpenaiCodexResponsesOptions, WsConn, WsConnFactory,
    get_openai_codex_websocket_debug_stats, reset_openai_codex_websocket_debug_stats, stream,
    stream_simple,
};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{Context, Model, ModelCompat, ModelCost, StopReason, Transport};

// --- Helpers ---------------------------------------------------------------

struct CapturedFetch {
    requests: Mutex<Vec<FetchRequest>>,
    responses: Mutex<VecDeque<FetchResponse>>,
}

impl CapturedFetch {
    fn new(responses: Vec<FetchResponse>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        })
    }
}

#[async_trait::async_trait]
impl FetchFn for CapturedFetch {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AiError::Other("no scripted response".to_string()))
    }
}

fn sse_response(body: &str) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(body.as_bytes().to_vec())])),
    }
}

fn json_response(status: u16, body: &str, extra_headers: Vec<(String, String)>) -> FetchResponse {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    headers.extend(extra_headers);
    FetchResponse {
        status,
        headers,
        body: Box::pin(futures::stream::iter(vec![Ok(body.as_bytes().to_vec())])),
    }
}

/// Upstream `buildSSEPayload` (default usage 5/3/8).
fn build_sse_payload(status: &str, include_done: bool, end_turn: Option<bool>) -> String {
    let terminal_type = if status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let mut events = vec![
        format!(
            "data: {}",
            serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } })
        ),
        format!(
            "data: {}",
            serde_json::json!({ "type": "response.output_text.delta", "delta": "Hello" })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": "Hello" }],
                },
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "type": terminal_type,
                "response": {
                    "status": status,
                    "end_turn": end_turn,
                    "incomplete_details": if status == "incomplete" { serde_json::json!({ "reason": "max_output_tokens" }) } else { serde_json::Value::Null },
                    "usage": {
                        "input_tokens": 5,
                        "output_tokens": 3,
                        "total_tokens": 8,
                        "input_tokens_details": { "cached_tokens": 0 },
                    },
                },
            })
        ),
    ];
    if include_done {
        events.push("data: [DONE]".to_string());
    }
    format!("{}\n\n", events.join("\n\n"))
}

fn model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-codex-responses".to_string(),
        provider: "openai-codex".to_string(),
        base_url: "https://chatgpt.com/backend-api".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: Some(ModelCompat::OpenaiResponses(Default::default())),
    }
}

fn model_with_cost(id: &str, input: f64, output: f64) -> Model {
    let mut model = model(id);
    model.cost.rates.input = input;
    model.cost.rates.output = output;
    model
}

fn context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![pillar_ai::types::Message::User {
            content: pillar_ai::types::UserContent::Text("Say hello".to_string()),
            timestamp: 1,
        }],
        tools: Vec::new(),
    }
}

fn tool_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![pillar_ai::types::Message::User {
            content: pillar_ai::types::UserContent::Text(
                "Do not call ping. Respond with text instead.".to_string(),
            ),
            timestamp: 1,
        }],
        tools: vec![pillar_ai::types::Tool {
            name: "ping".to_string(),
            description: "Ping".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
            }),
            constrained_sampling: None,
        }],
    }
}

/// JWT with `chatgpt_account_id` in the OpenAI auth claim.
fn mock_token(account_id: &str) -> String {
    let payload = base64_url(
        &serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id },
        })
        .to_string(),
    );
    format!("aaa.{payload}.bbb")
}

fn base64_url(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let bytes = input.as_bytes();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        output.push(TABLE[(n >> 18) as usize & 63] as char);
        output.push(TABLE[(n >> 12) as usize & 63] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    output
}

fn request_json(request: &FetchRequest) -> serde_json::Value {
    let body = request.body.as_deref().expect("request body");
    serde_json::from_slice(body).expect("JSON body")
}

fn header_value<'a>(request: &'a FetchRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// --- Mock WebSocket transport -----------------------------------------------

/// Scripted mock WebSocket mirroring upstream's MockWebSocket class: opens
/// immediately, and on the first `send` dispatches scripted messages.
// --- Simplified mock (flattened) --------------------------------------------

#[derive(Default)]
struct MockWsState {
    sent_frames: Mutex<Vec<serde_json::Value>>,
    scripted: Mutex<VecDeque<Vec<serde_json::Value>>>,
    incoming: Mutex<VecDeque<String>>,
    connected_headers: Mutex<Vec<Vec<(String, String)>>>,
    connection_count: Mutex<u32>,
    closed: Mutex<Vec<String>>,
    /// Set once a scripted batch has been dispatched; recv then ends the
    /// stream after draining (mirrors a clean server close).
    terminal_sent: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
struct MockWsConn {
    state: Arc<MockWsState>,
}

#[async_trait::async_trait]
impl WsConn for MockWsConn {
    fn send(&self, data: String) -> Result<(), String> {
        self.state
            .sent_frames
            .lock()
            .unwrap()
            .push(serde_json::from_str(&data).expect("sent frame is JSON"));
        // Scripted messages are queued synchronously; recv observes them on
        // its next poll, which happens after send returns (upstream
        // queueMicrotask semantics without a spawn).
        let scripted = self.state.scripted.lock().unwrap().pop_front();
        if let Some(scripted) = scripted {
            let mut incoming = self.state.incoming.lock().unwrap();
            for message in scripted {
                incoming.push_back(message.to_string());
            }
        }
        self.state
            .terminal_sent
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn close(&self, _code: u16, reason: &str) {
        self.state.closed.lock().unwrap().push(reason.to_string());
    }

    async fn recv(&self) -> Option<Result<String, String>> {
        loop {
            {
                let mut incoming = self.state.incoming.lock().unwrap();
                if let Some(message) = incoming.pop_front() {
                    return Some(Ok(message));
                }
                // Scripted frames exhausted and the terminal event already
                // dispatched: end the stream like a clean server close.
                if self
                    .state
                    .terminal_sent
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && incoming.is_empty()
                {
                    return None;
                }
            }
            if !self.state.closed.lock().unwrap().is_empty() {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn is_reusable(&self) -> bool {
        self.state.closed.lock().unwrap().is_empty()
    }
}

struct MockWsFactory {
    state: Arc<MockWsState>,
    /// Scripted messages per connection (front = first connection).
    script: Mutex<VecDeque<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl WsConnFactory for MockWsFactory {
    async fn connect(
        &self,
        _url: &str,
        headers: &[(String, String)],
        _signal: Option<&pillar_ai::AbortSignal>,
        _connect_timeout_ms: u64,
    ) -> Result<Arc<dyn WsConn>, String> {
        *self.state.connection_count.lock().unwrap() += 1;
        self.state
            .connected_headers
            .lock()
            .unwrap()
            .push(headers.to_vec());
        *self.state.scripted.lock().unwrap() = self.script.lock().unwrap().clone();
        Ok(Arc::new(MockWsConn {
            state: Arc::clone(&self.state),
        }))
    }
}

// --- Tests ------------------------------------------------------------------

fn codex_options(
    token: &str,
    fetch: Arc<CapturedFetch>,
    extra: impl FnOnce(&mut OpenaiCodexResponsesOptions),
) -> OpenaiCodexResponsesOptions {
    let mut options = OpenaiCodexResponsesOptions {
        api_key: Some(token.to_string()),
        transport: Some(Transport::Sse),
        fetch: Some(fetch),
        ..Default::default()
    };
    extra(&mut options);
    options
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_sse_responses_into_assistant_message_event_stream() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    let stream_result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, Arc::clone(&fetch), |_| {})),
    );
    let mut saw_text_delta = false;
    let mut saw_done = false;
    let mut events = stream_result.iter();
    while let Some(event) = futures::StreamExt::next(&mut events).await {
        match event {
            pillar_ai::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                saw_text_delta = true;
                assert_eq!(delta, "Hello");
            }
            pillar_ai::types::AssistantMessageEvent::Done { message, .. } => {
                saw_done = true;
                let text = message
                    .content
                    .iter()
                    .find_map(|content| match content {
                        pillar_ai::types::Content::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .expect("text block");
                assert_eq!(text, "Hello");
            }
            _ => {}
        }
    }

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        request.url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        header_value(request, "authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(
        header_value(request, "chatgpt-account-id"),
        Some("acc_test")
    );
    assert_eq!(
        header_value(request, "openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(header_value(request, "originator"), Some("pi"));
    assert_eq!(header_value(request, "accept"), Some("text/event-stream"));
    assert!(header_value(request, "x-api-key").is_none());

    assert!(saw_text_delta);
    assert!(saw_done);
}

#[tokio::test(flavor = "multi_thread")]
async fn completes_after_response_completed_even_when_the_sse_body_stays_open() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", true, Some(false));
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    let result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch, |_| {})),
    )
    .result()
    .await;

    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            pillar_ai::types::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("text block");
    assert_eq!(text, "Hello");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.end_turn, Some(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_response_incomplete_to_stop_reason_length_even_when_the_sse_body_stays_open() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("incomplete", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    let result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch, |_| {})),
    )
    .result()
    .await;

    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            pillar_ai::types::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("text block");
    assert_eq!(text, "Hello");
    assert_eq!(result.stop_reason, StopReason::Length);
}

#[tokio::test(flavor = "multi_thread")]
async fn aborts_sse_fetch_after_the_configured_http_timeout_when_response_headers_do_not_arrive() {
    let token = mock_token("acc_test");
    // A fetch that never resolves (mirrors upstream's hanging fetch).
    struct HangingFetch;
    #[async_trait::async_trait]
    impl FetchFn for HangingFetch {
        async fn fetch(&self, _request: FetchRequest) -> Result<FetchResponse, AiError> {
            std::future::pending().await
        }
    }

    let result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(OpenaiCodexResponsesOptions {
            api_key: Some(token),
            transport: Some(Transport::Sse),
            fetch: Some(Arc::new(HangingFetch)),
            timeout_ms: Some(10),
            ..Default::default()
        }),
    )
    .result()
    .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Codex SSE response headers timed out after 10ms")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aborts_sse_body_reads_after_response_headers_arrive() {
    let token = mock_token("acc_test");
    let signal = pillar_ai::AbortSignal::new();

    // SSE body that stalls after the first delta; the writer honors abort
    // like upstream's ReadableStream `cancel()` callback.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, AiError>>(4);
    let signal_for_body = signal.clone();
    tokio::spawn(async move {
        let chunk = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
            }),
            serde_json::json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } }),
            serde_json::json!({ "type": "response.output_text.delta", "delta": "one" }),
        );
        tx.send(Ok(chunk.into_bytes())).await.unwrap();
        // Wait for abort; cancelled reads never deliver later chunks.
        signal_for_body.aborted_or_pending().await;
        drop(tx);
    });

    let fetch = CapturedFetch::new(vec![FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })),
    }]);

    let events: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let stream = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(OpenaiCodexResponsesOptions {
            api_key: Some(token),
            transport: Some(Transport::Sse),
            fetch: Some(fetch),
            signal: Some(signal.clone()),
            ..Default::default()
        }),
    );
    let mut stream_iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut stream_iter).await {
        match &event {
            pillar_ai::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                events.lock().unwrap().push(format!("text_delta:{delta}"));
                if delta == "one" {
                    signal.abort(None);
                }
            }
            pillar_ai::types::AssistantMessageEvent::Error { .. } => {
                events.lock().unwrap().push("error".to_string());
                break;
            }
            _ => {}
        }
    }

    let result = stream.result().await;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request was aborted"));
    let events = events.lock().unwrap();
    assert!(events.contains(&"text_delta:one".to_string()));
    assert!(!events.contains(&"text_delta:two".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sets_session_id_headers_and_prompt_cache_key_when_session_id_is_provided() {
    let token = mock_token("acc_test");
    let session_id = "test-session-123";
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch.clone(), |options| {
            options.session_id = Some(session_id.to_string());
        })),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(header_value(request, "session-id"), Some(session_id));
    assert!(header_value(request, "session_id").is_none());
    assert_eq!(
        header_value(request, "x-client-request-id"),
        Some(session_id)
    );
    let body = request_json(request);
    assert_eq!(
        body.get("prompt_cache_key"),
        Some(&serde_json::json!(session_id))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn omits_sse_cache_affinity_when_cache_retention_is_none() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch.clone(), |options| {
            options.cache_retention = Some(pillar_ai::types::CacheRetention::None);
            options.session_id = Some("one-off-summary".to_string());
        })),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert!(header_value(request, "session-id").is_none());
    assert!(header_value(request, "x-client-request-id").is_none());
    let body = request_json(request);
    assert!(body.get("prompt_cache_key").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn clamps_prompt_cache_key_to_openai_64_character_limit() {
    let token = mock_token("acc_test");
    let session_id = "x".repeat(67);
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
    let captured_cb = Arc::clone(&captured);
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch, move |options| {
            options.session_id = Some(session_id);
            let captured = Arc::clone(&captured_cb);
            options.on_payload = Some(Arc::new(move |_model, payload| {
                *captured.lock().unwrap() = Some(payload.clone());
                Box::pin(async move { Some(payload) })
            }));
        })),
    )
    .result()
    .await;

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.as_ref().unwrap().get("prompt_cache_key"),
        Some(&serde_json::json!("x".repeat(64)))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clamps_codex_session_id_header_to_64_characters() {
    let token = mock_token("acc_test");
    let session_id = "x".repeat(67);
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch.clone(), |options| {
            options.session_id = Some(session_id);
        })),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        header_value(request, "session-id"),
        Some("x".repeat(64).as_str())
    );
    assert_eq!(
        header_value(request, "x-client-request-id"),
        Some("x".repeat(64).as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn preserves_gpt_5_5_xhigh_reasoning_effort_from_simple_options() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);
    let mut gpt55 = model("gpt-5.5");
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        pillar_ai::types::ModelThinkingLevel::Xhigh,
        Some("xhigh".to_string()),
    );
    gpt55.thinking_level_map = Some(map);

    stream_simple(
        gpt55,
        context(),
        Some(CodexSimpleStreamOptions {
            api_key: Some(token),
            transport: Some(Transport::Sse),
            fetch: Some(fetch.clone()),
            reasoning: Some(pillar_ai::types::ThinkingLevel::Xhigh),
            ..Default::default()
        }),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let body = request_json(&requests[0]);
    assert_eq!(
        body.get("reasoning"),
        Some(&serde_json::json!({ "effort": "xhigh", "summary": "auto" }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_required_tool_choice() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.5"),
        tool_context(),
        Some(codex_options(&token, fetch.clone(), |options| {
            options.tool_choice = Some(serde_json::json!("required"));
        })),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let body = request_json(&requests[0]);
    assert_eq!(
        body.get("tool_choice"),
        Some(&serde_json::json!("required"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sets_codex_strict_mode_explicitly_and_honors_constrained_sampling() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
    let captured_cb = Arc::clone(&captured);

    let strict_context = Context {
        system_prompt: None,
        messages: vec![pillar_ai::types::Message::User {
            content: pillar_ai::types::UserContent::Text("Use a tool".to_string()),
            timestamp: 1,
        }],
        tools: vec![
            pillar_ai::types::Tool {
                name: "optional".to_string(),
                description: "Optional constrained sampling".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                }),
                constrained_sampling: None,
            },
            pillar_ai::types::Tool {
                name: "strict".to_string(),
                description: "Strict constrained sampling".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "additionalProperties": false,
                }),
                constrained_sampling: Some(
                    pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
                        strict: pillar_ai::types::ConstrainedStrictness::Prefer,
                    },
                ),
            },
        ],
    };

    stream(
        model("gpt-5.5"),
        strict_context,
        Some(codex_options(&token, fetch, move |options| {
            let captured = Arc::clone(&captured_cb);
            options.on_payload = Some(Arc::new(move |_model, payload| {
                *captured.lock().unwrap() = Some(payload.clone());
                Box::pin(async move { Some(payload) })
            }));
        })),
    )
    .result()
    .await;

    let captured = captured.lock().unwrap();
    let tools = captured
        .as_ref()
        .unwrap()
        .get("tools")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(tools[0].get("name"), Some(&serde_json::json!("optional")));
    assert_eq!(tools[0].get("strict"), Some(&serde_json::Value::Null));
    assert_eq!(tools[1].get("name"), Some(&serde_json::json!("strict")));
    assert_eq!(tools[1].get("strict"), Some(&serde_json::json!(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_set_session_id_headers_when_session_id_is_not_provided() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch.clone(), |_| {})),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert!(header_value(request, "session-id").is_none());
    assert!(header_value(request, "x-client-request-id").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_exponential_backoff_across_repeated_sse_retries_without_retry_headers() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let error_body = serde_json::json!({
        "error": { "code": "rate_limit_exceeded", "message": "rate limited" }
    })
    .to_string();
    let retry_headers = vec![("retry-after-ms".to_string(), "1000".to_string())];
    let fetch = CapturedFetch::new(vec![
        json_response(429, &error_body, retry_headers.clone()),
        json_response(429, &error_body, retry_headers.clone()),
        json_response(429, &error_body, retry_headers),
        sse_response(&sse),
    ]);

    let result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch.clone(), |options| {
            options.max_retries = Some(3);
        })),
    )
    .result()
    .await;

    assert_eq!(fetch.requests.lock().unwrap().len(), 4);
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            pillar_ai::types::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("text block");
    assert_eq!(text, "Hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_error_friendly_message_for_usage_limit() {
    let token = mock_token("acc_test");
    let error_body = serde_json::json!({
        "error": {
            "code": "usage_limit_reached",
            "message": "You've hit your usage limit",
            "plan_type": "PLUS",
            "resets_at": 1_800_000_000,
        }
    })
    .to_string();
    let fetch = CapturedFetch::new(vec![json_response(429, &error_body, vec![])]);

    let result = stream(
        model("gpt-5.1-codex"),
        context(),
        Some(codex_options(&token, fetch, |_| {})),
    )
    .result()
    .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    let message = result.error_message.unwrap();
    assert!(
        message.starts_with("You have hit your ChatGPT usage limit (plus plan)."),
        "unexpected message: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn zstd_compresses_sse_request_bodies() {
    let token = mock_token("acc_test");
    let sse = build_sse_payload("completed", false, None);
    let large_text = "compress me ".repeat(400);
    let large_context = Context {
        system_prompt: Some("You are a helpful assistant.".to_string()),
        messages: vec![pillar_ai::types::Message::User {
            content: pillar_ai::types::UserContent::Text(large_text.clone()),
            timestamp: 1,
        }],
        tools: Vec::new(),
    };
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);

    stream(
        model("gpt-5.1-codex"),
        large_context,
        Some(codex_options(&token, fetch.clone(), |_| {})),
    )
    .result()
    .await;

    let requests = fetch.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(header_value(request, "content-encoding"), Some("zstd"));
    let body = request.body.as_deref().unwrap();
    let decoded = zstd::decode_all(body).expect("zstd body");
    let parsed: serde_json::Value = serde_json::from_slice(&decoded).expect("JSON");
    assert_eq!(
        parsed
            .pointer("/input/0/content/0/text")
            .and_then(serde_json::Value::as_str),
        Some(large_text.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_the_client_sent_service_tier_for_gpt_5_1_codex_priority_when_codex_echoes_default() {
    let token = mock_token("acc_test");
    let mut events = vec![
        format!(
            "data: {}",
            serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } })
        ),
        format!(
            "data: {}",
            serde_json::json!({ "type": "response.output_text.delta", "delta": "Hello" })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": "Hello" }],
                },
            })
        ),
        format!(
            "data: {}",
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "service_tier": "default",
                    "usage": {
                        "input_tokens": 1_000_000,
                        "output_tokens": 1_000_000,
                        "total_tokens": 2_000_000,
                        "input_tokens_details": { "cached_tokens": 0 },
                    },
                },
            })
        ),
    ];
    let sse = format!("{}\n\n", events.join("\n\n"));
    events.clear();
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);
    let m = model_with_cost("gpt-5.1-codex", 1.0, 2.0);

    let result = stream(
        m,
        context(),
        Some(codex_options(&token, fetch, |options| {
            options.service_tier = Some("priority".to_string());
        })),
    )
    .result()
    .await;

    // priority on non-gpt-5.5 = 2x.
    assert!((result.usage.cost.input - 2.0).abs() < 1e-9);
    assert!((result.usage.cost.output - 4.0).abs() < 1e-9);
    assert!((result.usage.cost.total - 6.0).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_the_client_sent_service_tier_for_gpt_5_5_priority_when_codex_echoes_default() {
    let token = mock_token("acc_test");
    let sse = build_service_tier_sse();
    let fetch = CapturedFetch::new(vec![sse_response(&sse)]);
    let m = model_with_cost("gpt-5.5", 1.0, 2.0);

    let result = stream(
        m,
        context(),
        Some(codex_options(&token, fetch, |options| {
            options.service_tier = Some("priority".to_string());
        })),
    )
    .result()
    .await;

    // priority on gpt-5.5 = 2.5x.
    assert!((result.usage.cost.input - 2.5).abs() < 1e-9);
    assert!((result.usage.cost.output - 5.0).abs() < 1e-9);
    assert!((result.usage.cost.total - 7.5).abs() < 1e-9);
}

fn build_service_tier_sse() -> String {
    format!(
        "{}\n\n",
        [
            format!(
                "data: {}",
                serde_json::json!({
                    "type": "response.output_item.added",
                    "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
                })
            ),
            format!(
                "data: {}",
                serde_json::json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } })
            ),
            format!("data: {}", serde_json::json!({ "type": "response.output_text.delta", "delta": "Hello" })),
            format!(
                "data: {}",
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "id": "msg_1",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{ "type": "output_text", "text": "Hello" }],
                    },
                })
            ),
            format!(
                "data: {}",
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "status": "completed",
                        "service_tier": "default",
                        "usage": {
                            "input_tokens": 1_000_000,
                            "output_tokens": 1_000_000,
                            "total_tokens": 2_000_000,
                            "input_tokens_details": { "cached_tokens": 0 },
                        },
                    },
                })
            ),
        ]
        .join("\n\n")
    )
}

// --- WebSocket tests ---------------------------------------------------------

fn completed_frame(response_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "status": "completed",
            "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8 },
        },
    })
}

fn hello_stream_frames(response_id: &str) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "response.output_item.added",
            "item": { "type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": [] },
        }),
        serde_json::json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } }),
        serde_json::json!({ "type": "response.output_text.delta", "delta": "Hello" }),
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "Hello" }],
            },
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "status": "completed",
                "end_turn": false,
                "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "input_tokens_details": { "cached_tokens": 0 } },
            },
        }),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_auto_transport_from_stream_simple_options_and_uses_cached_websocket_context() {
    let token = mock_token("acc_test");
    let state = Arc::new(MockWsState::default());
    let factory = Arc::new(MockWsFactory {
        state: Arc::clone(&state),
        script: Mutex::new(vec![hello_stream_frames("resp_1")].into_iter().collect()),
    });
    // Any HTTP fetch is a failure (websocket should handle everything).
    let fetch = CapturedFetch::new(vec![json_response(500, "unexpected fetch", vec![])]);

    let result = stream_simple(
        model("gpt-5.1-codex"),
        context(),
        Some(CodexSimpleStreamOptions {
            api_key: Some(token),
            fetch: Some(fetch.clone()),
            websocket: Some(factory),
            ..Default::default()
        }),
    )
    .result()
    .await;

    // auto transport defaults to websocket; the fetch should not have run.
    assert!(fetch.requests.lock().unwrap().is_empty());
    let sent = state.sent_frames.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].get("type"),
        Some(&serde_json::json!("response.create"))
    );
    assert_eq!(sent[0].get("store"), Some(&serde_json::Value::Bool(false)));
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            pillar_ai::types::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("text block");
    assert_eq!(text, "Hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn scopes_cached_websockets_to_the_authenticated_account() {
    let token_a = mock_token("account-a");
    let token_b = mock_token("account-b");
    reset_openai_codex_websocket_debug_stats(None);

    let make_factory = || {
        Arc::new(MockWsFactory {
            state: Arc::new(MockWsState::default()),
            script: Mutex::new(vec![vec![completed_frame("resp_x")]].into_iter().collect()),
        })
    };

    let context_empty = Context {
        system_prompt: Some(String::new()),
        messages: vec![],
        tools: vec![],
    };

    // account-a -> new connection
    let factory_a = make_factory();
    stream(
        model("gpt-5.1-codex"),
        context_empty.clone(),
        Some(OpenaiCodexResponsesOptions {
            api_key: Some(token_a.clone()),
            session_id: Some("shared-session".to_string()),
            transport: Some(Transport::WebsocketCached),
            websocket: Some(factory_a),
            ..Default::default()
        }),
    )
    .result()
    .await;

    // account-b -> separate connection (never reuses account-a's).
    let factory_b = make_factory();
    stream(
        model("gpt-5.1-codex"),
        context_empty.clone(),
        Some(OpenaiCodexResponsesOptions {
            api_key: Some(token_b.clone()),
            session_id: Some("shared-session".to_string()),
            transport: Some(Transport::WebsocketCached),
            websocket: Some(factory_b),
            ..Default::default()
        }),
    )
    .result()
    .await;

    let stats = get_openai_codex_websocket_debug_stats("shared-session").expect("debug stats");
    // Two separate sessions each created one connection.
    assert!(stats.connections_created >= 2);
}
