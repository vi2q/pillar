//! Port of packages/ai/src/api/pi-messages.ts (pi v0.84.3).
//!
//! Streams pi's own message protocol to a backend: a single POST of
//! `{ model, context, options }` to `<baseUrl>/messages`, the response is an
//! SSE stream of serialized assistant-message events plus a terminal
//! `done`/`error` event. This is the wire protocol spoken by the Radius
//! gateway.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::AbortSignal;
use crate::api::impl_from_request_options;
use crate::api::{ProviderResponseInfo, get_user_agent};
use crate::diagnostics::append_assistant_message_diagnostic;
use crate::event_stream::AssistantMessageEventStream;
use crate::json_parse::parse_streaming_json;
use crate::provider_env::get_provider_env_value;
use crate::provider_retry::{ProviderRequestError, retry_provider_request};
use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, Content,
    Context, Model, StopReason, ThinkingLevel, Usage, UsageCost,
};

// --- Options ---------------------------------------------------------------

/// Upstream `PiMessagesOptions`.
#[derive(Default)]
pub struct PiMessagesOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<crate::api::OnPayloadFn>,
    pub on_response: Option<crate::api::OnResponseFn>,
    pub headers: Option<crate::types::ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    /// "auto" | "none" | "required" | { type: "function", function: { name } }.
    pub tool_choice: Option<Value>,
    /// Ask the backend for debug metadata (e.g. routing response headers).
    pub debug: Option<bool>,
}

impl_from_request_options!(PiMessagesOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for this API.
#[derive(Default)]
pub struct SimpleStreamOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<crate::types::ProviderEnv>,
    pub on_payload: Option<crate::api::OnPayloadFn>,
    pub on_response: Option<crate::api::OnResponseFn>,
    pub headers: Option<crate::types::ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
}

// --- Wire types ------------------------------------------------------------

/// Upstream `PiMessagesRewriteImpact`: impact summary of a server-side
/// message rewrite (e.g. a gateway policy).
#[derive(Debug, Clone, PartialEq)]
pub struct PiMessagesRewriteImpact {
    pub policy_id: String,
    pub policy_version: u64,
    pub changed: bool,
    pub token_count_change: i64,
    pub message_count_change: i64,
    pub system_prompt_changed: bool,
}

/// Terminal event carried in the wire stream.
#[derive(Debug, Clone)]
enum TerminalEvent {
    Done {
        reason: Option<String>,
        usage: Usage,
        response_id: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
    Error {
        reason: Option<String>,
        usage: Usage,
        error_message: Option<String>,
        response_id: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn reason_to_stop_reason(reason: &str) -> Option<StopReason> {
    match reason {
        "stop" => Some(StopReason::Stop),
        "length" => Some(StopReason::Length),
        "toolUse" => Some(StopReason::ToolUse),
        "aborted" => Some(StopReason::Aborted),
        "error" => Some(StopReason::Error),
        _ => None,
    }
}

fn empty_usage() -> Usage {
    Usage::default()
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input: value.get("input").and_then(Value::as_u64).unwrap_or(0),
        output: value.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_read: value.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
        cache_write: value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0),
        total_tokens: value
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost: value
            .get("cost")
            .map(|cost| UsageCost {
                input: cost.get("input").and_then(Value::as_f64).unwrap_or(0.0),
                output: cost.get("output").and_then(Value::as_f64).unwrap_or(0.0),
                cache_read: cost.get("cacheRead").and_then(Value::as_f64).unwrap_or(0.0),
                cache_write: cost
                    .get("cacheWrite")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                total: cost.get("total").and_then(Value::as_f64).unwrap_or(0.0),
            })
            .unwrap_or_default(),
        ..Usage::default()
    }
}

fn parse_rewrite(value: &Value) -> Option<PiMessagesRewriteImpact> {
    let object = value.as_object()?;
    Some(PiMessagesRewriteImpact {
        policy_id: object.get("policyId")?.as_str()?.to_string(),
        policy_version: object.get("policyVersion")?.as_u64()?,
        changed: object.get("changed")?.as_bool()?,
        token_count_change: object.get("tokenCountChange")?.as_i64()?,
        message_count_change: object.get("messageCountChange")?.as_i64()?,
        system_prompt_changed: object.get("systemPromptChanged")?.as_bool()?,
    })
}

fn append_rewrite_diagnostic(
    message: &mut AssistantMessage,
    rewrite: &Option<PiMessagesRewriteImpact>,
) {
    if let Some(rewrite) = rewrite {
        append_assistant_message_diagnostic(
            message,
            AssistantMessageDiagnostic {
                kind: "pi_messages_rewrite".to_string(),
                timestamp: now_ms(),
                error: None,
                details: Some(json!({
                    "policyId": rewrite.policy_id,
                    "policyVersion": rewrite.policy_version,
                    "changed": rewrite.changed,
                    "tokenCountChange": rewrite.token_count_change,
                    "messageCountChange": rewrite.message_count_change,
                    "systemPromptChanged": rewrite.system_prompt_changed,
                })),
            },
        );
    }
}

/// Error body of a non-2xx response: `{ error: { message, code, ... } }`.
fn parse_error_body(body: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let error = parsed.get("error")?;
    error.as_object()?;
    Some(parsed)
}

fn format_response_error(
    status: u16,
    status_text: &str,
    body: &str,
    error_body: Option<&Value>,
) -> String {
    let message = error_body
        .and_then(|parsed| parsed.pointer("/error/message"))
        .and_then(Value::as_str);
    let code = error_body
        .and_then(|parsed| parsed.pointer("/error/code"))
        .and_then(Value::as_str);
    let suffix = message.unwrap_or(body);
    let code_suffix = code.map(|code| format!(" ({code})")).unwrap_or_default();
    format!("{status} {status_text}: {suffix}{code_suffix}")
}

// --- Event conversion ------------------------------------------------------

/// Upstream `createEventConverter`: folds wire events into a partial
/// AssistantMessage and emits stream events.
struct EventConverter {
    partial: AssistantMessage,
    tool_json: std::collections::BTreeMap<usize, String>,
}

impl EventConverter {
    fn new(model: &Model) -> Self {
        Self {
            partial: AssistantMessage {
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: empty_usage(),
                stop_reason: StopReason::Pending,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: now_ms(),
            },
            tool_json: std::collections::BTreeMap::new(),
        }
    }

    fn block_mut(&mut self, content_index: usize) -> Option<&mut Content> {
        self.partial.content.get_mut(content_index)
    }

    /// Ensure the content vector has a slot at `index` (upstream sparse
    /// array assignment).
    fn slot(&mut self, content_index: usize, make: impl FnOnce() -> Content) -> &mut Content {
        while self.partial.content.len() <= content_index {
            self.partial.content.push(Content::Text {
                text: String::new(),
                text_signature: None,
            });
        }
        if matches!(self.partial.content[content_index], Content::Text { ref text, text_signature: None } if text.is_empty())
        {
            self.partial.content[content_index] = make();
        }
        &mut self.partial.content[content_index]
    }

    /// Convert one wire event. Returns `None` for `start` (no emission).
    fn convert(&mut self, event: &Value) -> Result<Option<AssistantMessageEvent>, String> {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let content_index = event
            .get("contentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        match event_type {
            "start" => Ok(None),
            "text_start" => {
                self.slot(content_index, || Content::Text {
                    text: String::new(),
                    text_signature: None,
                });
                Ok(Some(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: self.partial.clone(),
                }))
            }
            "text_delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Content::Text { text, .. } = self.slot(content_index, || Content::Text {
                    text: String::new(),
                    text_signature: None,
                }) {
                    text.push_str(&delta);
                }
                Ok(Some(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                    partial: self.partial.clone(),
                }))
            }
            "text_end" => {
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let signature = event
                    .get("contentSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(Content::Text {
                    text,
                    text_signature,
                }) = self.block_mut(content_index)
                {
                    *text = content.clone();
                    *text_signature = signature;
                }
                Ok(Some(AssistantMessageEvent::TextEnd {
                    content_index,
                    content,
                    partial: self.partial.clone(),
                }))
            }
            "thinking_start" => {
                self.slot(content_index, || Content::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                });
                Ok(Some(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: self.partial.clone(),
                }))
            }
            "thinking_delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Content::Thinking { thinking, .. } =
                    self.slot(content_index, || Content::Thinking {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    })
                {
                    thinking.push_str(&delta);
                }
                Ok(Some(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: self.partial.clone(),
                }))
            }
            "thinking_end" => {
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let signature = event
                    .get("contentSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let redacted = event.get("redacted").and_then(Value::as_bool);
                if let Some(Content::Thinking {
                    thinking: thinking_text,
                    thinking_signature,
                    redacted: redacted_slot,
                }) = self.block_mut(content_index)
                {
                    *thinking_text = content.clone();
                    *thinking_signature = signature;
                    *redacted_slot = redacted;
                }
                Ok(Some(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial: self.partial.clone(),
                }))
            }
            "toolcall_start" => {
                let id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool_name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.slot(content_index, || Content::ToolCall {
                    id: id.clone(),
                    name: tool_name.clone(),
                    arguments: json!({}),
                    thought_signature: None,
                    namespace: None,
                });
                self.tool_json.insert(content_index, String::new());
                Ok(Some(AssistantMessageEvent::ToolcallStart {
                    content_index,
                    partial: self.partial.clone(),
                }))
            }
            "toolcall_delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let json_buffer = self.tool_json.entry(content_index).or_default();
                json_buffer.push_str(&delta);
                let arguments = parse_streaming_json(Some(json_buffer));
                if let Some(Content::ToolCall {
                    arguments: slot, ..
                }) = self.block_mut(content_index)
                {
                    *slot = arguments;
                }
                Ok(Some(AssistantMessageEvent::ToolcallDelta {
                    content_index,
                    delta,
                    partial: self.partial.clone(),
                }))
            }
            "toolcall_end" => {
                let tool_call_value = event.get("toolCall").cloned().unwrap_or(Value::Null);
                let wire_call = wire_tool_call(&tool_call_value)?;
                if let Some(Content::ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature,
                    namespace,
                }) = self.block_mut(content_index)
                {
                    *id = wire_call.id.clone();
                    *name = wire_call.name.clone();
                    *arguments = wire_call.arguments.clone();
                    *thought_signature = wire_call.thought_signature.clone();
                    *namespace = wire_call.namespace.clone();
                }
                self.tool_json.remove(&content_index);
                Ok(Some(AssistantMessageEvent::ToolcallEnd {
                    content_index,
                    tool_call: self.partial.content[content_index].clone(),
                    partial: self.partial.clone(),
                }))
            }
            "done" | "error" => {
                // Terminal: handled by the caller (it needs the usage/rewrite
                // data). Not expected inside the event loop.
                Err(format!("unexpected terminal event: {event_type}"))
            }
            other => Err(format!("unknown pi-messages event type: {other}")),
        }
    }
}

// --- SSE reading -----------------------------------------------------------

/// Wire `toolCall` payload: `{ type, id, name, arguments, ... }`.
struct WireToolCall {
    id: String,
    name: String,
    arguments: Value,
    thought_signature: Option<String>,
    namespace: Option<String>,
}

fn wire_tool_call(value: &Value) -> Result<WireToolCall, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "invalid toolCall event: not an object".to_string())?;
    Ok(WireToolCall {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        name: object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        arguments: object.get("arguments").cloned().unwrap_or(json!({})),
        thought_signature: object
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .map(str::to_string),
        namespace: object
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Parse one SSE-framed wire event (`data: <json>` line, `[DONE]` sentinel).
fn parse_pi_messages_event(raw: &str) -> Option<Value> {
    let data = raw
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))
        .map(str::trim)?;
    if data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

/// Read all wire events from an SSE body, handling CRLF normalization and a
/// trailing unterminated event.
async fn read_pi_messages_events(
    body: crate::transport::ByteStream,
) -> Result<(Vec<Value>, Option<TerminalEvent>), ProviderRequestError> {
    use futures::StreamExt;
    let mut stream = body;
    let mut buffer = String::new();
    // A chunk boundary can split a multi-byte character; decode incrementally
    // so the torn bytes are carried to the next chunk instead of becoming U+FFFD.
    let mut decoder = crate::api::Utf8ChunkDecoder::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ProviderRequestError::transport(error.to_string()))?;
        buffer.push_str(&decoder.push(&chunk));
    }
    buffer.push_str(&decoder.finish());
    buffer = buffer.replace("\r\n", "\n");

    let mut events = Vec::new();
    let mut terminal = None;
    for frame in buffer.split("\n\n") {
        if frame.trim().is_empty() {
            continue;
        }
        let Some(event) = parse_pi_messages_event(frame) else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "done" => {
                terminal = Some(TerminalEvent::Done {
                    reason: event
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    usage: event
                        .get("usage")
                        .map(parse_usage)
                        .unwrap_or_else(empty_usage),
                    response_id: event
                        .get("responseId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    rewrite: event.get("rewrite").and_then(parse_rewrite),
                });
                break;
            }
            "error" => {
                terminal = Some(TerminalEvent::Error {
                    reason: event
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    usage: event
                        .get("usage")
                        .map(parse_usage)
                        .unwrap_or_else(empty_usage),
                    error_message: event
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    response_id: event
                        .get("responseId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    rewrite: event.get("rewrite").and_then(parse_rewrite),
                });
                break;
            }
            _ => events.push(event),
        }
    }
    Ok((events, terminal))
}

// --- Stream ----------------------------------------------------------------

fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&crate::types::ProviderEnv>,
) -> Option<CacheRetention> {
    if cache_retention.is_some() {
        return cache_retention;
    }
    // Backend defaults apply when unset; only the legacy env opt-in is mapped.
    (get_provider_env_value("PILLAR_CACHE_RETENTION", env).as_deref() == Some("long"))
        .then_some(CacheRetention::Long)
}

fn create_error_event(model: &Model, error: &str, aborted: bool) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: empty_usage(),
        stop_reason: if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        },
        deferred: None,
        error_message: Some(error.to_string()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

pub fn stream(
    model: Model,
    context: Context,
    options: Option<PiMessagesOptions>,
) -> AssistantMessageEventStream {
    let event_stream = crate::event_stream::assistant_message_event_stream();
    let task_stream = event_stream.clone_stream();
    tokio::spawn(run_stream(
        model,
        context,
        options.unwrap_or_default(),
        task_stream,
    ));
    event_stream
}

async fn run_stream(
    model: Model,
    context: Context,
    options: PiMessagesOptions,
    stream: AssistantMessageEventStream,
) {
    let result = run_stream_inner(&model, &context, &options, &stream).await;
    if let Err(error) = result {
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        let mut message = create_error_event(&model, &error.message, aborted);
        if !aborted && error.status.is_some() {
            append_assistant_message_diagnostic(
                &mut message,
                AssistantMessageDiagnostic {
                    kind: "pi_messages_response_failure".to_string(),
                    timestamp: now_ms(),
                    error: Some(crate::types::DiagnosticErrorInfo {
                        name: None,
                        message: error.message.clone(),
                        stack: None,
                        code: error.code.as_ref().map(|code| json!(code)),
                    }),
                    details: Some(json!({
                        "version": 1,
                        "provider": model.provider,
                        "model": model.id,
                        "url": error.url,
                        "status": error.status,
                        "statusText": error.status_text,
                        "error": error.error_body,
                        "body": error.body_excerpt,
                    })),
                },
            );
        }
        stream.push(AssistantMessageEvent::Error {
            reason: message.stop_reason,
            error: message.clone(),
        });
        stream.end(Some(message));
    }
}

/// Structured error carrying diagnostic context for non-2xx responses.
struct StreamFailure {
    message: String,
    status: Option<u16>,
    status_text: Option<String>,
    code: Option<String>,
    error_body: Option<Value>,
    url: String,
    body_excerpt: Option<String>,
}

fn truncate_diagnostic_string(value: &str) -> String {
    const MAX: usize = 8192;
    if value.len() > MAX {
        format!("{}…", &value[..MAX])
    } else {
        value.to_string()
    }
}

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &PiMessagesOptions,
    stream: &AssistantMessageEventStream,
) -> Result<(), StreamFailure> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("No API key provided for provider \"{}\"", model.provider))?
        .to_string();

    let mut url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    if options.debug.unwrap_or(false) {
        url.push_str("?debug=1");
    }

    let mut payload = json!({
        "model": model.id,
        "context": serde_json::to_value(context).map_err(|error| error.to_string())?,
        "options": {
            "temperature": options.temperature,
            "maxTokens": options.max_tokens,
            "reasoning": options.reasoning.map(|level| json!(level)),
            "cacheRetention": resolve_cache_retention(options.cache_retention, options.env.as_ref()),
            "sessionId": options.session_id,
            "toolChoice": options.tool_choice,
        },
    });
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_payload) = on_payload(model, payload.clone()).await {
            payload = next_payload;
        }
    }

    let mut headers = vec![
        ("User-Agent".to_string(), get_user_agent()),
        ("Authorization".to_string(), format!("Bearer {api_key}")),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    for (name, value) in options.headers.iter().flat_map(|headers| headers.iter()) {
        if let Some(value) = value {
            headers.push((name.clone(), value.clone()));
        }
    }

    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: url.clone(),
        headers,
        body: Some(serde_json::to_vec(&payload).map_err(|error| error.to_string())?),
    };

    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let fetch_for_retry = Arc::clone(&fetch);
    let request_for_retry = request.clone();
    let timeout_ms = options.timeout_ms;
    let signal = options.signal.clone();
    let (response, response_status, response_headers) = {
        let result = retry_provider_request(
            || {
                let fetch = Arc::clone(&fetch_for_retry);
                let request = request_for_retry.clone();
                let signal = signal.clone();
                async move {
                    crate::api::fetch_json_stream(&fetch, request, signal.as_ref(), timeout_ms)
                        .await
                }
            },
            crate::provider_retry::ProviderRetryOptions {
                max_retries: options.max_retries,
                max_retry_delay_ms: options.max_retry_delay_ms,
                signal: options.signal.clone(),
            },
        )
        .await
        .map_err(|error| provider_error_to_failure(error, &url))?;
        let status = result.status;
        let headers = result.headers.clone();
        (result, status, headers)
    };

    if let Some(on_response) = &options.on_response {
        on_response(
            ProviderResponseInfo {
                status: response_status,
                headers: response_headers,
            },
            model,
        )
        .await;
    }

    if response_status != 200 {
        let body = collect_body(response.body).await?;
        let body_text = String::from_utf8_lossy(&body).to_string();
        let error_body = parse_error_body(&body_text);
        let message =
            format_response_error(response_status, "Error", &body_text, error_body.as_ref());
        return Err(StreamFailure {
            message,
            status: Some(response_status),
            status_text: Some("Error".to_string()),
            code: error_body
                .as_ref()
                .and_then(|parsed| parsed.pointer("/error/code"))
                .and_then(Value::as_str)
                .map(str::to_string),
            error_body: error_body.clone(),
            url,
            body_excerpt: if error_body.is_some() {
                None
            } else {
                Some(truncate_diagnostic_string(&body_text))
            },
        });
    }

    let (events, terminal) = read_pi_messages_events(response.body)
        .await
        .map_err(|error| error.message)?;

    stream.push(AssistantMessageEvent::Start {
        partial: AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: empty_usage(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        },
    });

    let mut converter = EventConverter::new(model);
    for event in &events {
        if let Some(converted) = converter.convert(event)? {
            stream.push(converted);
        }
    }

    let mut partial = converter.partial;
    match terminal {
        Some(TerminalEvent::Done {
            reason,
            usage,
            response_id,
            rewrite,
        }) => {
            partial.stop_reason = reason
                .as_deref()
                .and_then(reason_to_stop_reason)
                .unwrap_or(StopReason::Stop);
            partial.usage = usage;
            partial.response_id = response_id;
            append_rewrite_diagnostic(&mut partial, &rewrite);
            stream.push(AssistantMessageEvent::Done {
                reason: partial.stop_reason,
                message: partial.clone(),
            });
            stream.end(Some(partial));
            Ok(())
        }
        Some(TerminalEvent::Error {
            reason,
            usage,
            error_message,
            response_id,
            rewrite,
        }) => {
            partial.stop_reason = reason
                .as_deref()
                .and_then(reason_to_stop_reason)
                .unwrap_or(StopReason::Error);
            partial.usage = usage;
            partial.error_message = error_message;
            partial.response_id = response_id;
            append_rewrite_diagnostic(&mut partial, &rewrite);
            stream.push(AssistantMessageEvent::Error {
                reason: partial.stop_reason,
                error: partial.clone(),
            });
            stream.end(Some(partial));
            Ok(())
        }
        None => Err(StreamFailure::from(format!(
            "{} stream ended without a terminal event",
            model.provider
        ))),
    }
}

fn default_fetch() -> crate::transport::SharedFetchFn {
    Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

/// Convert a non-2xx transport error into a diagnostic-carrying failure
/// (upstream `createPiMessagesResponseError`).
fn provider_error_to_failure(error: ProviderRequestError, url: &str) -> StreamFailure {
    let body_text = error.message.clone();
    let error_body = parse_error_body(&body_text);
    let message = format_response_error(
        error.status.unwrap_or(0),
        "Error",
        &body_text,
        error_body.as_ref(),
    );
    StreamFailure {
        message,
        status: error.status,
        status_text: None,
        code: error_body
            .as_ref()
            .and_then(|parsed| parsed.pointer("/error/code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        error_body: error_body.clone(),
        url: url.to_string(),
        body_excerpt: if error_body.is_some() {
            None
        } else {
            Some(truncate_diagnostic_string(&body_text))
        },
    }
}

async fn collect_body(body: crate::transport::ByteStream) -> Result<Vec<u8>, String> {
    use futures::StreamExt;
    let mut stream = body;
    let mut buffered = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffered.extend_from_slice(&chunk.map_err(|error| error.to_string())?);
    }
    Ok(buffered)
}

/// Upstream `streamSimple`.
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.unwrap_or_default();
    stream(
        model,
        context,
        Some(PiMessagesOptions {
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
            max_tokens: options.max_tokens,
            cache_retention: options.cache_retention,
            session_id: options.session_id,
            reasoning: options.reasoning,
            tool_choice: options.tool_choice,
            debug: None,
        }),
    )
}

impl From<String> for StreamFailure {
    fn from(message: String) -> Self {
        StreamFailure {
            message,
            status: None,
            status_text: None,
            code: None,
            error_body: None,
            url: String::new(),
            body_excerpt: None,
        }
    }
}
