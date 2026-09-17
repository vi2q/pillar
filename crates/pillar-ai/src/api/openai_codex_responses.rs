//! Port of packages/ai/src/api/openai-codex-responses.ts (pi v0.84.3).
//!
//! OpenAI Codex Responses adapter (ChatGPT backend): SSE and WebSocket
//! transports over the Responses event protocol.
//!
//! divergence: upstream drives the WHATWG fetch API with a zstd-compressed
//! body and the runtime's global WebSocket constructor. The Rust port sends
//! the request through [`crate::transport::FetchFn`] (zstd compression via
//! the `zstd` crate, `Content-Encoding: zstd` header set as upstream) and
//! abstracts WebSocket connections behind [`WsConnFactory`] / [`WsConn`] so
//! tests can inject a fake transport (upstream tests stub
//! `globalThis.WebSocket`).
//!
//! Other divergences:
//! - WebSocket pooling marks connections idle via a TTL checked on acquire
//!   instead of upstream's `setTimeout` expiry callbacks.
//! - Bun's HTTP-proxy WebSocket subclass is not applicable.
//! - Upstream's `Date.parse(retry-after)` fallback is skipped (numeric
//!   headers only), matching `provider_retry.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::api::impl_from_request_options;
use crate::api::openai_completions::{SseDataEvents, SseJsonEvents};
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, ProcessResponsesStreamOptions,
    convert_responses_messages, convert_responses_tools, process_responses_stream,
};
use crate::api::{OnPayloadFn, OnResponseFn, ProviderResponseInfo, get_user_agent};
use crate::constrained_sampling::create_grammar_tool_input_properties;
use crate::deferred_tools::split_deferred_tools;
use crate::diagnostics::append_assistant_message_diagnostic;
use crate::error::AiError;
use crate::error_body::{format_provider_error, normalize_provider_error};
use crate::event_stream::AssistantMessageEventStream;
use crate::models::clamp_thinking_level;
use crate::provider_retry::ProviderRequestError;
use crate::simple_options::clamp_max_tokens_to_context;
use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, Context,
    Model, ModelThinkingLevel, ProviderHeaders, StopReason, ThinkingLevel, Transport, Usage,
};
use crate::uuid::uuidv7;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;

#[cfg(not(target_arch = "wasm32"))]
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
#[cfg(not(target_arch = "wasm32"))]
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";

const CODEX_RESPONSE_STATUSES: [&str; 6] = [
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];
const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const SESSION_WEBSOCKET_MAX_AGE_MS: u64 = 55 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ============================================================================
// Options
// ============================================================================

/// Upstream `OpenAICodexResponsesOptions` + base stream options.
#[derive(Default)]
pub struct OpenaiCodexResponsesOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub transport: Option<Transport>,
    pub websocket_connect_timeout_ms: Option<u64>,
    /// `None` | Minimal | Low | Medium | High | Xhigh | Max (upstream also
    /// accepts "none", represented here as `Minimal` mapped through the
    /// model's thinking level map — see `build_request_body`).
    pub reasoning_effort: Option<ThinkingLevel>,
    /// "auto" | "detailed" | "concise"; `Some(None)` = "auto".
    pub reasoning_summary: Option<Option<String>>,
    /// "flex" | "priority" | "default" (passed through).
    pub service_tier: Option<String>,
    /// "low" | "medium" | "high".
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<Value>,
    /// Injected WebSocket transport (upstream: the global WebSocket
    /// constructor). `None` uses the native tokio-tungstenite transport.
    pub websocket: Option<Arc<dyn WsConnFactory>>,
}

impl_from_request_options!(OpenaiCodexResponsesOptions);
impl_from_request_options!(CodexSimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for this API.
#[derive(Default)]
pub struct CodexSimpleStreamOptions {
    pub signal: Option<crate::AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub transport: Option<Transport>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub reasoning: Option<ThinkingLevel>,
    pub tool_choice: Option<Value>,
    pub websocket: Option<Arc<dyn WsConnFactory>>,
}

// ============================================================================
// WebSocket transport abstraction (upstream: global WebSocket constructor)
// ============================================================================

/// A live WebSocket connection. Upstream models this with the DOM
/// `WebSocketLike` interface (send/close + event listeners).
#[async_trait::async_trait]
pub trait WsConn: Send + Sync {
    /// Send a text frame.
    fn send(&self, data: String) -> Result<(), String>;
    /// Close the connection (code/reason informational).
    fn close(&self, code: u16, reason: &str);
    /// Receive the next text message. `None` = clean close; `Some(Err)` =
    /// transport/protocol failure. Upstream surfaces close/error events as
    /// errors unless completion was already seen.
    async fn recv(&self) -> Option<Result<String, String>>;
    /// Whether the connection is still open. Upstream checks
    /// `readyState === 1` and treats an unavailable ready state as reusable.
    fn is_reusable(&self) -> bool {
        true
    }
}

/// Connects a WebSocket with the given headers. Upstream: `new WebSocket(url,
/// { headers })` plus open/error/close event wiring with a connect timeout;
/// `OpenAI-Beta` is deleted from the header map before connect.
#[async_trait::async_trait]
pub trait WsConnFactory: Send + Sync {
    async fn connect(
        &self,
        url: &str,
        headers: &[(String, String)],
        signal: Option<&crate::AbortSignal>,
        connect_timeout_ms: u64,
    ) -> Result<Arc<dyn WsConn>, String>;
}

// ============================================================================
// WebSocket session cache & debug stats (upstream module-level maps)
// ============================================================================

/// Upstream `CachedWebSocketContinuationState`.
#[derive(Clone)]
struct CachedWebSocketContinuationState {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct CachedWebSocketConnection {
    socket: Arc<dyn WsConn>,
    busy: bool,
    created_at: u64,
    continuation: Option<CachedWebSocketContinuationState>,
}

/// Upstream `OpenAICodexWebSocketDebugStats`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenaiCodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: u64,
    pub last_delta_input_items: Option<u64>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: bool,
    pub last_websocket_error: Option<String>,
}

#[derive(Default)]
struct CodexWsState {
    /// sessionId -> accountId -> cached connection.
    session_cache: HashMap<String, HashMap<String, CachedWebSocketConnection>>,
    debug_stats: HashMap<String, OpenaiCodexWebSocketDebugStats>,
    sse_fallback_sessions: HashSet<String>,
}

fn ws_state() -> std::sync::MutexGuard<'static, CodexWsState> {
    static STATE: std::sync::LazyLock<Mutex<CodexWsState>> =
        std::sync::LazyLock::new(|| Mutex::new(CodexWsState::default()));
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Upstream `getOpenAICodexWebSocketDebugStats`.
pub fn get_openai_codex_websocket_debug_stats(
    session_id: &str,
) -> Option<OpenaiCodexWebSocketDebugStats> {
    ws_state().debug_stats.get(session_id).cloned()
}

/// Upstream `resetOpenAICodexWebSocketDebugStats`.
pub fn reset_openai_codex_websocket_debug_stats(session_id: Option<&str>) {
    let mut state = ws_state();
    match session_id {
        Some(session_id) => {
            state.debug_stats.remove(session_id);
            state.sse_fallback_sessions.remove(session_id);
        }
        None => {
            state.debug_stats.clear();
            state.sse_fallback_sessions.clear();
        }
    }
}

/// Upstream `closeOpenAICodexWebSocketSessions`.
pub fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let mut state = ws_state();
    let take = |state: &mut CodexWsState, key: &str| state.session_cache.remove(key);
    match session_id {
        Some(session_id) => {
            if let Some(entries) = take(&mut state, session_id) {
                for entry in entries.into_values() {
                    entry.socket.close(1000, "debug_close");
                }
            }
        }
        None => {
            for (_, entries) in state.session_cache.drain() {
                for entry in entries.into_values() {
                    entry.socket.close(1000, "debug_close");
                }
            }
        }
    }
}

fn record_websocket_sse_fallback(state: &mut CodexWsState, session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    state.sse_fallback_sessions.insert(session_id.to_string());
    let stats = state.debug_stats.entry(session_id.to_string()).or_default();
    stats.sse_fallbacks += 1;
    stats.websocket_fallback_active = true;
}

fn record_websocket_failure(state: &mut CodexWsState, session_id: Option<&str>, error: &str) {
    let Some(session_id) = session_id else {
        return;
    };
    state.sse_fallback_sessions.insert(session_id.to_string());
    let stats = state.debug_stats.entry(session_id.to_string()).or_default();
    stats.websocket_failures += 1;
    stats.last_websocket_error = Some(error.to_string());
    stats.websocket_fallback_active = true;
}

// ============================================================================
// Errors
// ============================================================================

/// Upstream `CodexApiError`: a modeled API error carrying an optional code.
#[derive(Debug, Clone)]
pub struct CodexApiError {
    pub message: String,
    pub code: Option<String>,
    pub payload: Option<Value>,
}

/// Upstream `CodexProtocolError`: a malformed wire payload.
#[derive(Debug, Clone)]
pub struct CodexProtocolError {
    pub message: String,
    pub payload: Option<Value>,
}

/// Terminal error categories mirroring upstream's error-class checks.
/// divergence: SSE JSON parse failures surface as `Plain` (the shared
/// `SseJsonEvents` source reports `AiError::Other`); the `Protocol` variant
/// remains for WebSocket JSON errors.
#[derive(Debug)]
#[allow(dead_code)]
enum CodexStreamError {
    Api(CodexApiError),
    Protocol(CodexProtocolError),
    /// Upstream `WebSocketCloseError` / plain errors.
    Plain(String),
    /// Upstream `RetryDelayExceededError`.
    RetryDelayExceeded(String),
    Aborted,
}

impl CodexStreamError {
    fn message(&self) -> String {
        match self {
            Self::Api(error) => error.message.clone(),
            Self::Protocol(error) => error.message.clone(),
            Self::Plain(message) | Self::RetryDelayExceeded(message) => message.clone(),
            Self::Aborted => "Request was aborted".to_string(),
        }
    }

    fn is_websocket_connection_limit_reached(&self) -> bool {
        matches!(self, Self::Api(error) if error.code.as_deref() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE))
    }

    fn is_previous_response_not_found(&self) -> bool {
        matches!(self, Self::Api(error) if error.code.as_deref() == Some(PREVIOUS_RESPONSE_NOT_FOUND_CODE))
    }

    fn is_non_transport(&self) -> bool {
        matches!(self, Self::Api(_) | Self::Protocol(_))
    }
}

impl From<ProviderRequestError> for CodexStreamError {
    fn from(error: ProviderRequestError) -> Self {
        if error.aborted {
            Self::Aborted
        } else {
            Self::Plain(error.message)
        }
    }
}

impl From<AiError> for CodexStreamError {
    fn from(error: AiError) -> Self {
        match error {
            AiError::Aborted(message) => Self::Plain(message),
            AiError::Other(message) => Self::Plain(message),
        }
    }
}

// ============================================================================
// Entry points
// ============================================================================

/// Upstream `stream` for `openai-codex-responses`.
pub fn stream(
    model: Model,
    context: Context,
    options: Option<OpenaiCodexResponsesOptions>,
) -> AssistantMessageEventStream {
    let stream = crate::event_stream::assistant_message_event_stream();
    let task_stream = stream.clone_stream();
    tokio::spawn(run_stream(
        model,
        context,
        options.unwrap_or_default(),
        task_stream,
    ));
    stream
}

async fn run_stream(
    model: Model,
    context: Context,
    options: OpenaiCodexResponsesOptions,
    stream: AssistantMessageEventStream,
) {
    let mut output = fresh_output(&model);
    let result = run_stream_inner(&model, &context, &options, &mut output, &stream).await;
    if let Err(error) = result {
        // Streaming scratch buffers are only used during parsing; never
        // persist them.
        output.stop_reason = if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
            || matches!(error, CodexStreamError::Aborted)
        {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        let norm = normalize_provider_error(error.message(), None, None);
        output.error_message = Some(format_provider_error(&norm, None));
        stream.push(AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        });
        stream.end(Some(output));
    }
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
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

fn assert_successful_output(output: &AssistantMessage) -> Result<(), CodexStreamError> {
    match output.stop_reason {
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => Ok(()),
        _ => Err(CodexStreamError::Plain(format!(
            "Unexpected stop reason: {:?}",
            output.stop_reason
        ))),
    }
}

// ============================================================================
// Retry helpers
// ============================================================================

fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    const PATTERNS: [&str; 8] = [
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ];
    PATTERNS.iter().any(|pattern| error_text.contains(pattern))
}

fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    // /rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused/i
    let lower = error_text.to_lowercase();
    lower.contains("rate") && lower.contains("limit")
        || lower.contains("overloaded")
        || lower.contains("service") && lower.contains("unavailable")
        || lower.contains("upstream") && lower.contains("connect")
        || lower.contains("connection") && lower.contains("refused")
}

/// Upstream `getRetryAfterDelayMs` on a raw header list.
fn get_retry_after_delay_ms(headers: &[(String, String)]) -> Option<u64> {
    let header = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    if let Some(retry_after_ms) = header("retry-after-ms") {
        if let Ok(millis) = retry_after_ms.trim().parse::<f64>() {
            if millis.is_finite() {
                return Some(millis.max(0.0) as u64);
            }
        }
    }
    let retry_after = header("retry-after")?;
    let trimmed = retry_after.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<f64>() {
        if seconds.is_finite() {
            return Some((seconds * 1000.0).max(0.0) as u64);
        }
    }
    // divergence: upstream falls back to Date.parse(retry-after) HTTP dates.
    None
}

fn validate_retry_delay_ms(
    delay_ms: u64,
    max_retry_delay_ms: Option<u64>,
) -> Result<u64, CodexStreamError> {
    let max_delay_ms = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_delay_ms > 0 && delay_ms > max_delay_ms {
        return Err(CodexStreamError::RetryDelayExceeded(format!(
            "Server requested {}s retry delay (max: {}s)",
            delay_ms.div_ceil(1000),
            max_delay_ms.div_ceil(1000)
        )));
    }
    Ok(delay_ms)
}

async fn sleep(ms: u64, signal: Option<&crate::AbortSignal>) -> Result<(), CodexStreamError> {
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return Err(CodexStreamError::Aborted);
    }
    crate::clock::sleep(Duration::from_millis(ms)).await;
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return Err(CodexStreamError::Aborted);
    }
    Ok(())
}

/// Upstream `compressRequestBodyZstd`.
#[cfg(not(target_arch = "wasm32"))]
fn compress_request_body_zstd(body_json: &str) -> Option<Vec<u8>> {
    // divergence: upstream relies on node:zlib availability and compresses
    // everything; the port skips bodies under 1 KiB where frame overhead
    // outweighs compression and falls back to plain JSON on failure.
    if body_json.len() < 1024 {
        return None;
    }
    zstd::encode_all(body_json.as_bytes(), REQUEST_COMPRESSION_ZSTD_LEVEL).ok()
}

/// wasm32 has no zstd encoder; requests are sent uncompressed (the injected
/// transport may compress at the host boundary).
#[cfg(target_arch = "wasm32")]
fn compress_request_body_zstd(_body_json: &str) -> Option<Vec<u8>> {
    None
}

// ============================================================================
// Main stream body
// ============================================================================

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &OpenaiCodexResponsesOptions,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
) -> Result<(), CodexStreamError> {
    let api_key = options
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            CodexStreamError::Plain(format!("No API key for provider: {}", model.provider))
        })?;
    let account_id = extract_account_id(&api_key)?;
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        Some(&context.tools),
        codex_compat(model)
            .supports_openai_grammar_tools
            .unwrap_or(false),
    );
    let cache_session_id: Option<String> = match options.cache_retention {
        Some(CacheRetention::None) => None,
        _ => options.session_id.clone(),
    };
    let codex_session_id = clamp_openai_prompt_cache_key(cache_session_id.as_deref());
    let mut body = build_request_body(
        model,
        context,
        options,
        codex_session_id.as_deref(),
        &grammar_tool_input_properties,
    );
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_body) = on_payload(model, body.clone()).await {
            body = next_body;
        }
    }
    let websocket_request_id = codex_session_id.clone().unwrap_or_else(uuidv7);
    let sse_headers = build_sse_headers(
        model.headers.as_ref(),
        options.headers.as_ref(),
        &account_id,
        &api_key,
        codex_session_id.as_deref(),
    );
    let websocket_headers = build_websocket_headers(
        model.headers.as_ref(),
        options.headers.as_ref(),
        &account_id,
        &api_key,
        &websocket_request_id,
    );
    let body_json = serde_json::to_string(&body).map_err(|error| {
        CodexStreamError::Plain(format!("failed to serialize request body: {error}"))
    })?;
    let websocket_connect_timeout_ms = options
        .websocket_connect_timeout_ms
        .unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);
    let transport = options.transport.unwrap_or(Transport::Auto);
    let mut start_emitted = false;
    let websocket_disabled_for_session = !matches!(transport, Transport::Sse) && {
        let state = ws_state();
        cache_session_id
            .as_deref()
            .is_some_and(|id| state.sse_fallback_sessions.contains(id))
    };

    if !matches!(transport, Transport::Sse) && !websocket_disabled_for_session {
        let mut websocket_started;
        let mut retried_websocket_connection_limit = false;
        let mut retried_missing_websocket_continuation = false;
        loop {
            websocket_started = false;
            let result = process_websocket_stream(
                &resolve_codex_websocket_url(&model.base_url),
                &body,
                &websocket_headers,
                output,
                stream,
                model,
                &mut websocket_started,
                &mut start_emitted,
                options.timeout_ms,
                websocket_connect_timeout_ms,
                cache_session_id.as_deref(),
                &account_id,
                &grammar_tool_input_properties,
                options,
            )
            .await;

            let error = match result {
                Ok(()) => {
                    if options
                        .signal
                        .as_ref()
                        .is_some_and(|signal| signal.is_aborted())
                    {
                        return Err(CodexStreamError::Aborted);
                    }
                    assert_successful_output(output)?;
                    stream.push(AssistantMessageEvent::Done {
                        reason: output.stop_reason,
                        message: output.clone(),
                    });
                    stream.end(Some(output.clone()));
                    return Ok(());
                }
                Err(error) => error,
            };

            let aborted = options
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
                || matches!(error, CodexStreamError::Aborted);
            let connection_limit_before_start =
                !websocket_started && error.is_websocket_connection_limit_reached();
            let previous_response_not_found = error.is_previous_response_not_found();
            if !aborted && previous_response_not_found && !retried_missing_websocket_continuation {
                retried_missing_websocket_continuation = true;
                continue;
            }
            if !aborted && connection_limit_before_start && !retried_websocket_connection_limit {
                retried_websocket_connection_limit = true;
                continue;
            }
            if aborted || (error.is_non_transport() && !connection_limit_before_start) {
                return Err(error);
            }
            append_assistant_message_diagnostic(
                output,
                AssistantMessageDiagnostic {
                    kind: "provider_transport_failure".to_string(),
                    timestamp: now_ms(),
                    error: None,
                    details: Some(json!({
                        "configuredTransport": transport_to_str(&transport),
                        "fallbackTransport": if websocket_started { Value::Null } else { json!("sse") },
                        "eventsEmitted": websocket_started,
                        "phase": if websocket_started { "after_message_stream_start" } else { "before_message_stream_start" },
                        "requestBytes": body_json.len(),
                    })),
                },
            );
            {
                let mut state = ws_state();
                record_websocket_failure(&mut state, cache_session_id.as_deref(), &error.message());
            }
            if websocket_started {
                return Err(error);
            }
            {
                let mut state = ws_state();
                record_websocket_sse_fallback(&mut state, cache_session_id.as_deref());
            }
            break;
        }
    }

    // Compress the request body once for the SSE path. The Codex backend
    // decodes Content-Encoding: zstd; the WebSocket transport above sends the
    // uncompressed JSON frame, matching the official Codex client.
    let compressed_body = compress_request_body_zstd(&body_json);
    let mut sse_headers = sse_headers;
    let sse_body: Vec<u8> = match &compressed_body {
        Some(bytes) => {
            set_header(&mut sse_headers, "content-encoding", "zstd");
            bytes.clone()
        }
        None => body_json.clone().into_bytes(),
    };

    // Fetch with retry logic for rate limits and transient errors.
    let max_retries = options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
    let mut response: Option<crate::transport::FetchResponse> = None;
    let mut last_error: Option<CodexStreamError> = None;

    for attempt in 0..=max_retries {
        if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            return Err(CodexStreamError::Aborted);
        }

        match fetch_sse_once(
            model,
            options,
            &sse_headers,
            sse_body.clone(),
            options.timeout_ms,
        )
        .await
        {
            Ok((fetch_response, headers_timed_out)) => {
                if headers_timed_out {
                    return Err(CodexStreamError::Plain(format!(
                        "Codex SSE response headers timed out after {}ms",
                        options.timeout_ms.unwrap_or(0)
                    )));
                }
                let status = fetch_response.status;
                if let Some(on_response) = &options.on_response {
                    on_response(
                        ProviderResponseInfo {
                            status,
                            headers: fetch_response.headers.clone(),
                        },
                        model,
                    )
                    .await;
                }

                if (200..300).contains(&status) {
                    response = Some(fetch_response);
                    break;
                }

                let error_text = read_error_text(fetch_response.body).await;
                if attempt < max_retries && is_retryable_error(status, &error_text) {
                    let retry_after_delay_ms = get_retry_after_delay_ms(&fetch_response.headers);
                    let delay_ms = match retry_after_delay_ms {
                        None => BASE_DELAY_MS * 2u64.pow(attempt),
                        Some(delay) => validate_retry_delay_ms(delay, options.max_retry_delay_ms)?,
                    };
                    sleep(delay_ms, options.signal.as_ref()).await?;
                    continue;
                }

                let info = parse_error_response(status, &error_text);
                return Err(CodexStreamError::Plain(
                    info.friendly_message.unwrap_or(info.message),
                ));
            }
            Err(error) => {
                if matches!(error, CodexStreamError::RetryDelayExceeded(_))
                    || error.message().contains("usage limit")
                {
                    return Err(error);
                }
                last_error = Some(error);
                if attempt < max_retries {
                    let delay_ms = BASE_DELAY_MS * 2u64.pow(attempt);
                    sleep(delay_ms, options.signal.as_ref()).await?;
                    continue;
                }
                return Err(last_error.unwrap_or_else(|| {
                    CodexStreamError::Plain("Failed after retries".to_string())
                }));
            }
        }
    }

    let response = response.ok_or_else(|| {
        last_error.unwrap_or_else(|| CodexStreamError::Plain("Failed after retries".to_string()))
    })?;

    if !start_emitted {
        stream.push(AssistantMessageEvent::Start {
            partial: output.clone(),
        });
    }
    process_sse_stream(
        response,
        output,
        stream,
        model,
        &grammar_tool_input_properties,
        options,
    )
    .await?;

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(CodexStreamError::Aborted);
    }

    assert_successful_output(output)?;
    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

fn transport_to_str(transport: &Transport) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(entry) = headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        entry.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

fn delete_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
}

async fn read_error_text(body: crate::transport::ByteStream) -> String {
    let mut body = body;
    let mut bytes = Vec::new();
    while let Some(chunk) = futures::StreamExt::next(&mut body).await {
        match chunk {
            Ok(mut chunk) => bytes.append(&mut chunk),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// One SSE fetch attempt. Returns `(response, headers_timed_out)`. Upstream
/// combines the caller signal with `AbortSignal.timeout(httpTimeoutMs)` and
/// reports "headers timed out" when only the timeout fired.
async fn fetch_sse_once(
    model: &Model,
    options: &OpenaiCodexResponsesOptions,
    headers: &[(String, String)],
    body: Vec<u8>,
    timeout_ms: Option<u64>,
) -> Result<(crate::transport::FetchResponse, bool), CodexStreamError> {
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: resolve_codex_url(&model.base_url),
        headers: headers.to_vec(),
        body: Some(body),
    };

    let fetch_future = fetch.fetch(request);
    tokio::pin!(fetch_future);
    let caller_aborted = || {
        options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
    };

    if let Some(timeout_ms) = timeout_ms.filter(|timeout| *timeout > 0) {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        tokio::select! {
            result = &mut fetch_future => {
                match result {
                    Ok(response) => Ok((response, false)),
                    Err(error) => Err(match error {
                        AiError::Aborted(_message) if caller_aborted() => CodexStreamError::Aborted,
                        AiError::Aborted(message) => CodexStreamError::Plain(message),
                        AiError::Other(message) => CodexStreamError::Plain(message),
                    }),
                }
            }
            _ = crate::clock::sleep(deadline.saturating_duration_since(std::time::Instant::now())) => {
                if caller_aborted() {
                    Err(CodexStreamError::Aborted)
                } else {
                    Ok((empty_response(), true))
                }
            }
        }
    } else {
        match fetch_future.await {
            Ok(response) => Ok((response, false)),
            Err(error) => Err(match error {
                AiError::Aborted(_message) if caller_aborted() => CodexStreamError::Aborted,
                AiError::Aborted(message) => CodexStreamError::Plain(message),
                AiError::Other(message) => CodexStreamError::Plain(message),
            }),
        }
    }
}

fn empty_response() -> crate::transport::FetchResponse {
    crate::transport::FetchResponse {
        status: 0,
        headers: Vec::new(),
        body: Box::pin(futures::stream::empty()),
    }
}

fn default_fetch() -> crate::transport::SharedFetchFn {
    Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

/// Upstream `parseErrorResponse`.
struct ParsedErrorResponse {
    message: String,
    friendly_message: Option<String>,
}

fn parse_error_response(status: u16, raw: &str) -> ParsedErrorResponse {
    let mut message = if raw.is_empty() {
        "Request failed".to_string()
    } else {
        raw.to_string()
    };
    let mut friendly_message: Option<String> = None;

    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(err) = parsed.get("error") {
            let code = err
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| err.get("type").and_then(Value::as_str))
                .unwrap_or("");
            let lower_code = code.to_lowercase();
            if lower_code.contains("usage_limit_reached")
                || lower_code.contains("usage_not_included")
                || lower_code.contains("rate_limit_exceeded")
                || status == 429
            {
                let plan = err
                    .get("plan_type")
                    .and_then(Value::as_str)
                    .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
                    .unwrap_or_default();
                let mins = err
                    .get("resets_at")
                    .and_then(Value::as_f64)
                    .map(|resets_at| {
                        ((resets_at * 1000.0 - now_ms() as f64) / 60_000.0)
                            .round()
                            .max(0.0) as u64
                    });
                let when = mins
                    .map(|mins| format!(" Try again in ~{mins} min."))
                    .unwrap_or_default();
                friendly_message = Some(
                    format!("You have hit your ChatGPT usage limit{plan}.{when}")
                        .trim_end()
                        .to_string(),
                );
            }
            if let Some(err_message) = err.get("message").and_then(Value::as_str) {
                message = err_message.to_string();
            } else if let Some(friendly) = &friendly_message {
                message = friendly.clone();
            }
        }
    }

    ParsedErrorResponse {
        message,
        friendly_message,
    }
}

/// Upstream `extractAccountId`: JWT payload → `chatgpt_account_id`.
fn extract_account_id(token: &str) -> Result<String, CodexStreamError> {
    let parts: Vec<&str> = token.split('.').collect();
    let decoded = (|| -> Option<String> {
        if parts.len() != 3 {
            return None;
        }
        let bytes = base64_decode_url_nopad(parts[1])?;
        let parsed: Value = serde_json::from_slice(&bytes).ok()?;
        parsed
            .get(JWT_CLAIM_PATH)
            .and_then(|claim| claim.get("chatgpt_account_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })();
    decoded.ok_or_else(|| {
        CodexStreamError::Plain("Failed to extract accountId from token".to_string())
    })
}

/// Base64url decode without padding (JWT segment).
fn base64_decode_url_nopad(segment: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut normalized = segment.replace(['-', '_'], "+/");
    while normalized.len() % 4 != 0 {
        normalized.push('=');
    }
    let mut output = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in normalized.bytes() {
        if ch == b'=' {
            break;
        }
        let value = TABLE.iter().position(|&c| c == ch)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((acc >> bits) as u8);
        }
    }
    Some(output)
}

// ============================================================================
// Compat
// ============================================================================

fn codex_compat(model: &Model) -> crate::types::OpenaiResponsesCompat {
    match model.compat.as_ref() {
        Some(crate::types::ModelCompat::OpenaiResponses(compat)) => (**compat).clone(),
        _ => crate::types::OpenaiResponsesCompat::default(),
    }
}

// ============================================================================
// Request building
// ============================================================================

/// Upstream `buildRequestBody`.
fn build_request_body(
    model: &Model,
    context: &Context,
    options: &OpenaiCodexResponsesOptions,
    cache_session_id: Option<&str>,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Value {
    let compat = codex_compat(model);
    let supports_strict_mode = compat.supports_strict_mode.unwrap_or(true);
    let supports_openai_grammar_tools = compat.supports_openai_grammar_tools.unwrap_or(false);
    let deferred_tools_mode = if compat.supports_additional_tools.unwrap_or(false) {
        Some("additional-tools")
    } else if compat.supports_tool_search.unwrap_or(false) {
        Some("tool-search")
    } else {
        None
    };
    let tool_placement = split_deferred_tools(context, deferred_tools_mode.is_some());
    let allowed_providers: BTreeSet<String> = CODEX_TOOL_CALL_PROVIDERS
        .iter()
        .map(|provider| provider.to_string())
        .collect();
    let messages = convert_responses_messages(
        model,
        context,
        &allowed_providers,
        Some(ConvertResponsesMessagesOptions {
            include_system_prompt: Some(false),
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&tool_placement.deferred),
            deferred_tools_mode,
            tool_options: Some(ConvertResponsesToolsOptions {
                strict: None,
                supports_strict_mode: Some(supports_strict_mode),
                supports_openai_grammar_tools: Some(supports_openai_grammar_tools),
                defer_loading: None,
            }),
        }),
    );

    let mut body = Map::new();
    body.insert("model".to_string(), json!(model.id));
    body.insert("store".to_string(), Value::Bool(false));
    body.insert("stream".to_string(), Value::Bool(true));
    body.insert(
        "instructions".to_string(),
        json!(
            context
                .system_prompt
                .clone()
                .unwrap_or_else(|| "You are a helpful assistant.".to_string())
        ),
    );
    body.insert("input".to_string(), Value::Array(messages));
    body.insert(
        "text".to_string(),
        json!({ "verbosity": options.text_verbosity.clone().unwrap_or_else(|| "low".to_string()) }),
    );
    body.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );
    if let Some(cache_session_id) = cache_session_id {
        body.insert("prompt_cache_key".to_string(), json!(cache_session_id));
    }
    body.insert(
        "tool_choice".to_string(),
        options.tool_choice.clone().unwrap_or_else(|| json!("auto")),
    );
    body.insert("parallel_tool_calls".to_string(), Value::Bool(true));

    if let Some(temperature) = options.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(service_tier) = &options.service_tier {
        body.insert("service_tier".to_string(), json!(service_tier));
    }
    if !tool_placement.immediate.is_empty() {
        let converted = convert_responses_tools(
            &tool_placement.immediate,
            Some(&ConvertResponsesToolsOptions {
                strict: None,
                supports_strict_mode: Some(supports_strict_mode),
                supports_openai_grammar_tools: Some(supports_openai_grammar_tools),
                defer_loading: None,
            }),
        );
        // Upstream passes `strict: null` in toolOptions (JS null is distinct
        // from undefined); tools then carry `strict: null` unless constrained
        // sampling forces true. The port's Option<bool> helper cannot emit
        // null, so remap the default false emission here.
        let converted: Vec<Value> = converted
            .into_iter()
            .map(|mut tool| {
                if tool.get("strict") == Some(&Value::Bool(false)) {
                    if let Value::Object(object) = &mut tool {
                        object.insert("strict".to_string(), Value::Null);
                    }
                }
                tool
            })
            .collect();
        body.insert("tools".to_string(), Value::Array(converted));
    }
    if let Some(reasoning_effort) = &options.reasoning_effort {
        // Upstream "none" maps through thinkingLevelMap.off ?? "none"; the
        // port's ThinkingLevel has no Off variant, so "none" is expressed by
        // pairing Minimal with a map that resolves to "none"/null. Effort
        // resolution: map lookup first, then the level name.
        let level_key = codex_to_model_level(*reasoning_effort);
        let effort = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&level_key).cloned().flatten())
            .unwrap_or_else(|| level_to_string(*reasoning_effort));
        if effort != "none" {
            body.insert(
                "reasoning".to_string(),
                json!({
                    "effort": effort,
                    "summary": options
                        .reasoning_summary
                        .clone()
                        .unwrap_or(Some("auto".to_string()))
                        .unwrap_or_else(|| "auto".to_string()),
                }),
            );
        }
    }

    Value::Object(body)
}

fn codex_to_model_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn level_to_string(level: ThinkingLevel) -> String {
    match level {
        ThinkingLevel::Minimal => "minimal".to_string(),
        ThinkingLevel::Low => "low".to_string(),
        ThinkingLevel::Medium => "medium".to_string(),
        ThinkingLevel::High => "high".to_string(),
        ThinkingLevel::Xhigh => "xhigh".to_string(),
        ThinkingLevel::Max => "max".to_string(),
    }
}

// ============================================================================
// Headers
// ============================================================================

/// Upstream `buildBaseCodexHeaders`. `init_headers` is upstream's
/// `model.headers` record; `additional_headers` is the per-request override
/// (null values delete).
fn build_base_codex_headers(
    init_headers: Option<&crate::types::ProviderHeaders>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
) -> Vec<(String, String)> {
    // Vec preserves upstream's Headers set/delete/overwrite semantics.
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(init_headers) = init_headers {
        for (key, value) in init_headers {
            if let Some(value) = value {
                set_header(&mut headers, key, value);
            }
        }
    }
    if let Some(additional_headers) = additional_headers {
        for (key, value) in additional_headers {
            match value {
                Some(value) => set_header(&mut headers, key, value),
                None => delete_header(&mut headers, key),
            }
        }
    }
    set_header(&mut headers, "Authorization", &format!("Bearer {token}"));
    set_header(&mut headers, "chatgpt-account-id", account_id);
    set_header(&mut headers, "originator", "pi");
    set_header(&mut headers, "User-Agent", &get_user_agent());
    headers
}

/// Upstream `buildSSEHeaders`.
fn build_sse_headers(
    init_headers: Option<&crate::types::ProviderHeaders>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = build_base_codex_headers(init_headers, additional_headers, account_id, token);
    set_header(&mut headers, "OpenAI-Beta", "responses=experimental");
    set_header(&mut headers, "accept", "text/event-stream");
    set_header(&mut headers, "content-type", "application/json");
    if let Some(session_id) = session_id {
        set_header(&mut headers, "session-id", session_id);
        set_header(&mut headers, "x-client-request-id", session_id);
    }
    headers
}

/// Upstream `buildWebSocketHeaders`.
fn build_websocket_headers(
    init_headers: Option<&crate::types::ProviderHeaders>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut headers = build_base_codex_headers(init_headers, additional_headers, account_id, token);
    delete_header(&mut headers, "accept");
    delete_header(&mut headers, "content-type");
    delete_header(&mut headers, "OpenAI-Beta");
    delete_header(&mut headers, "openai-beta");
    set_header(
        &mut headers,
        "OpenAI-Beta",
        OPENAI_BETA_RESPONSES_WEBSOCKETS,
    );
    set_header(&mut headers, "x-client-request-id", request_id);
    set_header(&mut headers, "session-id", request_id);
    headers
}

// ============================================================================
// URL resolution
// ============================================================================

/// Upstream `resolveCodexUrl`.
fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        return normalized.to_string();
    }
    if normalized.ends_with("/codex") {
        return format!("{normalized}/responses");
    }
    format!("{normalized}/codex/responses")
}

/// Upstream `resolveCodexWebSocketUrl`.
fn resolve_codex_websocket_url(base_url: &str) -> String {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return format!("ws://{rest}");
    }
    url
}

// ============================================================================
// Codex event mapping
// ============================================================================

fn extract_codex_event_error(event: &Value) -> (Option<String>, String) {
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (code, message)
}

// ============================================================================
// SSE stream
// ============================================================================

/// Upstream `processStream` for the SSE transport: map Codex events and
/// delegate to the shared Responses stream processor.
async fn process_sse_stream(
    response: crate::transport::FetchResponse,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
    model: &Model,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    options: &OpenaiCodexResponsesOptions,
) -> Result<(), CodexStreamError> {
    let service_tier = options.service_tier.clone();
    let model_id = model.id.clone();
    let pricing = move |usage: &mut Usage, response_tier: Option<&str>| {
        let resolved = resolve_codex_service_tier(response_tier, service_tier.as_deref());
        apply_service_tier_pricing(usage, resolved.as_deref(), &model_id);
    };
    let end_turn_slot: Arc<Mutex<Option<bool>>> = Arc::default();
    let sse = SseJsonEvents::new(SseDataEvents::new(response.body));
    let mut events = CodexMappedEvents::new(sse, Arc::clone(&end_turn_slot));
    process_responses_stream(
        &mut events,
        output,
        stream,
        model,
        Some(ProcessResponsesStreamOptions {
            grammar_tool_input_properties,
            apply_service_tier_pricing: Some(Box::new(pricing)),
        }),
    )
    .await
    .map_err(|error| {
        // Upstream parseSSE checks the signal around every read and throws
        // "Request was aborted"; an abort mid-body must win over the shared
        // processor's "ended before terminal event" diagnosis.
        if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            CodexStreamError::Aborted
        } else {
            CodexStreamError::Plain(error.to_string())
        }
    })?;
    // Upstream's mapCodexEvents writes output.endTurn synchronously during
    // event mapping; the port's immutable mapper stages it here.
    if let Some(end_turn) = *end_turn_slot.lock().unwrap() {
        output.end_turn = Some(end_turn);
    }
    Ok(())
}

/// Stream adapter that applies `mapCodexEvent` to each SSE JSON value.
/// `output` is reached through the caller's reference by staging end_turn
/// writes in `pending_end_turn` (the shared processor reads it back).
struct CodexMappedEvents {
    inner: SseJsonEvents,
    buffer: VecDeque<Value>,
    error: Option<CodexStreamError>,
    end_turn: Option<Arc<Mutex<Option<bool>>>>,
}

impl CodexMappedEvents {
    fn new(inner: SseJsonEvents, end_turn: Arc<Mutex<Option<bool>>>) -> Self {
        Self {
            inner,
            buffer: VecDeque::new(),
            error: None,
            end_turn: Some(end_turn),
        }
    }
}

impl futures::Stream for CodexMappedEvents {
    type Item = Result<Value, AiError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(event) = self.buffer.pop_front() {
                return std::task::Poll::Ready(Some(Ok(event)));
            }
            if let Some(error) = &self.error {
                let message = error.message();
                return std::task::Poll::Ready(Some(Err(AiError::Other(message))));
            }
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(event))) => {
                    match map_codex_event_immutable(event, self.end_turn.as_ref()) {
                        Ok(Some(mapped)) => self.buffer.push_back(mapped),
                        Ok(None) => continue,
                        Err(error) => {
                            self.error = Some(error);
                        }
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                std::task::Poll::Ready(None) => {
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Output-free variant of `mapCodexEvent`; end_turn is captured into the
/// shared slot the adapter applies to `output.end_turn` after processing.
fn map_codex_event_immutable(
    event: Value,
    end_turn: Option<&Arc<Mutex<Option<bool>>>>,
) -> Result<Option<Value>, CodexStreamError> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if event_type == "error" {
        let (code, message) = extract_codex_event_error(&event);
        return Err(CodexStreamError::Api(CodexApiError {
            message: format!(
                "Codex error: {}",
                if message.is_empty() {
                    code.clone().unwrap_or_else(|| event.to_string())
                } else {
                    message
                }
            ),
            code,
            payload: Some(event),
        }));
    }
    if event_type == "response.failed" {
        let code = event
            .pointer("/response/error/code")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = event
            .pointer("/response/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Codex response failed")
            .to_string();
        return Err(CodexStreamError::Api(CodexApiError {
            message,
            code,
            payload: Some(event),
        }));
    }
    if matches!(
        event_type.as_str(),
        "response.done" | "response.completed" | "response.incomplete"
    ) {
        let mut mapped = event.clone();
        if let Some(response) = mapped.get_mut("response") {
            if let Some(end_turn_value) = response.get("end_turn").and_then(Value::as_bool) {
                if let Some(slot) = end_turn {
                    *slot.lock().unwrap() = Some(end_turn_value);
                }
            }
            let mut normalized = response.clone();
            if let Value::Object(object) = &mut normalized {
                let status = object
                    .get("status")
                    .and_then(Value::as_str)
                    .filter(|status| CODEX_RESPONSE_STATUSES.contains(status))
                    .map(str::to_string);
                match status {
                    Some(status) => object.insert("status".to_string(), json!(status)),
                    None => object.remove("status"),
                };
            }
            if let Value::Object(object) = &mut mapped {
                object.insert("type".to_string(), json!("response.completed"));
                object.insert("response".to_string(), normalized);
            }
        }
        return Ok(Some(mapped));
    }
    Ok(Some(event))
}

fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model_id: &str) {
    let multiplier = get_service_tier_cost_multiplier(model_id, service_tier);
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// Upstream `getServiceTierCostMultiplier` (Codex variant: gpt-5.5 priority
/// is 2.5x).
fn get_service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Upstream `resolveCodexServiceTier`: when the backend echoes "default",
/// keep the client-sent tier for pricing.
fn resolve_codex_service_tier(
    response_service_tier: Option<&str>,
    request_service_tier: Option<&str>,
) -> Option<String> {
    if response_service_tier == Some("default")
        && matches!(request_service_tier, Some("flex") | Some("priority"))
    {
        return request_service_tier.map(str::to_string);
    }
    response_service_tier
        .or(request_service_tier)
        .map(str::to_string)
}

// ============================================================================
// WebSocket transport
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn process_websocket_stream(
    url: &str,
    body: &Value,
    headers: &[(String, String)],
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
    model: &Model,
    websocket_started: &mut bool,
    start_emitted: &mut bool,
    idle_timeout_ms: Option<u64>,
    connect_timeout_ms: u64,
    cache_session_id: Option<&str>,
    account_id: &str,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    options: &OpenaiCodexResponsesOptions,
) -> Result<(), CodexStreamError> {
    let (socket, cache_key, reused) = acquire_websocket(
        url,
        headers,
        cache_session_id,
        account_id,
        options.signal.as_ref(),
        connect_timeout_ms,
        options.websocket.as_ref(),
    )
    .await?;

    let use_cached_context = !matches!(
        options.transport,
        Some(Transport::Websocket) | Some(Transport::Sse)
    );
    // ChatGPT Codex Responses rejects `store: true` ("Store must be set to
    // false"). WebSocket continuation still works via connection-scoped
    // previous_response_id state.
    let full_body = body.clone();
    let request_body = if use_cached_context {
        build_cached_websocket_request_body(cache_session_id, account_id, &full_body)
    } else {
        full_body.clone()
    };

    // Update debug stats (upstream getOrCreateWebSocketDebugStats increments).
    {
        let mut state = ws_state();
        if let Some(session_id) = cache_session_id {
            let stats = state.debug_stats.entry(session_id.to_string()).or_default();
            stats.requests += 1;
            if reused {
                stats.connections_reused += 1;
            } else {
                stats.connections_created += 1;
            }
            if use_cached_context {
                stats.cached_context_requests += 1;
            }
            if request_body.get("store") == Some(&Value::Bool(true)) {
                stats.store_true_requests += 1;
            }
            let input_len = request_body
                .get("input")
                .and_then(Value::as_array)
                .map(|items| items.len() as u64)
                .unwrap_or(0);
            stats.last_input_items = input_len;
            if let Some(previous) = request_body
                .get("previous_response_id")
                .and_then(Value::as_str)
            {
                stats.delta_requests += 1;
                stats.last_delta_input_items = Some(input_len);
                stats.last_previous_response_id = Some(previous.to_string());
            } else {
                stats.full_context_requests += 1;
                stats.last_delta_input_items = None;
                stats.last_previous_response_id = None;
            }
        }
    }

    let send_result: Result<(), CodexStreamError> = async {
        let frame_json = serde_json::to_string(&{
            let mut frame = Map::new();
            frame.insert("type".to_string(), json!("response.create"));
            if let Value::Object(object) = request_body {
                for (key, value) in object {
                    frame.insert(key, value);
                }
            }
            Value::Object(frame)
        })
        .map_err(|error| CodexStreamError::Plain(format!("failed to serialize frame: {error}")));
        let frame_json = frame_json?;
        socket.send(frame_json).map_err(CodexStreamError::Plain)
    }
    .await;
    if let Err(error) = send_result {
        release_websocket(
            cache_session_id,
            account_id,
            cache_key.as_ref(),
            &socket,
            false,
        );
        return Err(error);
    }

    let events = websocket_event_stream(Arc::clone(&socket), idle_timeout_ms);
    tokio::pin!(events);
    let model_id = model.id.clone();
    let service_tier = options.service_tier.clone();
    let make_pricing = || {
        let model_id = model_id.clone();
        let service_tier = service_tier.clone();
        move |usage: &mut Usage, response_tier: Option<&str>| {
            let resolved = resolve_codex_service_tier(response_tier, service_tier.as_deref());
            apply_service_tier_pricing(usage, resolved.as_deref(), &model_id);
        }
    };

    // First event triggers onStart (upstream startWebSocketOutputOnFirstEvent).
    let mut first = true;
    let process_result = loop {
        match futures::StreamExt::next(&mut events).await {
            Some(Ok(event)) => {
                if first {
                    first = false;
                    *websocket_started = true;
                    if !*start_emitted {
                        *start_emitted = true;
                        stream.push(AssistantMessageEvent::Start {
                            partial: output.clone(),
                        });
                    }
                    let _ = start_emitted;
                }
                // Feed each event through the shared processor one at a time
                // by buffering into a single-event stream.
                let mut single = SingleEvent::new(Ok(event), &mut events);
                let result = process_responses_stream(
                    &mut single,
                    output,
                    stream,
                    model,
                    Some(ProcessResponsesStreamOptions {
                        grammar_tool_input_properties,
                        apply_service_tier_pricing: Some(Box::new(make_pricing())),
                    }),
                )
                .await;
                match result {
                    Ok(()) => continue,
                    Err(error) => break Err(CodexStreamError::from(error)),
                }
            }
            Some(Err(error)) => break Err(CodexStreamError::from(error)),
            None => {
                if first {
                    // No events at all: upstream still saw completion tracking
                    // in parseWebSocket and errors on early close there.
                    break Err(CodexStreamError::Plain(
                        "WebSocket stream closed before response.completed".to_string(),
                    ));
                }
                break Ok(());
            }
        }
    };

    let mut keep_connection = true;
    match &process_result {
        Ok(()) => {
            if use_cached_context && output.response_id.is_some() {
                if let Some(cache_key) = &cache_key {
                    let response_items: Vec<Value> = convert_responses_messages(
                        model,
                        &Context {
                            system_prompt: None,
                            messages: vec![crate::types::Message::Assistant(Box::new(
                                output.clone(),
                            ))],
                            tools: Vec::new(),
                        },
                        &CODEX_TOOL_CALL_PROVIDERS
                            .iter()
                            .map(|provider| provider.to_string())
                            .collect(),
                        Some(ConvertResponsesMessagesOptions {
                            include_system_prompt: Some(false),
                            grammar_tool_input_properties: Some(grammar_tool_input_properties),
                            ..Default::default()
                        }),
                    )
                    .into_iter()
                    .filter(|item| {
                        !matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("function_call_output") | Some("custom_tool_call_output")
                        )
                    })
                    .collect();
                    let mut state = ws_state();
                    if let Some(entry) = state
                        .session_cache
                        .get_mut(&cache_key.0)
                        .and_then(|entries| entries.get_mut(&cache_key.1))
                    {
                        entry.continuation = Some(CachedWebSocketContinuationState {
                            last_request_body: full_body,
                            last_response_id: output.response_id.clone().unwrap_or_default(),
                            last_response_items: response_items,
                        });
                    }
                }
            }
        }
        Err(_) => {
            if let Some(cache_key) = &cache_key {
                let mut state = ws_state();
                if let Some(entry) = state
                    .session_cache
                    .get_mut(&cache_key.0)
                    .and_then(|entries| entries.get_mut(&cache_key.1))
                {
                    entry.continuation = None;
                }
            }
            keep_connection = false;
        }
    }

    release_websocket(
        cache_session_id,
        account_id,
        cache_key.as_ref(),
        &socket,
        keep_connection,
    );
    process_result
}

/// Wrapper that yields one buffered event then delegates to the parent
/// stream, so `process_responses_stream` can be invoked per event while
/// retaining `output` ownership semantics.
struct SingleEvent<'a> {
    first: Option<Result<Value, AiError>>,
    rest: &'a mut WebSocketEventStream,
}

impl<'a> SingleEvent<'a> {
    fn new(first: Result<Value, AiError>, rest: &'a mut WebSocketEventStream) -> Self {
        Self {
            first: Some(first),
            rest,
        }
    }
}

impl futures::Stream for SingleEvent<'_> {
    type Item = Result<Value, AiError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(first) = self.first.take() {
            return std::task::Poll::Ready(Some(first));
        }
        std::pin::Pin::new(&mut *self.rest).poll_next(cx)
    }
}

type WsCacheKey = (String, String);

/// Upstream `acquireWebSocket`: one-shot when no session, pooled per
/// (sessionId, accountId) otherwise. Busy cached entries fall through to a
/// fresh unclosed-on-keep one-shot connection.
async fn acquire_websocket(
    url: &str,
    headers: &[(String, String)],
    session_id: Option<&str>,
    account_id: &str,
    signal: Option<&crate::AbortSignal>,
    connect_timeout_ms: u64,
    websocket_factory: Option<&Arc<dyn WsConnFactory>>,
) -> Result<(Arc<dyn WsConn>, Option<WsCacheKey>, bool), CodexStreamError> {
    let Some(session_id) = session_id else {
        let socket =
            connect_websocket(url, headers, signal, connect_timeout_ms, websocket_factory).await?;
        return Ok((socket, None, false));
    };

    // Reuse path. All cache reads complete before any await: the guard is
    // released by ending the scoped block (docs/INSTRUCTIONS.md #1).
    {
        let decision = {
            let state = ws_state();
            state
                .session_cache
                .get(session_id)
                .and_then(|entries| entries.get(account_id))
                .map(|cached| {
                    let expired =
                        now_ms().saturating_sub(cached.created_at) >= SESSION_WEBSOCKET_MAX_AGE_MS;
                    (
                        expired,
                        cached.busy,
                        cached.socket.is_reusable(),
                        Arc::clone(&cached.socket),
                    )
                })
        };
        if let Some((expired, busy, reusable, cached_socket)) = decision {
            if !busy && expired {
                // Age limit: close and replace (upstream connection_age_limit).
                cached_socket.close(1000, "connection_age_limit");
                let mut state = ws_state();
                if let Some(entries) = state.session_cache.get_mut(session_id) {
                    entries.remove(account_id);
                    if entries.is_empty() {
                        state.session_cache.remove(session_id);
                    }
                }
            } else if !busy && reusable {
                let mut state = ws_state();
                if let Some(cached) = state
                    .session_cache
                    .get_mut(session_id)
                    .and_then(|entries| entries.get_mut(account_id))
                {
                    cached.busy = true;
                }
                return Ok((
                    cached_socket,
                    Some((session_id.to_string(), account_id.to_string())),
                    true,
                ));
            } else if busy {
                // Busy: fresh one-shot connection, released without caching.
                let socket =
                    connect_websocket(url, headers, signal, connect_timeout_ms, websocket_factory)
                        .await?;
                return Ok((socket, None, false));
            }
        }
    }

    // Fresh pooled connection.
    let socket =
        connect_websocket(url, headers, signal, connect_timeout_ms, websocket_factory).await?;
    {
        let mut state = ws_state();
        state
            .session_cache
            .entry(session_id.to_string())
            .or_default()
            .insert(
                account_id.to_string(),
                CachedWebSocketConnection {
                    socket: Arc::clone(&socket),
                    busy: true,
                    created_at: now_ms(),
                    continuation: None,
                },
            );
    }
    Ok((
        socket,
        Some((session_id.to_string(), account_id.to_string())),
        false,
    ))
}

fn release_websocket(
    session_id: Option<&str>,
    account_id: &str,
    cache_key: Option<&WsCacheKey>,
    socket: &Arc<dyn WsConn>,
    keep: bool,
) {
    let Some(session_id) = session_id else {
        socket.close(1000, "done");
        return;
    };
    // One-shot (busy fallback) connections are never pooled.
    let Some(cache_key) = cache_key else {
        socket.close(1000, "done");
        return;
    };
    let _ = &cache_key;
    let mut state = ws_state();
    let pooled = state
        .session_cache
        .get(session_id)
        .and_then(|entries| entries.get(account_id))
        .map(|cached| Arc::ptr_eq(&cached.socket, socket))
        .unwrap_or(false);
    if !keep || !pooled || !socket.is_reusable() {
        socket.close(1000, "done");
        if let Some(entries) = state.session_cache.get_mut(session_id) {
            if entries
                .get(account_id)
                .map(|cached| Arc::ptr_eq(&cached.socket, socket))
                .unwrap_or(false)
            {
                entries.remove(account_id);
            }
            if entries.is_empty() {
                state.session_cache.remove(session_id);
            }
        }
        return;
    }
    if let Some(cached) = state
        .session_cache
        .get_mut(session_id)
        .and_then(|entries| entries.get_mut(account_id))
    {
        cached.busy = false;
        // Upstream schedules a TTL expiry timer; the port closes lazily on
        // the next acquire when the max age is exceeded.
    }
}

async fn connect_websocket(
    url: &str,
    headers: &[(String, String)],
    signal: Option<&crate::AbortSignal>,
    connect_timeout_ms: u64,
    websocket_factory: Option<&Arc<dyn WsConnFactory>>,
) -> Result<Arc<dyn WsConn>, CodexStreamError> {
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return Err(CodexStreamError::Aborted);
    }
    let factory: Arc<dyn WsConnFactory> = if let Some(factory) = websocket_factory {
        Arc::clone(factory)
    } else {
        Arc::new(NativeWsConnFactory)
    };
    let connect = factory.connect(url, headers, signal, connect_timeout_ms);
    tokio::pin!(connect);
    let deadline = std::time::Instant::now() + Duration::from_millis(connect_timeout_ms);
    tokio::select! {
        result = &mut connect => result.map_err(CodexStreamError::Plain),
        _ = crate::clock::sleep(deadline.saturating_duration_since(std::time::Instant::now())) => {
            if signal.is_some_and(|signal| signal.is_aborted()) {
                Err(CodexStreamError::Aborted)
            } else {
                Err(CodexStreamError::Plain(format!(
                    "WebSocket connect timeout after {connect_timeout_ms}ms"
                )))
            }
        }
    }
}

/// Default WebSocket transport over tokio-tungstenite.
#[cfg(not(target_arch = "wasm32"))]
struct NativeWsConnFactory;

/// wasm32 fallback: no native socket transport is available, so callers must
/// inject a [`WsConnFactory`] through `StreamOptions.websocket`.
#[cfg(target_arch = "wasm32")]
struct NativeWsConnFactory;

#[cfg(target_arch = "wasm32")]
#[async_trait::async_trait]
impl WsConnFactory for NativeWsConnFactory {
    async fn connect(
        &self,
        _url: &str,
        _headers: &[(String, String)],
        _signal: Option<&crate::AbortSignal>,
        _connect_timeout_ms: u64,
    ) -> Result<Arc<dyn WsConn>, String> {
        Err(
            "native websocket transport is unavailable on wasm32; inject a WsConnFactory"
                .to_string(),
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl WsConnFactory for NativeWsConnFactory {
    async fn connect(
        &self,
        url: &str,
        headers: &[(String, String)],
        signal: Option<&crate::AbortSignal>,
        _connect_timeout_ms: u64,
    ) -> Result<Arc<dyn WsConn>, String> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderName;
        let mut request = url
            .into_client_request()
            .map_err(|error| format!("invalid websocket url: {error}"))?;
        for (key, value) in headers {
            // OpenAI-Beta is deleted before connect (upstream behavior).
            if key.eq_ignore_ascii_case("openai-beta") {
                continue;
            }
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|error| format!("invalid header name: {error}"))?;
            request.headers_mut().insert(
                name,
                value
                    .parse()
                    .map_err(|error| format!("invalid header value: {error}"))?,
            );
        }
        let (stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| format!("websocket connect failed: {error}"))?;
        let conn = Arc::new(NativeWsConn {
            inner: tokio::sync::Mutex::new(Some(stream)),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        // Abort handling: close the socket when the signal fires (upstream
        // wires an abort listener during connect).
        if let Some(signal) = signal {
            let signal = signal.clone();
            let closer = Arc::downgrade(&conn);
            tokio::spawn(async move {
                signal.aborted_or_pending().await;
                if let Some(conn) = closer.upgrade() {
                    conn.close(1000, "aborted");
                }
            });
        }
        Ok(conn)
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct NativeWsConn {
    inner: tokio::sync::Mutex<
        Option<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    >,
    closed: std::sync::atomic::AtomicBool,
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl WsConn for NativeWsConn {
    fn send(&self, data: String) -> Result<(), String> {
        // Called from async context upstream; the port bridges through
        // block_in_place which is valid on the multi-thread runtime.
        let mut guard = self.inner.blocking_lock();
        let stream = guard.as_mut().ok_or("socket closed")?;
        use futures::SinkExt;
        futures::executor::block_on(
            stream.send(tokio_tungstenite::tungstenite::Message::Text(data.into())),
        )
        .map_err(|error| format!("websocket send failed: {error}"))
    }

    fn close(&self, _code: u16, _reason: &str) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.try_lock() {
            if let Some(stream) = guard.as_mut() {
                let _ = futures::executor::block_on(futures::SinkExt::close(stream));
            }
            *guard = None;
        }
    }

    async fn recv(&self) -> Option<Result<String, String>> {
        use futures::StreamExt;
        loop {
            let message = {
                let mut guard = self.inner.lock().await;
                match guard.as_mut() {
                    Some(stream) => stream.next().await,
                    None => return None,
                }
            };
            match message {
                Some(Ok(message)) => match message {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        return Some(Ok(text.to_string()));
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                        return Some(Ok(String::from_utf8_lossy(&bytes).to_string()));
                    }
                    tokio_tungstenite::tungstenite::Message::Close(frame) => {
                        if let Some(frame) = frame {
                            if frame.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE) {
                                return Some(Err("websocket message too big".to_string()));
                            }
                        }
                        return None;
                    }
                    _ => continue,
                },
                Some(Err(error)) => return Some(Err(format!("websocket error: {error}"))),
                None => return None,
            }
        }
    }

    fn is_reusable(&self) -> bool {
        !self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

// ============================================================================
// WebSocket event stream
// ============================================================================

/// Upstream `parseWebSocket` as a `Stream`: yields JSON messages, errors on
/// protocol failures, idle timeouts, or closes before completion.
struct WebSocketEventStream {
    socket: Arc<dyn WsConn>,
    queue: VecDeque<Value>,
    done: bool,
    saw_completion: bool,
    idle_timeout_ms: Option<u64>,
}

impl WebSocketEventStream {
    fn new(socket: Arc<dyn WsConn>, idle_timeout_ms: Option<u64>) -> Self {
        Self {
            socket,
            queue: VecDeque::new(),
            done: false,
            saw_completion: false,
            idle_timeout_ms,
        }
    }
}

impl futures::Stream for WebSocketEventStream {
    type Item = Result<Value, AiError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(event) = this.queue.pop_front() {
                return std::task::Poll::Ready(Some(Ok(event)));
            }
            if this.done {
                return std::task::Poll::Ready(None);
            }
            // Async receive with optional idle timeout, bridged through an
            // inline future. `poll_fn` borrows `this.socket`; the borrow ends
            // when the future resolves.
            let idle_timeout_ms = this.idle_timeout_ms;
            let socket = this.socket.clone();
            let recv_fut = async move {
                if let Some(idle_timeout_ms) = idle_timeout_ms.filter(|timeout| *timeout > 0) {
                    let recv = socket.recv();
                    tokio::pin!(recv);
                    match crate::clock::timeout(Duration::from_millis(idle_timeout_ms), &mut recv)
                        .await
                    {
                        Ok(message) => message,
                        Err(_) => Some(Err(format!(
                            "WebSocket idle timeout after {idle_timeout_ms}ms"
                        ))),
                    }
                } else {
                    socket.recv().await
                }
            };
            tokio::pin!(recv_fut);
            match recv_fut.poll(cx) {
                std::task::Poll::Ready(Some(Ok(text))) => {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(parsed) => {
                            let event_type =
                                parsed.get("type").and_then(Value::as_str).unwrap_or("");
                            if matches!(
                                event_type,
                                "response.completed" | "response.done" | "response.incomplete"
                            ) {
                                this.saw_completion = true;
                                this.done = true;
                            }
                            this.queue.push_back(parsed);
                        }
                        Err(error) => {
                            this.done = true;
                            return std::task::Poll::Ready(Some(Err(AiError::Other(format!(
                                "Invalid Codex WebSocket JSON: {error}"
                            )))));
                        }
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return std::task::Poll::Ready(Some(Err(AiError::Other(error))));
                }
                std::task::Poll::Ready(None) => {
                    this.done = true;
                    if !this.saw_completion {
                        return std::task::Poll::Ready(Some(Err(AiError::Other(
                            "WebSocket stream closed before response.completed".to_string(),
                        ))));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

fn websocket_event_stream(
    socket: Arc<dyn WsConn>,
    idle_timeout_ms: Option<u64>,
) -> WebSocketEventStream {
    WebSocketEventStream::new(socket, idle_timeout_ms)
}

// ============================================================================
// Cached-context request body
// ============================================================================

/// Upstream `requestBodyWithoutInput`.
fn request_body_without_input(body: &Value) -> Value {
    let mut object = match body {
        Value::Object(object) => object.clone(),
        other => return other.clone(),
    };
    object.remove("input");
    object.remove("previous_response_id");
    Value::Object(object)
}

/// Upstream `responseInputsEqual`.
fn response_inputs_equal(a: Option<&Vec<Value>>, b: Option<&Vec<Value>>) -> bool {
    serde_json::to_string(&a.cloned().unwrap_or_default()).unwrap_or_default()
        == serde_json::to_string(&b.cloned().unwrap_or_default()).unwrap_or_default()
}

/// Upstream `requestBodiesMatchExceptInput`.
fn request_bodies_match_except_input(a: &Value, b: &Value) -> bool {
    serde_json::to_string(&request_body_without_input(a)).unwrap_or_default()
        == serde_json::to_string(&request_body_without_input(b)).unwrap_or_default()
}

/// Upstream `getCachedWebSocketInputDelta`.
fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &CachedWebSocketContinuationState,
) -> Option<Vec<Value>> {
    if !request_bodies_match_except_input(body, &continuation.last_request_body) {
        return None;
    }
    let current_input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut baseline = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current_input.len() < baseline.len() {
        return None;
    }
    let prefix = current_input[..baseline.len()].to_vec();
    if !response_inputs_equal(Some(&prefix), Some(&baseline)) {
        return None;
    }
    Some(current_input[baseline.len()..].to_vec())
}

/// Upstream `buildCachedWebSocketRequestBody`.
fn build_cached_websocket_request_body(
    session_id: Option<&str>,
    account_id: &str,
    body: &Value,
) -> Value {
    let Some(session_id) = session_id else {
        return body.clone();
    };
    let continuation = {
        let state = ws_state();
        state
            .session_cache
            .get(session_id)
            .and_then(|entries| entries.get(account_id))
            .and_then(|entry| entry.continuation.clone())
    };
    let Some(continuation) = continuation else {
        return body.clone();
    };

    let delta = get_cached_websocket_input_delta(body, &continuation);
    match delta.filter(|_| !continuation.last_response_id.is_empty()) {
        Some(delta) => {
            let mut next = body.clone();
            if let Value::Object(object) = &mut next {
                object.insert(
                    "previous_response_id".to_string(),
                    json!(continuation.last_response_id),
                );
                object.insert("input".to_string(), Value::Array(delta));
            }
            next
        }
        None => {
            let mut state = ws_state();
            if let Some(entry) = state
                .session_cache
                .get_mut(session_id)
                .and_then(|entries| entries.get_mut(account_id))
            {
                entry.continuation = None;
            }
            body.clone()
        }
    }
}

// ============================================================================
// streamSimple
// ============================================================================

/// Upstream `streamSimple` for `openai-codex-responses`.
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<CodexSimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.unwrap_or_default();
    if options.api_key.as_deref().unwrap_or("").is_empty() {
        let stream = crate::event_stream::assistant_message_event_stream();
        let message = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some(format!("No API key for provider: {}", model.provider)),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        };
        stream.push(AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: message.clone(),
        });
        stream.end(Some(message));
        return stream;
    }

    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = clamp_max_tokens_to_context(model.context_window, &context, base_max_tokens);
    let clamped_reasoning = options
        .reasoning
        .map(|reasoning| clamp_thinking_level(&model, codex_to_model_level(reasoning)));
    let reasoning_effort: Option<ThinkingLevel> = clamped_reasoning.and_then(|level| match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
    });

    stream(
        model,
        context,
        Some(OpenaiCodexResponsesOptions {
            signal: options.signal,
            api_key: options.api_key,
            fetch: options.fetch,
            env: options.env,
            on_payload: options.on_payload,
            on_response: options.on_response,
            headers: options.headers,
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            temperature: options.temperature,
            max_tokens: Some(max_tokens),
            cache_retention: options.cache_retention,
            session_id: options.session_id,
            transport: options.transport,
            websocket_connect_timeout_ms: options.websocket_connect_timeout_ms,
            reasoning_effort,
            reasoning_summary: None,
            service_tier: None,
            text_verbosity: None,
            tool_choice: options.tool_choice,
            websocket: options.websocket,
        }),
    )
}
