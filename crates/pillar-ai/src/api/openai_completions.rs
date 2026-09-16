//! Port of packages/ai/src/api/openai-completions.ts (pi v0.84.3).
//!
//! divergence: the OpenAI SDK is replaced by direct JSON requests through
//! [`crate::transport::FetchFn`] with local SSE parsing. Request bodies are
//! `serde_json` objects, so object key order follows serde's ordering rather
//! than the SDK's insertion order (not observable to providers).

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::abort::AbortSignal;
use crate::api::impl_from_request_options;
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::{
    OnPayloadFn, OnResponseFn, ProviderResponseInfo, get_user_agent, merge_request_headers,
    resolve_cache_retention,
};
use crate::api::{fetch_json_stream, format_stream_error};
use crate::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta,
    create_grammar_tool_input_properties, get_grammar_tool_input, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::error::AiError;
use crate::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use crate::hash::short_hash;
use crate::json_parse::parse_streaming_json;
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::provider_retry::{ProviderRequestError, retry_provider_request};
use crate::simple_options::{
    clamp_max_tokens_to_context, clamp_thinking_budget_to_answer_room, thinking_budget_for_level,
};
use crate::text::sanitize_surrogates;
use crate::transform_messages::transform_messages;
use crate::types::AssistantMessageEvent;
use crate::types::{
    AssistantMessage, CacheRetention, Content, Context, Message, Model, ModelThinkingLevel,
    ProviderHeaders, StopReason, ThinkingBudgets, ThinkingLevel, Tool, ToolResultMessage,
    UserContent,
};

// --- Options -------------------------------------------------------------

/// Upstream `OpenAICompletionsOptions` (extends `StreamOptions`).
#[derive(Default)]
pub struct OpenaiCompletionsOptions {
    pub signal: Option<AbortSignal>,
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
    pub sampling_params: Option<Map<String, Value>>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<Map<String, Value>>,
    /// Upstream accepts the full SDK tool-choice shape; passed through.
    pub tool_choice: Option<Value>,
    pub reasoning_effort: Option<ThinkingLevel>,
    /// Token budgets per thinking level. Used when
    /// `compat.thinking_token_budget_field` is set or by
    /// `{ "$var": "thinking.budget" }`.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl_from_request_options!(OpenaiCompletionsOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for this API.
#[derive(Default)]
pub struct SimpleStreamOptions {
    pub signal: Option<AbortSignal>,
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
    pub sampling_params: Option<Map<String, Value>>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<Map<String, Value>>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

// --- Compat --------------------------------------------------------------

/// URL/provider auto-detected + model-overridden compatibility settings
/// (upstream `ResolvedOpenAICompletionsCompat`).
#[derive(Debug, Clone)]
pub struct ResolvedCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: ThinkingFormat,
    pub chat_template_kwargs: BTreeMap<String, Value>,
    pub chat_template_args: BTreeMap<String, Value>,
    pub open_router_routing: Option<Value>,
    pub vercel_gateway_routing: Option<Value>,
    pub zai_tool_stream: bool,
    pub supports_thinking_token_budget: bool,
    pub thinking_token_budget_field: Option<String>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub cache_control_format: Option<String>,
    pub send_session_affinity_headers: bool,
    pub deferred_tools_mode: Option<String>,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingFormat {
    Openai,
    Openrouter,
    Deepseek,
    Together,
    Baseten,
    Zai,
    Qwen,
    ChatTemplate,
    QwenChatTemplate,
    StringThinking,
    AntLing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAffinityFormat {
    Openai,
    OpenaiNosession,
    Openrouter,
}

fn is_openai_completions_reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning" | "reasoning_content" | "reasoning_text")
}

// --- Small helpers -------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    let expected = name.to_lowercase();
    headers.iter().any(|(key, value)| {
        key.to_lowercase() == expected
            && value.as_ref().is_some_and(|value| !value.trim().is_empty())
    })
}

pub fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, AiError> {
    if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        return Ok(api_key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    Err(AiError::Other(format!(
        "No API key for provider: {provider}"
    )))
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall { .. })),
        _ => false,
    })
}

fn get_deferred_tool_names(messages: &[Message]) -> Vec<String> {
    let mut names = Vec::new();
    for message in messages {
        if let Message::ToolResult(tool_result) = message {
            for name in tool_result.added_tool_names.iter().flatten() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

fn get_tools_by_name<'a>(tools: Option<&'a [Tool]>, names: &[String]) -> Vec<&'a Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|name| tools.iter().find(|tool| &tool.name == name))
        .collect()
}

// --- Reasoning details (replay metadata) ---------------------------------

#[derive(Debug, Clone)]
pub(crate) enum ReasoningDetail {
    Summary {
        id: Option<String>,
        format: Option<String>,
        index: Option<f64>,
        summary: String,
    },
    Encrypted {
        id: Option<String>,
        format: Option<String>,
        index: Option<f64>,
        data: String,
    },
    Text {
        id: Option<String>,
        format: Option<String>,
        index: Option<f64>,
        text: String,
        signature: Option<String>,
    },
}

impl ReasoningDetail {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        let (kind, payload): (&str, Value) = match self {
            ReasoningDetail::Summary { summary, .. } => {
                ("reasoning.summary", Value::String(summary.clone()))
            }
            ReasoningDetail::Encrypted { data, .. } => {
                ("reasoning.encrypted", Value::String(data.clone()))
            }
            ReasoningDetail::Text { text, .. } => ("reasoning.text", Value::String(text.clone())),
        };
        let (id, format, index) = match self {
            ReasoningDetail::Summary {
                id, format, index, ..
            }
            | ReasoningDetail::Encrypted {
                id, format, index, ..
            }
            | ReasoningDetail::Text {
                id, format, index, ..
            } => (id, format, index),
        };
        map.insert("type".to_string(), Value::String(kind.to_string()));
        if let Some(id) = id {
            map.insert("id".to_string(), Value::String(id.clone()));
        }
        if let Some(format) = format {
            map.insert("format".to_string(), Value::String(format.clone()));
        }
        if let Some(index) = index {
            map.insert("index".to_string(), serde_json::json!(index));
        }
        match (kind, payload) {
            ("reasoning.summary", payload)
            | ("reasoning.encrypted", payload)
            | ("reasoning.text", payload) => {
                let key = match kind {
                    "reasoning.summary" => "summary",
                    "reasoning.encrypted" => "data",
                    _ => "text",
                };
                map.insert(key.to_string(), payload);
                if let ReasoningDetail::Text {
                    signature: Some(signature),
                    ..
                } = self
                {
                    map.insert("signature".to_string(), Value::String(signature.clone()));
                }
            }
            _ => unreachable!("covered above"),
        }
        Value::Object(map)
    }

    fn parse(detail: &Value) -> Option<ReasoningDetail> {
        let map = detail.as_object()?;
        let common_valid = map.get("id").is_none_or(|v| v.is_null() || v.is_string())
            && map.get("format").is_none_or(|v| v.is_string())
            && map.get("index").is_none_or(|v| v.is_number());
        if !common_valid {
            return None;
        }
        let id = map.get("id").and_then(Value::as_str).map(str::to_string);
        let format = map
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string);
        let index = map.get("index").and_then(Value::as_f64);
        match map.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => {
                let summary = map.get("summary")?.as_str()?.to_string();
                Some(ReasoningDetail::Summary {
                    id,
                    format,
                    index,
                    summary,
                })
            }
            Some("reasoning.encrypted") => {
                let data = map.get("data")?.as_str()?.to_string();
                Some(ReasoningDetail::Encrypted {
                    id,
                    format,
                    index,
                    data,
                })
            }
            Some("reasoning.text") => {
                let text = map.get("text")?.as_str()?.to_string();
                let signature = map
                    .get("signature")
                    .map(|value| (!value.is_null()).then(|| value.as_str().map(str::to_string)))
                    .unwrap_or(None)
                    .flatten();
                Some(ReasoningDetail::Text {
                    id,
                    format,
                    index,
                    text,
                    signature,
                })
            }
            _ => None,
        }
    }

    fn fill_missing_common_fields(target: &mut ReasoningDetail, source: &ReasoningDetail) {
        let (target_id, target_format, target_index) = match target {
            ReasoningDetail::Summary {
                id, format, index, ..
            }
            | ReasoningDetail::Encrypted {
                id, format, index, ..
            }
            | ReasoningDetail::Text {
                id, format, index, ..
            } => (id, format, index),
        };
        let (source_id, source_format, source_index) = match source {
            ReasoningDetail::Summary {
                id, format, index, ..
            }
            | ReasoningDetail::Encrypted {
                id, format, index, ..
            }
            | ReasoningDetail::Text {
                id, format, index, ..
            } => (id, format, index),
        };
        if target_id.is_none() {
            *target_id = source_id.clone();
        }
        if target_format.as_deref().map(str::is_empty).unwrap_or(true) {
            *target_format = source_format.clone();
        }
        if target_index.is_none() {
            *target_index = *source_index;
        }
    }
}

fn parse_openai_reasoning_details(signature: Option<&str>) -> Option<Vec<ReasoningDetail>> {
    let signature = signature?;
    let parsed = serde_json::from_str::<Value>(signature).ok()?;
    let items = parsed.as_array()?;
    if items.is_empty() {
        return None;
    }
    let details: Option<Vec<ReasoningDetail>> = items.iter().map(ReasoningDetail::parse).collect();
    details
}

fn parse_legacy_encrypted_reasoning_detail(signature: Option<&str>) -> Option<ReasoningDetail> {
    let detail = parse_openai_reasoning_details(signature)?;
    match detail.into_iter().next()? {
        ReasoningDetail::Encrypted { id, data, .. }
            if !id.clone().unwrap_or_default().is_empty() && !data.is_empty() =>
        {
            Some(ReasoningDetail::Encrypted {
                id,
                format: None,
                index: None,
                data,
            })
        }
        _ => None,
    }
}

fn append_openai_reasoning_detail(details: &mut Vec<ReasoningDetail>, detail: ReasoningDetail) {
    if let (
        Some(last),
        ReasoningDetail::Text {
            text, signature, ..
        },
    ) = (details.last_mut(), &detail)
    {
        if let ReasoningDetail::Text {
            text: last_text,
            signature: last_signature,
            ..
        } = last
        {
            last_text.push_str(text);
            if last_signature.is_none() {
                *last_signature = signature.clone();
            }
            ReasoningDetail::fill_missing_common_fields(last, &detail);
            return;
        }
    }
    if let (Some(last), ReasoningDetail::Summary { summary, .. }) = (details.last_mut(), &detail) {
        if let ReasoningDetail::Summary {
            summary: last_summary,
            ..
        } = last
        {
            last_summary.push_str(summary);
            ReasoningDetail::fill_missing_common_fields(last, &detail);
            return;
        }
    }
    details.push(detail);
}

// --- SSE -----------------------------------------------------------------

/// Yields `data:` payloads from an SSE byte stream (upstream: the SDK's
/// stream decoder). Handles CRLF, multi-line data, comments, and flushes a
/// final unterminated event.
pub struct SseDataEvents {
    byte_stream: crate::transport::ByteStream,
    buffer: Vec<u8>,
    finished: bool,
}

impl SseDataEvents {
    pub(crate) fn new(byte_stream: crate::transport::ByteStream) -> Self {
        Self {
            byte_stream,
            buffer: Vec::new(),
            finished: false,
        }
    }

    /// Extract the next complete event's data payload from the buffer.
    /// Returns `None` when no complete event is buffered yet.
    fn pop_event(&mut self) -> Option<String> {
        let (content_end, terminator_len) = find_event_end(&self.buffer)?;
        let event: Vec<u8> = self.buffer.drain(..content_end).collect();
        self.buffer.drain(..terminator_len);
        Some(decode_data_payload(&event))
    }
}

/// Returns (event content end, terminator length). Recognized event
/// terminators (blank lines): "\n\n", "\n\r\n", "\r\n\r\n".
fn find_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buffer.len() {
        if buffer[i] == b'\n' {
            if buffer.get(i + 1) == Some(&b'\n') {
                return Some((i, 2));
            }
            if buffer.get(i + 1) == Some(&b'\r') && buffer.get(i + 2) == Some(&b'\n') {
                return Some((i, 3));
            }
        }
        if buffer[i] == b'\r'
            && buffer.get(i + 1) == Some(&b'\n')
            && buffer.get(i + 2) == Some(&b'\r')
            && buffer.get(i + 3) == Some(&b'\n')
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn decode_data_payload(event: &[u8]) -> String {
    let text = String::from_utf8_lossy(event);
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    data_lines.join("\n")
}

impl Stream for SseDataEvents {
    type Item = Result<String, AiError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(payload) = self.pop_event() {
                return std::task::Poll::Ready(Some(Ok(payload)));
            }
            if self.finished {
                // Flush a final event without a trailing blank line.
                if self.buffer.iter().any(|b| !b.is_ascii_whitespace()) {
                    let event = std::mem::take(&mut self.buffer);
                    return std::task::Poll::Ready(Some(Ok(decode_data_payload(&event))));
                }
                return std::task::Poll::Ready(None);
            }
            match std::pin::Pin::new(&mut self.byte_stream).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => self.buffer.extend_from_slice(&chunk),
                std::task::Poll::Ready(Some(Err(error))) => {
                    self.finished = true;
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                std::task::Poll::Ready(None) => self.finished = true,
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

// --- Stream --------------------------------------------------------------

pub fn stream(
    model: Model,
    context: Context,
    options: Option<OpenaiCompletionsOptions>,
) -> AssistantMessageEventStream {
    let stream = assistant_message_event_stream();
    let task_stream = stream.clone_stream();
    tokio::spawn(run_stream(
        model,
        context,
        options.unwrap_or_default(),
        task_stream,
    ));
    stream
}

#[derive(Debug, Clone)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: Value,
    partial_args: Option<String>,
    custom_input: Option<(String, GrammarToolInputJsonBuffer)>,
    stream_index: Option<f64>,
}

#[derive(Debug, Clone)]
enum StreamingBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        thinking_signature: String,
    },
    ToolCall(StreamingToolCall),
}

impl StreamingBlock {
    fn to_content(&self) -> Content {
        match self {
            StreamingBlock::Text { text } => Content::text(text.clone()),
            StreamingBlock::Thinking {
                thinking,
                thinking_signature,
            } => Content::Thinking {
                thinking: thinking.clone(),
                thinking_signature: (!thinking_signature.is_empty())
                    .then(|| thinking_signature.clone()),
                redacted: None,
            },
            StreamingBlock::ToolCall(call) => Content::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                thought_signature: None,
                namespace: None,
            },
        }
    }
}

struct StreamState {
    output: AssistantMessage,
    blocks: Vec<StreamingBlock>,
    /// Upstream keeps one open text block for the whole stream.
    text_block_position: Option<usize>,
    streamed_reasoning_details: Option<Vec<ReasoningDetail>>,
    has_finish_reason: bool,
}

impl StreamState {
    fn new(model: &Model) -> Self {
        Self {
            output: AssistantMessage {
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: crate::api::zeroed_usage(),
                stop_reason: StopReason::Pending,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: now_ms(),
            },
            blocks: Vec::new(),
            text_block_position: None,
            streamed_reasoning_details: None,
            has_finish_reason: false,
        }
    }

    fn sync_content(&mut self) {
        self.output.content = self.blocks.iter().map(|block| block.to_content()).collect();
    }

    fn content_index(&self, position: usize) -> usize {
        position
    }

    fn apply_streamed_reasoning_details(&mut self) {
        if let Some(details) = &self.streamed_reasoning_details {
            let serialized = serde_json::to_string(
                &details
                    .iter()
                    .map(ReasoningDetail::to_value)
                    .collect::<Vec<_>>(),
            );
            if let Ok(serialized) = serialized {
                if let Some(StreamingBlock::Thinking {
                    thinking_signature, ..
                }) = self
                    .blocks
                    .iter_mut()
                    .find(|block| matches!(block, StreamingBlock::Thinking { .. }))
                {
                    *thinking_signature = serialized;
                }
            }
        }
    }
}

async fn run_stream(
    model: Model,
    context: Context,
    options: OpenaiCompletionsOptions,
    stream: AssistantMessageEventStream,
) {
    let mut state = StreamState::new(&model);
    let compat = get_compat(&model);

    let result = run_stream_inner(&model, &context, &options, &compat, &mut state, &stream).await;
    if let Err(error) = result {
        // Apply replay metadata and strip scratch state from partials.
        state.apply_streamed_reasoning_details();
        state.output.stop_reason = if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        state.output.error_message = Some(format_stream_error(&error));
        stream.push(AssistantMessageEvent::Error {
            reason: state.output.stop_reason,
            error: state.output.clone(),
        });
        stream.end(Some(state.output.clone()));
    }
}

type RunResult = Result<(), ProviderRequestError>;

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &OpenaiCompletionsOptions,
    compat: &ResolvedCompat,
    state: &mut StreamState,
    stream: &AssistantMessageEventStream,
) -> RunResult {
    let api_key = get_client_api_key(
        &model.provider,
        options.api_key.as_deref(),
        options.headers.as_ref(),
    )
    .map_err(|error| ProviderRequestError::transport(error.to_string()))?;
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        Some(&context.tools),
        compat.supports_openai_grammar_tools,
    );
    let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let cache_session_id = match cache_retention {
        CacheRetention::None => None,
        _ => options.session_id.clone(),
    };
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let mut params = build_params(
        model,
        context,
        options,
        compat,
        cache_retention,
        &grammar_tool_input_properties,
    );
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers: build_request_headers(
            model,
            context,
            options,
            compat,
            &api_key,
            cache_session_id.as_deref(),
        ),
        body: Some(serde_json::to_vec(&params).map_err(|error| {
            ProviderRequestError::transport(format!("failed to serialize request body: {error}"))
        })?),
    };

    let fetch_for_retry = fetch.clone();
    let request_for_retry = request.clone();
    let timeout_ms = options.timeout_ms;
    let (response, response_status, response_headers) = {
        let result = retry_provider_request(
            || {
                let fetch = Arc::clone(&fetch_for_retry);
                let request = request_for_retry.clone();
                async move {
                    fetch_json_stream(&fetch, request, options.signal.as_ref(), timeout_ms).await
                }
            },
            crate::provider_retry::ProviderRetryOptions {
                max_retries: options.max_retries,
                max_retry_delay_ms: options.max_retry_delay_ms,
                signal: options.signal.clone(),
            },
        )
        .await?;
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

    stream.push(AssistantMessageEvent::Start {
        partial: state.output.clone(),
    });

    let sse = SseDataEvents::new(response.body);
    tokio::pin!(sse);
    loop {
        // Stop reading the body the moment the user aborts (upstream the
        // SDK's `abortSignal` cancelling the request). `break` rather than
        // propagate: the open content blocks must be flushed first so the
        // streamed partial text survives, and the post-loop check reports the
        // abort.
        let next = match crate::api::race_abort(options.signal.as_ref(), sse.next()).await {
            Ok(next) => next,
            Err(_) => break,
        };
        let Some(payload) = next else {
            break;
        };
        let payload =
            payload.map_err(|error| ProviderRequestError::transport(error.to_string()))?;
        if payload.trim() == "[DONE]" {
            break;
        }
        if payload.trim().is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&payload).map_err(|error| {
            ProviderRequestError::transport(format!("invalid SSE chunk JSON: {error}"))
        })?;
        process_chunk(
            model,
            chunk,
            compat,
            state,
            stream,
            &grammar_tool_input_properties,
        );
    }

    // Finish all open blocks.
    let positions: Vec<usize> = (0..state.blocks.len()).collect();
    for position in positions {
        finish_block(state, stream, position);
    }

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(ProviderRequestError {
            message: "Request was aborted".to_string(),
            aborted: true,
            ..ProviderRequestError::transport("")
        });
    }
    if state.output.stop_reason == StopReason::Aborted {
        return Err(ProviderRequestError {
            message: "Request was aborted".to_string(),
            aborted: true,
            ..ProviderRequestError::transport("")
        });
    }
    if !state.has_finish_reason && !compat.supports_finish_reason {
        state.output.stop_reason = if state
            .output
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if state.output.stop_reason == StopReason::Error {
        return Err(ProviderRequestError::transport(
            state
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_string()),
        ));
    }
    if (compat.supports_finish_reason && !state.has_finish_reason)
        || state.output.stop_reason == StopReason::Pending
    {
        return Err(ProviderRequestError::transport(
            "Stream ended without finish_reason".to_string(),
        ));
    }

    state.sync_content();
    let output = state.output.clone();
    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output));
    Ok(())
}

fn default_fetch() -> crate::transport::SharedFetchFn {
    Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_request_headers(
    model: &Model,
    context: &Context,
    options: &OpenaiCompletionsOptions,
    compat: &ResolvedCompat,
    api_key: &str,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut defaults = vec![
        ("User-Agent".to_string(), get_user_agent()),
        ("Authorization".to_string(), format!("Bearer {api_key}")),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if model.provider == "github-copilot" {
        let has_images =
            crate::api::github_copilot_headers::has_copilot_vision_input(&context.messages);
        defaults.extend(
            crate::api::github_copilot_headers::build_copilot_dynamic_headers(
                &context.messages,
                has_images,
            ),
        );
    }
    if let Some(session_id) = session_id {
        if compat.send_session_affinity_headers {
            match compat.session_affinity_format {
                SessionAffinityFormat::Openrouter => {
                    defaults.push(("x-session-id".to_string(), session_id.to_string()));
                }
                format @ (SessionAffinityFormat::Openai
                | SessionAffinityFormat::OpenaiNosession) => {
                    if format == SessionAffinityFormat::Openai {
                        defaults.push(("session_id".to_string(), session_id.to_string()));
                    }
                    defaults.push(("x-client-request-id".to_string(), session_id.to_string()));
                    defaults.push(("x-session-affinity".to_string(), session_id.to_string()));
                }
            }
        }
    }
    merge_request_headers(defaults, model.headers.as_ref(), options.headers.as_ref())
}

/// Map a `finish_reason` to the pillar stop reason (upstream `mapStopReason`).
pub fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("Provider finish_reason: content_filter".to_string()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("Provider finish_reason: network_error".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

/// Port of `parseChunkUsage`.
pub fn parse_chunk_usage(raw_usage: &Value, model: &Model) -> crate::types::Usage {
    let prompt_tokens = raw_usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt_details = raw_usage.get("prompt_tokens_details");
    let cache_read_tokens = prompt_details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            raw_usage
                .get("prompt_cache_hit_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| raw_usage.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write_tokens = prompt_details
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Follow documented OpenAI/OpenRouter semantics: cached_tokens is
    // cache-read tokens (hits). Placement differs per provider (see upstream
    // comment); do not subtract writes from cached_tokens.
    let input = prompt_tokens.saturating_sub(cache_read_tokens + cache_write_tokens);
    let output_tokens = raw_usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = raw_usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut usage = crate::types::Usage {
        input,
        output: output_tokens,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        cache_write_1h: None,
        reasoning: Some(reasoning).filter(|reasoning| *reasoning > 0),
        total_tokens: input + output_tokens + cache_read_tokens + cache_write_tokens,
        cost: Default::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

fn process_chunk(
    model: &Model,
    chunk: Value,
    _compat: &ResolvedCompat,
    state: &mut StreamState,
    stream: &AssistantMessageEventStream,
    grammar_properties: &BTreeMap<String, String>,
) {
    let Some(chunk_obj) = chunk.as_object() else {
        return;
    };

    // Each chunk in a streamed completion carries the same chat id.
    if state.output.response_id.is_none() {
        if let Some(id) = chunk_obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            state.output.response_id = Some(id.to_string());
        }
    }
    if let Some(chunk_model) = chunk_obj.get("model").and_then(Value::as_str) {
        if !chunk_model.is_empty()
            && chunk_model != model.id
            && state.output.response_model.is_none()
        {
            state.output.response_model = Some(chunk_model.to_string());
        }
    }
    if let Some(usage) = chunk_obj.get("usage").filter(|usage| !usage.is_null()) {
        state.output.usage = parse_chunk_usage(usage, model);
    }

    let Some(choice) = chunk_obj
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object)
    else {
        return;
    };

    // Fallback: some providers (e.g. Moonshot) return usage in choice.usage.
    if chunk_obj.get("usage").is_none() {
        if let Some(usage) = choice.get("usage").filter(|usage| !usage.is_null()) {
            state.output.usage = parse_chunk_usage(usage, model);
        }
    }

    if let Some(finish_reason) = choice.get("finish_reason") {
        if let Some(reason) = finish_reason.as_str() {
            state.output.raw_stop_reason = Some(reason.to_string());
            let (stop_reason, error_message) = map_stop_reason(reason);
            state.output.stop_reason = stop_reason;
            if let Some(error_message) = error_message {
                state.output.error_message = Some(error_message);
            }
            state.has_finish_reason = true;
        } else if finish_reason.is_null() {
            // Treated as no finish reason upstream (null finish_reason maps
            // to "stop" only when the SDK hands it over as an actual value;
            // chunks omit it entirely).
        }
    }

    let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
        return;
    };

    if let Some(content) = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
    {
        let position = ensure_text_block(state, stream);
        if let StreamingBlock::Text { text } = &mut state.blocks[position] {
            text.push_str(content);
        }
        state.sync_content();
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: state.content_index(position),
            delta: content.to_string(),
            partial: state.output.clone(),
        });
    }

    // Some endpoints return reasoning in reasoning_content (llama.cpp),
    // reasoning (other OpenAI-compatible endpoints), or reasoning_text. Use
    // the first non-empty field to avoid duplication.
    const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];
    let mut found_reasoning_field: Option<(&str, &str)> = None;
    for field in REASONING_FIELDS {
        if let Some(Value::String(value)) = delta.get(field) {
            if !value.is_empty() {
                found_reasoning_field = Some((field, value));
                break;
            }
        }
    }
    if let Some((field, delta_text)) = found_reasoning_field {
        let thinking_signature = if model.provider == "opencode-go" && field == "reasoning" {
            "reasoning_content"
        } else {
            field
        };
        let position = ensure_thinking_block(state, thinking_signature);
        if let StreamingBlock::Thinking { thinking, .. } = &mut state.blocks[position] {
            thinking.push_str(delta_text);
        }
        state.sync_content();
        stream.push(AssistantMessageEvent::ThinkingDelta {
            content_index: state.content_index(position),
            delta: delta_text.to_string(),
            partial: state.output.clone(),
        });
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let position = ensure_tool_call_block(state, tool_call, grammar_properties, stream);
            let obj = tool_call.as_object();
            let tool_id = obj
                .and_then(|obj| obj.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let function_name = obj
                .and_then(|obj| obj.get("function"))
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str);
            let custom_name = obj
                .and_then(|obj| obj.get("custom"))
                .and_then(|custom| custom.get("name"))
                .and_then(Value::as_str);
            let function_args = obj
                .and_then(|obj| obj.get("function"))
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str);
            let custom_input = obj
                .and_then(|obj| obj.get("custom"))
                .and_then(|custom| custom.get("input"))
                .and_then(Value::as_str);

            let StreamingBlock::ToolCall(call) = &mut state.blocks[position] else {
                unreachable!("looked up a tool call block");
            };
            if call.id.is_empty() && !tool_id.is_empty() {
                call.id = tool_id.clone();
            }
            if call.name.is_empty() {
                let name = function_name.or(custom_name).unwrap_or("");
                if !name.is_empty() {
                    call.name = name.to_string();
                }
            }

            let mut delta_text = String::new();
            if let Some(arguments) = function_args {
                delta_text = arguments.to_string();
                let existing = call.partial_args.clone().unwrap_or_default();
                call.partial_args = Some(format!("{existing}{arguments}"));
                call.arguments = parse_streaming_json(call.partial_args.as_deref());
            } else if let Some(input) = custom_input {
                let property = call
                    .custom_input
                    .as_ref()
                    .map(|(property, _)| property.clone())
                    .unwrap_or_else(|| "input".to_string());
                let current = call
                    .arguments
                    .get(&property)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let next_input = format!("{current}{input}");
                if let Some((_, buffer)) = &mut call.custom_input {
                    delta_text =
                        append_grammar_tool_input_json_delta(buffer, &property, &next_input, false)
                            .unwrap_or(None)
                            .unwrap_or_default();
                }
                call.arguments = serde_json::json!({ property: next_input });
            }
            state.sync_content();
            stream.push(AssistantMessageEvent::ToolcallDelta {
                content_index: state.content_index(position),
                delta: delta_text,
                partial: state.output.clone(),
            });
        }
    }

    if let Some(reasoning_details) = delta.get("reasoning_details").and_then(Value::as_array) {
        for detail in reasoning_details {
            let Some(detail) = ReasoningDetail::parse(detail) else {
                continue;
            };
            ensure_thinking_block(state, "");
            state
                .streamed_reasoning_details
                .get_or_insert_with(Vec::new);
            append_openai_reasoning_detail(
                state
                    .streamed_reasoning_details
                    .as_mut()
                    .expect("just initialized"),
                detail,
            );
        }
    }
}

// (grammar properties are passed explicitly from run_stream_inner)

fn ensure_text_block(state: &mut StreamState, stream: &AssistantMessageEventStream) -> usize {
    // Upstream keeps ONE open text block for the whole stream: every text
    // delta appends to it, wherever it appears between other blocks.
    if let Some(position) = state.text_block_position {
        return position;
    }
    state.blocks.push(StreamingBlock::Text {
        text: String::new(),
    });
    state.sync_content();
    let index = state.blocks.len() - 1;
    state.text_block_position = Some(index);
    stream.push(AssistantMessageEvent::TextStart {
        content_index: state.content_index(index),
        partial: state.output.clone(),
    });
    index
}
fn ensure_thinking_block(state: &mut StreamState, thinking_signature: &str) -> usize {
    if let Some(position) = state
        .blocks
        .iter()
        .rposition(|block| matches!(block, StreamingBlock::Thinking { .. }))
    {
        return position;
    }
    state.blocks.push(StreamingBlock::Thinking {
        thinking: String::new(),
        thinking_signature: thinking_signature.to_string(),
    });
    state.sync_content();
    state.blocks.len() - 1
}

fn ensure_tool_call_block(
    state: &mut StreamState,
    tool_call: &Value,
    grammar_properties: &BTreeMap<String, String>,
    stream: &AssistantMessageEventStream,
) -> usize {
    let obj = tool_call.as_object();
    let stream_index = obj.and_then(|obj| obj.get("index")).and_then(Value::as_f64);
    let id = obj
        .and_then(|obj| obj.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let function_name = obj
        .and_then(|obj| obj.get("function"))
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str);
    let custom_name = obj
        .and_then(|obj| obj.get("custom"))
        .and_then(|custom| custom.get("name"))
        .and_then(Value::as_str);
    let name = function_name.or(custom_name).unwrap_or("").to_string();
    let has_function = obj.is_some_and(|obj| obj.get("function").is_some());
    let is_custom = obj.is_some_and(|obj| obj.get("custom").is_some());

    let mut position = stream_index.and_then(|index| {
        state.blocks.iter().position(|block| {
            matches!(block, StreamingBlock::ToolCall(call) if call.stream_index == Some(index))
        })
    });
    if position.is_none() && !id.is_empty() {
        position = state
            .blocks
            .iter()
            .position(|block| matches!(block, StreamingBlock::ToolCall(call) if call.id == id));
    }

    let position = match position {
        Some(position) => position,
        None => {
            // The "input" fallback should/must not be taken in practice; it
            // gives the parser a place to stash arguments for unknown tools.
            let custom_input_property = if is_custom && !has_function {
                grammar_properties
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| "input".to_string())
            } else {
                String::new()
            };
            let has_custom_input = is_custom && !has_function;
            let call = StreamingToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: if has_custom_input {
                    serde_json::json!({ custom_input_property.clone(): "" })
                } else {
                    Value::Object(Map::new())
                },
                partial_args: (!has_custom_input).then(String::new),
                custom_input: has_custom_input.then(|| {
                    (
                        custom_input_property.clone(),
                        GrammarToolInputJsonBuffer::default(),
                    )
                }),
                stream_index,
            };
            state.blocks.push(StreamingBlock::ToolCall(call));
            state.sync_content();
            let index = state.blocks.len() - 1;
            stream.push(AssistantMessageEvent::ToolcallStart {
                content_index: state.content_index(index),
                partial: state.output.clone(),
            });
            index
        }
    };

    let StreamingBlock::ToolCall(call) = &mut state.blocks[position] else {
        unreachable!("looked up a tool call block");
    };
    if call.stream_index.is_none() {
        call.stream_index = stream_index;
    }
    if !id.is_empty() && call.id.is_empty() {
        call.id = id.clone();
    }
    if call.name.is_empty() && !name.is_empty() {
        call.name = name.clone();
    }
    if is_custom && !has_function && call.custom_input.is_none() {
        let custom_input_property = grammar_properties
            .get(&call.name)
            .cloned()
            .unwrap_or_else(|| "input".to_string());
        call.arguments = serde_json::json!({ custom_input_property.clone(): "" });
        call.custom_input = Some((custom_input_property, GrammarToolInputJsonBuffer::default()));
        call.partial_args = None;
    }
    position
}

fn finish_block(state: &mut StreamState, stream: &AssistantMessageEventStream, position: usize) {
    if position >= state.blocks.len() {
        return;
    }
    match state.blocks[position].clone() {
        StreamingBlock::Text { text } => {
            stream.push(AssistantMessageEvent::TextEnd {
                content_index: state.content_index(position),
                content: text,
                partial: state.output.clone(),
            });
        }
        StreamingBlock::Thinking {
            thinking,
            thinking_signature,
        } => {
            state.apply_streamed_reasoning_details();
            let content = match &state.blocks[position] {
                StreamingBlock::Thinking { thinking, .. } => thinking.clone(),
                _ => thinking,
            };
            let _ = thinking_signature;
            stream.push(AssistantMessageEvent::ThinkingEnd {
                content_index: state.content_index(position),
                content,
                partial: state.output.clone(),
            });
        }
        StreamingBlock::ToolCall(mut call) => {
            let delta = if let Some((property, buffer)) = &mut call.custom_input {
                let property = property.clone();
                let current_input = call
                    .arguments
                    .get(&property)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                append_grammar_tool_input_json_delta(buffer, &property, &current_input, true)
                    .ok()
                    .flatten()
            } else {
                call.arguments = parse_streaming_json(call.partial_args.as_deref());
                None
            };
            if let Some(delta) = delta {
                stream.push(AssistantMessageEvent::ToolcallDelta {
                    content_index: state.content_index(position),
                    delta,
                    partial: state.output.clone(),
                });
            }
            state.blocks[position] = StreamingBlock::ToolCall(StreamingToolCall {
                partial_args: None,
                custom_input: None,
                ..call
            });
            state.sync_content();
            let Content::ToolCall { .. } = state.blocks[position].to_content() else {
                unreachable!("tool call block");
            };
            stream.push(AssistantMessageEvent::ToolcallEnd {
                content_index: state.content_index(position),
                tool_call: state.blocks[position].to_content(),
                partial: state.output.clone(),
            });
        }
    }
}

#[allow(dead_code)]
fn unused_tool_result_type(_: &ToolResultMessage) {}

#[allow(dead_code)]
fn unused_thinking_budgets(_: &ThinkingBudgets) {}

#[allow(dead_code)]
fn unused_thinking_level(_: &ThinkingLevel) {}

#[allow(dead_code)]
fn unused_model_thinking_level(_: &ModelThinkingLevel) {}

// --- Request building ----------------------------------------------------

/// Port of `convertMessages`.
pub fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &ResolvedCompat,
    grammar_tool_input_properties: Option<&BTreeMap<String, String>>,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();

    let normalize_tool_call_id = |id: &str| -> String {
        // Handle pipe-separated IDs from OpenAI Responses API:
        // {call_id}|{id} where {id} can be 400+ chars with special chars.
        // Preserve item-level uniqueness when replaying into Chat
        // Completions, which requires distinct tool call ids.
        if id.contains('|') {
            let separator_index = id.find('|').expect("contains |");
            let call_id: String = id[..separator_index]
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let item_id: String = id[separator_index + 1..]
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let combined_id = if !item_id.is_empty() {
                format!("{call_id}_{item_id}")
            } else {
                call_id.clone()
            };
            if combined_id.chars().count() <= 40 {
                return combined_id;
            }
            let hash = short_hash(id)[..8].to_string();
            let keep = 40usize.saturating_sub(hash.chars().count() + 1).max(1);
            let prefix: String = call_id.chars().take(keep).collect();
            return format!("{prefix}_{hash}");
        }

        if model.provider == "openai" {
            return id.chars().take(40).collect();
        }
        id.to_string()
    };

    let transformed_messages = transform_messages(
        context.messages.clone(),
        model,
        Some(&|id, _model, _source| normalize_tool_call_id(id)),
    );

    if let Some(system_prompt) = &context.system_prompt {
        let use_developer_role = model.reasoning && compat.supports_developer_role;
        let role = if use_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(
            serde_json::json!({ "role": role, "content": sanitize_surrogates(system_prompt) }),
        );
    }

    let mut last_role: Option<String> = None;

    let mut index = 0;
    while index < transformed_messages.len() {
        let message = &transformed_messages[index];
        // Some providers don't allow user messages directly after tool
        // results; insert a synthetic assistant message to bridge the gap.
        if compat.requires_assistant_after_tool_result
            && last_role.as_deref() == Some("toolResult")
            && matches!(message, Message::User { .. })
        {
            params.push(serde_json::json!({
                "role": "assistant",
                "content": "I have processed the tool results."
            }));
        }

        match message {
            Message::User { content, .. } => match content {
                UserContent::Text(text) => {
                    params.push(serde_json::json!({
                        "role": "user",
                        "content": sanitize_surrogates(text)
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|block| match block {
                            Content::Text { text, .. } => serde_json::json!({
                                "type": "text",
                                "text": sanitize_surrogates(text)
                            }),
                            Content::Image { data, mime_type } => serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime_type};base64,{data}") }
                            }),
                            _ => Value::Null,
                        })
                        .filter(|part| !part.is_null())
                        .collect();
                    if !parts.is_empty() {
                        params.push(serde_json::json!({ "role": "user", "content": parts }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                // Some providers don't accept null content, use empty string
                // instead when a bridge assistant is required.
                let mut assistant_msg = Map::new();
                assistant_msg.insert("role".to_string(), Value::String("assistant".to_string()));
                assistant_msg.insert(
                    "content".to_string(),
                    if compat.requires_assistant_after_tool_result {
                        Value::String(String::new())
                    } else {
                        Value::Null
                    },
                );

                let text_blocks: Vec<&Content> = assistant
                    .content
                    .iter()
                    .filter(|block| {
                        matches!(block, Content::Text { text, .. } if !text.trim().is_empty())
                    })
                    .collect();
                let assistant_text_parts: Vec<Value> = text_blocks
                    .iter()
                    .map(|block| match block {
                        Content::Text { text, .. } => serde_json::json!({
                            "type": "text",
                            "text": sanitize_surrogates(text)
                        }),
                        _ => Value::Null,
                    })
                    .filter(|part| !part.is_null())
                    .collect();
                let assistant_text: String = text_blocks
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                let thinking_blocks: Vec<&Content> = assistant
                    .content
                    .iter()
                    .filter(|block| matches!(block, Content::Thinking { .. }))
                    .collect();
                let tool_calls: Vec<&Content> = assistant
                    .content
                    .iter()
                    .filter(|block| matches!(block, Content::ToolCall { .. }))
                    .collect();

                let signed_reasoning_details = thinking_blocks
                    .iter()
                    .filter_map(|block| match block {
                        Content::Thinking {
                            thinking_signature, ..
                        } => parse_openai_reasoning_details(thinking_signature.as_deref()),
                        _ => None,
                    })
                    .next();
                let legacy_reasoning_details: Vec<ReasoningDetail> = tool_calls
                    .iter()
                    .filter_map(|block| match block {
                        Content::ToolCall {
                            thought_signature, ..
                        } => parse_legacy_encrypted_reasoning_detail(thought_signature.as_deref()),
                        _ => None,
                    })
                    .collect();
                let preserved_reasoning_details = signed_reasoning_details
                    .or((!legacy_reasoning_details.is_empty()).then_some(legacy_reasoning_details));

                let non_empty_thinking_blocks: Vec<&Content> = thinking_blocks
                    .iter()
                    .copied()
                    .filter(|block| match block {
                        Content::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                        _ => false,
                    })
                    .collect();

                if !non_empty_thinking_blocks.is_empty() {
                    if compat.requires_thinking_as_text {
                        // Convert thinking blocks to plain text (no tags to
                        // avoid model mimicking them).
                        let thinking_text = non_empty_thinking_blocks
                            .iter()
                            .filter_map(|block| match block {
                                Content::Thinking { thinking, .. } => {
                                    Some(sanitize_surrogates(thinking))
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut content_parts =
                            vec![serde_json::json!({ "type": "text", "text": thinking_text })];
                        content_parts.extend(assistant_text_parts.clone());
                        assistant_msg.insert("content".to_string(), Value::Array(content_parts));
                    } else {
                        // Always send assistant content as a plain string
                        // (OpenAI Chat Completions API standard format).
                        if !assistant_text.is_empty() {
                            assistant_msg.insert(
                                "content".to_string(),
                                Value::String(assistant_text.clone()),
                            );
                        }

                        // reasoning_details is the structured alternative to
                        // a raw reasoning field.
                        if preserved_reasoning_details.is_none() {
                            // Use the signature from the first thinking block
                            // if available (llama.cpp server + gpt-oss).
                            let mut signature = match non_empty_thinking_blocks.first() {
                                Some(Content::Thinking {
                                    thinking_signature, ..
                                }) => thinking_signature.clone(),
                                _ => None,
                            };
                            if model.provider == "opencode-go"
                                && signature.as_deref() == Some("reasoning")
                            {
                                signature = Some("reasoning_content".to_string());
                            }
                            if let Some(signature) = signature.filter(|signature| {
                                is_openai_completions_reasoning_field(signature)
                            }) {
                                let reasoning = non_empty_thinking_blocks
                                    .iter()
                                    .filter_map(|block| match block {
                                        Content::Thinking { thinking, .. } => {
                                            Some(thinking.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                assistant_msg.insert(signature, Value::String(reasoning));
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg.insert("content".to_string(), Value::String(assistant_text));
                }

                if !tool_calls.is_empty() {
                    let converted: Vec<Value> = tool_calls
                        .iter()
                        .filter_map(|block| match block {
                            Content::ToolCall { id, name, arguments, .. } => {
                                let custom_input_property = grammar_tool_input_properties
                                    .and_then(|properties| properties.get(name).cloned());
                                if let Some(custom_input_property) = custom_input_property {
                                    let input = get_grammar_tool_input(name, arguments, &custom_input_property)
                                        .unwrap_or_default();
                                    return Some(serde_json::json!({
                                        "id": id,
                                        "type": "custom",
                                        "custom": { "name": name, "input": sanitize_surrogates(&input) }
                                    }));
                                }
                                Some(serde_json::json!({
                                    "id": id,
                                    "type": "function",
                                    "function": { "name": name, "arguments": arguments.to_string() }
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    assistant_msg.insert("tool_calls".to_string(), Value::Array(converted));
                }
                if let Some(details) = &preserved_reasoning_details {
                    assistant_msg.insert(
                        "reasoning_details".to_string(),
                        Value::Array(details.iter().map(ReasoningDetail::to_value).collect()),
                    );
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_msg.contains_key("reasoning_content")
                {
                    assistant_msg.insert(
                        "reasoning_content".to_string(),
                        Value::String(String::new()),
                    );
                }
                // Skip assistant messages that have no content and no tool
                // calls (aborted assistant responses that got no content).
                let has_content = match assistant_msg.get("content") {
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(parts)) => !parts.is_empty(),
                    _ => false,
                };
                if !has_content && !assistant_msg.contains_key("tool_calls") {
                    index += 1;
                    continue;
                }
                params.push(Value::Object(assistant_msg));
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut deferred_tool_names: Vec<String> = Vec::new();
                let mut j = index;

                while j < transformed_messages.len() {
                    let Message::ToolResult(tool_msg) = &transformed_messages[j] else {
                        break;
                    };

                    // Extract text and image content.
                    let text_result = tool_msg
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = tool_msg
                        .content
                        .iter()
                        .any(|block| matches!(block, Content::Image { .. }));

                    // Always send tool result with text (or placeholder if
                    // only images).
                    let has_text = !text_result.is_empty();
                    let tool_result_text = if has_text {
                        text_result
                    } else if has_images {
                        "(see attached image)".to_string()
                    } else {
                        "(no tool output)".to_string()
                    };
                    let mut tool_result_msg = Map::new();
                    tool_result_msg.insert("role".to_string(), Value::String("tool".to_string()));
                    tool_result_msg.insert(
                        "content".to_string(),
                        Value::String(sanitize_surrogates(&tool_result_text)),
                    );
                    tool_result_msg.insert(
                        "tool_call_id".to_string(),
                        Value::String(tool_msg.tool_call_id.clone()),
                    );
                    if compat.requires_tool_result_name && !tool_msg.tool_name.is_empty() {
                        tool_result_msg.insert(
                            "name".to_string(),
                            Value::String(tool_msg.tool_name.clone()),
                        );
                    }
                    params.push(Value::Object(tool_result_msg));

                    if compat.deferred_tools_mode.as_deref() == Some("kimi") {
                        for name in tool_msg.added_tool_names.iter().flatten() {
                            if !deferred_tool_names.contains(name) {
                                deferred_tool_names.push(name.clone());
                            }
                        }
                    }

                    if has_images && model.input.iter().any(|input| input == "image") {
                        for block in &tool_msg.content {
                            if let Content::Image { data, mime_type } = block {
                                image_blocks.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": { "url": format!("data:{mime_type};base64,{data}") }
                                }));
                            }
                        }
                    }
                    j += 1;
                }

                index = j - 1;

                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(serde_json::json!({
                            "role": "assistant",
                            "content": "I have processed the tool results."
                        }));
                    }
                    let mut content = vec![serde_json::json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:"
                    })];
                    content.extend(image_blocks);
                    params.push(serde_json::json!({ "role": "user", "content": content }));
                    last_role = Some("user".to_string());
                } else {
                    last_role = Some("toolResult".to_string());
                }

                if !deferred_tool_names.is_empty() {
                    let deferred_tools =
                        get_tools_by_name(Some(&context.tools), &deferred_tool_names);
                    if !deferred_tools.is_empty() {
                        let converted: Vec<Value> = deferred_tools
                            .iter()
                            .map(|tool| convert_tool(tool, compat))
                            .collect();
                        // Kimi accepts a system message with tools but omits
                        // the standard content field.
                        params.push(serde_json::json!({ "role": "system", "tools": converted }));
                    }
                }
                // Upstream continues here without the trailing lastRole
                // assignment; the group already set it above.
                index += 1;
                continue;
            }
        }

        last_role = Some(
            match message {
                Message::User { .. } => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
            }
            .to_string(),
        );
        index += 1;
    }

    params
}

fn convert_tool(tool: &Tool, compat: &ResolvedCompat) -> Value {
    if let Ok(Some(grammar)) =
        resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)
    {
        return serde_json::json!({
            "type": "custom",
            "custom": {
                "name": tool.name,
                "description": tool.description,
                "format": {
                    "type": "grammar",
                    "grammar": { "syntax": grammar.format, "definition": grammar.definition }
                }
            }
        });
    }

    let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)
        .unwrap_or_else(|error| panic!("tool \"{}\" strict sampling: {error}", tool.name));
    let parameters = get_json_schema_tool_parameters(tool, Some(strict.unwrap_or(false)))
        .unwrap_or_else(|error| panic!("tool \"{}\" parameters: {error}", tool.name));
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(tool.name.clone()));
    function.insert(
        "description".to_string(),
        Value::String(tool.description.clone()),
    );
    function.insert("parameters".to_string(), parameters);
    if compat.supports_strict_mode {
        function.insert("strict".to_string(), Value::Bool(strict.unwrap_or(false)));
    }
    serde_json::json!({ "type": "function", "function": function })
}

/// Port of `convertTools`.
pub fn convert_tools(tools: &[Tool], compat: &ResolvedCompat) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| convert_tool(tool, compat))
        .collect()
}

// --- Params --------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenaiCompletionsOptions,
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Value {
    let messages = convert_messages(model, context, compat, Some(grammar_tool_input_properties));
    let cache_control = get_compat_cache_control(compat, cache_retention);

    let mut params = Map::new();
    params.insert("model".to_string(), Value::String(model.id.clone()));
    params.insert("messages".to_string(), Value::Array(messages));
    params.insert("stream".to_string(), Value::Bool(true));

    let prompt_cache_key_condition = (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention);
    if prompt_cache_key_condition {
        if let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
            params.insert("prompt_cache_key".to_string(), Value::String(key));
        }
    }
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params.insert(
            "prompt_cache_retention".to_string(),
            Value::String("24h".to_string()),
        );
    }

    if compat.supports_usage_in_streaming {
        params.insert(
            "stream_options".to_string(),
            serde_json::json!({ "include_usage": true }),
        );
    }

    if compat.supports_store {
        params.insert("store".to_string(), Value::Bool(false));
    }

    if let Some(max_tokens) = options.max_tokens {
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => {
                params.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
            }
            MaxTokensField::MaxCompletionTokens => {
                params.insert(
                    "max_completion_tokens".to_string(),
                    serde_json::json!(max_tokens),
                );
            }
        }
    }

    if let Some(temperature) = options.temperature {
        params.insert("temperature".to_string(), serde_json::json!(temperature));
    }

    let deferred_tool_names = if compat.deferred_tools_mode.as_deref() == Some("kimi") {
        get_deferred_tool_names(&context.messages)
    } else {
        Vec::new()
    };
    let active_tools: Vec<&Tool> = context
        .tools
        .iter()
        .filter(|tool| !deferred_tool_names.contains(&tool.name))
        .collect();
    if !active_tools.is_empty() {
        let converted: Vec<Value> = active_tools
            .iter()
            .map(|tool| convert_tool(tool, compat))
            .collect();
        params.insert("tools".to_string(), Value::Array(converted));
        if compat.zai_tool_stream {
            params.insert("tool_stream".to_string(), Value::Bool(true));
        }
    } else if has_tool_history(&context.messages) {
        // Anthropic (via LiteLLM/proxy) requires tools param when the
        // conversation has tool_calls/tool_results.
        params.insert("tools".to_string(), Value::Array(Vec::new()));
    }

    if let Some(cache_control) = cache_control {
        apply_anthropic_cache_control(&mut params, cache_control);
    }

    if let Some(tool_choice) = &options.tool_choice {
        params.insert("tool_choice".to_string(), tool_choice.clone());
    }

    let thinking_token_budget_field = resolve_thinking_token_budget_field(compat);
    let thinking_budget =
        resolve_clamped_thinking_budget(model, options, &Value::Object(params.clone()));

    apply_thinking_format(model, options, compat, &mut params, thinking_budget);

    // Cap reasoning with a top-level budget field. Independent of
    // thinkingFormat: reasoning and the answer share max_tokens, so an
    // uncapped reasoning phase can consume the whole response.
    if let (Some(field), Some(budget)) = (thinking_token_budget_field, thinking_budget) {
        params.insert(field, serde_json::json!(budget));
    }

    // OpenRouter provider routing preferences.
    if let Some(routing) = &compat.open_router_routing {
        params.insert("provider".to_string(), routing.clone());
    }

    // Vercel AI Gateway provider routing preferences.
    if let Some(routing) = &compat.vercel_gateway_routing {
        if routing.get("only").is_some() || routing.get("order").is_some() {
            let mut gateway = Map::new();
            if let Some(only) = routing.get("only") {
                gateway.insert("only".to_string(), only.clone());
            }
            if let Some(order) = routing.get("order") {
                gateway.insert("order".to_string(), order.clone());
            }
            params.insert(
                "providerOptions".to_string(),
                serde_json::json!({ "gateway": gateway }),
            );
        }
    }

    // Last so custom keys override the named request fields.
    if let Some(sampling_params) = &options.sampling_params {
        for (key, value) in sampling_params {
            params.insert(key.clone(), value.clone());
        }
    }

    Value::Object(params)
}

fn apply_thinking_format(
    model: &Model,
    options: &OpenaiCompletionsOptions,
    compat: &ResolvedCompat,
    params: &mut Map<String, Value>,
    thinking_budget: Option<u64>,
) {
    let reasoning_effort = options.reasoning_effort;
    let effort_string = |level: Option<ThinkingLevel>| -> Option<String> {
        let level = level?;
        match model.thinking_level_map.as_ref().and_then(|map| {
            map.get(&to_model_thinking_level(level))
                .and_then(|mapped| mapped.clone())
        }) {
            Some(mapped) => Some(mapped),
            None => Some(level_to_string(level)),
        }
    };

    let off_mapped = || -> Option<Option<String>> {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&ModelThinkingLevel::Off))
            .cloned()
    };

    match compat.thinking_format {
        ThinkingFormat::Zai if model.reasoning => {
            let thinking = if reasoning_effort.is_some() {
                serde_json::json!({ "type": "enabled", "clear_thinking": false })
            } else {
                serde_json::json!({ "type": "disabled" })
            };
            params.insert("thinking".to_string(), thinking);
            if reasoning_effort.is_some() && compat.supports_reasoning_effort {
                if let Some(effort) = effort_string(reasoning_effort) {
                    params.insert("reasoning_effort".to_string(), Value::String(effort));
                }
            }
        }
        ThinkingFormat::Qwen if model.reasoning => {
            params.insert(
                "enable_thinking".to_string(),
                Value::Bool(reasoning_effort.is_some()),
            );
            if reasoning_effort.is_some() && compat.supports_reasoning_effort {
                if let Some(effort) = effort_string(reasoning_effort) {
                    params.insert("reasoning_effort".to_string(), Value::String(effort));
                }
            }
        }
        ThinkingFormat::QwenChatTemplate if model.reasoning => {
            params.insert(
                "chat_template_kwargs".to_string(),
                serde_json::json!({
                    "enable_thinking": reasoning_effort.is_some(),
                    "preserve_thinking": true
                }),
            );
        }
        ThinkingFormat::ChatTemplate if model.reasoning => {
            if let Some(kwargs) = build_chat_template_values(
                model,
                reasoning_effort,
                compat.chat_template_kwargs.clone(),
                thinking_budget,
            ) {
                params.insert("chat_template_kwargs".to_string(), Value::Object(kwargs));
            }
        }
        ThinkingFormat::Baseten if model.reasoning => {
            if let Some(args) = build_chat_template_values(
                model,
                reasoning_effort,
                compat.chat_template_args.clone(),
                thinking_budget,
            ) {
                params.insert("chat_template_args".to_string(), Value::Object(args));
            }
            if compat.supports_reasoning_effort {
                let requested_effort = reasoning_effort;
                let effort = if requested_effort.is_some() {
                    effort_string(requested_effort)
                } else {
                    off_mapped().flatten()
                };
                if let Some(effort) = effort {
                    params.insert("reasoning_effort".to_string(), Value::String(effort));
                }
            }
        }
        ThinkingFormat::Deepseek if model.reasoning => {
            if reasoning_effort.is_some() {
                params.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "enabled" }),
                );
            } else if off_mapped().map(|off| off.is_some()).unwrap_or(true) {
                params.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "disabled" }),
                );
            }
            if reasoning_effort.is_some() && compat.supports_reasoning_effort {
                if let Some(effort) = effort_string(reasoning_effort) {
                    params.insert("reasoning_effort".to_string(), Value::String(effort));
                }
            }
        }
        ThinkingFormat::Openrouter if model.reasoning => {
            if let Some(effort) = effort_string(reasoning_effort) {
                params.insert(
                    "reasoning".to_string(),
                    serde_json::json!({ "effort": effort }),
                );
            } else {
                let off = off_mapped().flatten().unwrap_or_else(|| "none".to_string());
                params.insert(
                    "reasoning".to_string(),
                    serde_json::json!({ "effort": off }),
                );
            }
        }
        ThinkingFormat::AntLing if model.reasoning && reasoning_effort.is_some() => {
            if let Some(effort) = effort_string(reasoning_effort) {
                params.insert(
                    "reasoning".to_string(),
                    serde_json::json!({ "effort": effort }),
                );
            }
        }
        ThinkingFormat::Together if model.reasoning => {
            params.insert(
                "reasoning".to_string(),
                serde_json::json!({ "enabled": reasoning_effort.is_some() }),
            );
            if reasoning_effort.is_some() && compat.supports_reasoning_effort {
                if let Some(effort) = effort_string(reasoning_effort) {
                    params.insert("reasoning_effort".to_string(), Value::String(effort));
                }
            }
        }
        ThinkingFormat::StringThinking if model.reasoning => {
            if let Some(effort) = effort_string(reasoning_effort) {
                params.insert("thinking".to_string(), Value::String(effort));
            } else {
                let off = off_mapped().flatten().unwrap_or_else(|| "none".to_string());
                params.insert("thinking".to_string(), Value::String(off));
            }
        }
        _ => {
            if let Some(effort) = reasoning_effort
                .filter(|_| model.reasoning && compat.supports_reasoning_effort)
                .and_then(|level| effort_string(Some(level)))
            {
                params.insert("reasoning_effort".to_string(), Value::String(effort));
            } else if reasoning_effort.is_none()
                && model.reasoning
                && compat.supports_reasoning_effort
            {
                if let Some(off) = off_mapped().flatten() {
                    params.insert("reasoning_effort".to_string(), Value::String(off));
                }
            }
        }
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

fn to_model_thinking_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn resolve_thinking_token_budget_field(compat: &ResolvedCompat) -> Option<String> {
    if let Some(field) = &compat.thinking_token_budget_field {
        return Some(field.clone());
    }
    if compat.supports_thinking_token_budget {
        return Some("thinking_token_budget".to_string());
    }
    None
}

fn resolve_clamped_thinking_budget(
    model: &Model,
    options: &OpenaiCompletionsOptions,
    params: &Value,
) -> Option<u64> {
    let reasoning_effort = options.reasoning_effort?;
    if !model.reasoning {
        return None;
    }
    let ceiling = params
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| params.get("max_completion_tokens").and_then(Value::as_u64))
        .unwrap_or(model.max_tokens);
    let budget = clamp_thinking_budget_to_answer_room(
        thinking_budget_for_level(reasoning_effort, options.thinking_budgets),
        ceiling,
    );
    (budget > 0).then_some(budget)
}

fn build_chat_template_values(
    model: &Model,
    reasoning_effort: Option<ThinkingLevel>,
    values: BTreeMap<String, Value>,
    thinking_budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let mut resolved_values = Map::new();
    for (key, value) in values {
        if let Some(resolved) =
            resolve_chat_template_kwarg_value(model, reasoning_effort, value, thinking_budget)
        {
            resolved_values.insert(key, resolved);
        }
    }
    (!resolved_values.is_empty()).then_some(resolved_values)
}

fn resolve_chat_template_kwarg_value(
    model: &Model,
    reasoning_effort: Option<ThinkingLevel>,
    value: Value,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let Some(map) = value.as_object() else {
        return Some(value);
    };

    if reasoning_effort.is_none()
        && map
            .get("omitWhenOff")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    match map.get("$var").and_then(Value::as_str) {
        Some("thinking.enabled") => return Some(Value::Bool(reasoning_effort.is_some())),
        Some("thinking.budget") => {
            return Some(match thinking_budget {
                Some(budget) => serde_json::json!(budget),
                None => Value::Null,
            });
        }
        _ => {}
    }

    let mapped_value = if reasoning_effort.is_some() {
        model.thinking_level_map.as_ref().and_then(|map| {
            reasoning_effort
                .and_then(|level| map.get(&to_model_thinking_level(level)))
                .cloned()
                .flatten()
        })
    } else {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&ModelThinkingLevel::Off))
            .cloned()
            .flatten()
    };
    match mapped_value {
        Some(mapped) => Some(Value::String(mapped)),
        None => reasoning_effort.map(|level| Value::String(level_to_string(level))),
    }
}

fn get_compat_cache_control(
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
) -> Option<Value> {
    if compat.cache_control_format.as_deref() != Some("anthropic")
        || cache_retention == CacheRetention::None
    {
        return None;
    }
    let ttl = (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then_some("1h");
    let mut cache_control = Map::new();
    cache_control.insert("type".to_string(), Value::String("ephemeral".to_string()));
    if let Some(ttl) = ttl {
        cache_control.insert("ttl".to_string(), Value::String(ttl.to_string()));
    }
    Some(Value::Object(cache_control))
}

fn apply_anthropic_cache_control(params: &mut Map<String, Value>, cache_control: Value) {
    // System prompt first.
    if let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "system" || role == "developer" {
                add_cache_control_to_text_content(message, &cache_control);
                break;
            }
        }
    }

    // Last tool definition.
    if let Some(tools) = params.get_mut("tools").and_then(Value::as_array_mut) {
        if let Some(last_tool) = tools.last_mut() {
            if let Some(tool_obj) = last_tool.as_object_mut() {
                tool_obj.insert("cache_control".to_string(), cache_control.clone());
            }
        }
    }

    // Last conversation message (user/assistant/tool).
    if let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().rev() {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            if matches!(role, "user" | "assistant" | "tool")
                && add_cache_control_to_text_content(message, &cache_control)
            {
                break;
            }
        }
    }
}

fn add_cache_control_to_text_content(message: &mut Value, cache_control: &Value) -> bool {
    let Some(message_obj) = message.as_object_mut() else {
        return false;
    };
    match message_obj.get_mut("content") {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return false;
            }
            let text = text.clone();
            message_obj.insert(
                "content".to_string(),
                serde_json::json!([{ "type": "text", "text": text, "cache_control": cache_control }]),
            );
            true
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(part_obj) = part.as_object_mut() {
                        part_obj.insert("cache_control".to_string(), cache_control.clone());
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

// --- Compat detection ----------------------------------------------------

/// Auto-detect compatibility settings from provider name and baseUrl (upstream
/// `detectCompat`). Explicit `model.compat` entries override detected values.
pub fn get_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = matches!(provider, "zai" | "zai-coding-cn")
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot =
        matches!(provider, "moonshotai" | "moonshotai-cn") || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");

    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_openrouter_developer_role_model =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format = (provider == "openrouter" && model.id.starts_with("anthropic/"))
        .then(|| "anthropic".to_string());

    let detected = ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: is_openrouter_developer_role_model
            || (!is_non_standard && !is_openrouter),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_openrouter {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        },
        chat_template_kwargs: BTreeMap::new(),
        chat_template_args: BTreeMap::new(),
        open_router_routing: None,
        vercel_gateway_routing: None,
        zai_tool_stream: false,
        supports_thinking_token_budget: false,
        thinking_token_budget_field: None,
        supports_strict_mode: !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia,
        supports_openai_grammar_tools: false,
        cache_control_format,
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: if is_openrouter {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        },
        supports_long_cache_retention: !(is_together
            || is_cloudflare_workers_ai
            || is_cloudflare_ai_gateway
            || is_nvidia
            || is_ant_ling),
    };

    let compat = match model.compat.as_ref() {
        Some(crate::types::ModelCompat::OpenaiCompletions(compat)) => compat,
        _ => return detected,
    };

    ResolvedCompat {
        supports_store: compat.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: compat
            .supports_developer_role
            .unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: compat
            .supports_reasoning_effort
            .unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: compat
            .supports_usage_in_streaming
            .unwrap_or(detected.supports_usage_in_streaming),
        supports_finish_reason: compat
            .supports_finish_reason
            .unwrap_or(detected.supports_finish_reason),
        max_tokens_field: match compat.max_tokens_field.as_deref() {
            Some("max_tokens") => MaxTokensField::MaxTokens,
            Some("max_completion_tokens") => MaxTokensField::MaxCompletionTokens,
            _ => detected.max_tokens_field,
        },
        requires_tool_result_name: compat
            .requires_tool_result_name
            .unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: compat
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: compat
            .requires_thinking_as_text
            .unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: compat
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: match compat.thinking_format.as_deref() {
            Some("openrouter") => ThinkingFormat::Openrouter,
            Some("deepseek") => ThinkingFormat::Deepseek,
            Some("together") => ThinkingFormat::Together,
            Some("baseten") => ThinkingFormat::Baseten,
            Some("zai") => ThinkingFormat::Zai,
            Some("qwen") => ThinkingFormat::Qwen,
            Some("chat-template") => ThinkingFormat::ChatTemplate,
            Some("qwen-chat-template") => ThinkingFormat::QwenChatTemplate,
            Some("string-thinking") => ThinkingFormat::StringThinking,
            Some("ant-ling") => ThinkingFormat::AntLing,
            _ => detected.thinking_format,
        },
        chat_template_kwargs: compat
            .chat_template_kwargs
            .clone()
            .unwrap_or(detected.chat_template_kwargs),
        chat_template_args: compat
            .chat_template_args
            .clone()
            .unwrap_or(detected.chat_template_args),
        open_router_routing: compat
            .open_router_routing
            .clone()
            .or(Some(Value::Object(Map::new()))),
        vercel_gateway_routing: compat
            .vercel_gateway_routing
            .clone()
            .or(detected.vercel_gateway_routing),
        zai_tool_stream: compat.zai_tool_stream.unwrap_or(detected.zai_tool_stream),
        supports_thinking_token_budget: compat
            .supports_thinking_token_budget
            .unwrap_or(detected.supports_thinking_token_budget),
        thinking_token_budget_field: compat
            .thinking_token_budget_field
            .clone()
            .or(detected.thinking_token_budget_field),
        supports_strict_mode: compat
            .supports_strict_mode
            .unwrap_or(detected.supports_strict_mode),
        supports_openai_grammar_tools: compat
            .supports_openai_grammar_tools
            .unwrap_or(detected.supports_openai_grammar_tools),
        cache_control_format: compat
            .cache_control_format
            .clone()
            .or(detected.cache_control_format),
        send_session_affinity_headers: compat
            .send_session_affinity_headers
            .unwrap_or(detected.send_session_affinity_headers),
        deferred_tools_mode: compat
            .deferred_tools_mode
            .clone()
            .or(detected.deferred_tools_mode),
        session_affinity_format: match compat.session_affinity_format.as_deref() {
            Some("openrouter") => SessionAffinityFormat::Openrouter,
            Some("openai-nosession") => SessionAffinityFormat::OpenaiNosession,
            Some("openai") => SessionAffinityFormat::Openai,
            _ => detected.session_affinity_format,
        },
        supports_long_cache_retention: compat
            .supports_long_cache_retention
            .unwrap_or(detected.supports_long_cache_retention),
    }
}

// --- streamSimple --------------------------------------------------------

pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if let Err(error) = get_client_api_key(
        &model.provider,
        options
            .as_ref()
            .and_then(|options| options.api_key.as_deref()),
        options
            .as_ref()
            .and_then(|options| options.headers.as_ref()),
    ) {
        // Upstream validates the key before streaming; surface the failure
        // through the stream like every other request failure.
        let stream = assistant_message_event_stream();
        let message = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: crate::api::zeroed_usage(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some(error.to_string()),
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

    let options = options.unwrap_or_default();
    // buildBaseOptions: clamp maxTokens to the remaining context window.
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = clamp_max_tokens_to_context(model.context_window, &context, base_max_tokens);

    // Merge model-level sampling params over per-request ones (per key).
    let mut sampling_params: Map<String, Value> = model
        .sampling_params
        .clone()
        .map(|params| params.into_iter().collect())
        .unwrap_or_default();
    for (key, value) in options.sampling_params.clone().unwrap_or_default() {
        sampling_params.insert(key, value);
    }
    let sampling_params = (!sampling_params.is_empty()).then_some(sampling_params);

    let clamped_reasoning = options
        .reasoning
        .map(|reasoning| clamp_thinking_level(&model, to_model_thinking_level(reasoning)));
    let reasoning_effort = match clamped_reasoning {
        Some(ModelThinkingLevel::Off) | None => None,
        Some(ModelThinkingLevel::Minimal) => Some(ThinkingLevel::Minimal),
        Some(ModelThinkingLevel::Low) => Some(ThinkingLevel::Low),
        Some(ModelThinkingLevel::Medium) => Some(ThinkingLevel::Medium),
        Some(ModelThinkingLevel::High) => Some(ThinkingLevel::High),
        Some(ModelThinkingLevel::Xhigh) => Some(ThinkingLevel::Xhigh),
        Some(ModelThinkingLevel::Max) => Some(ThinkingLevel::Max),
    };

    let completions_options = OpenaiCompletionsOptions {
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
        sampling_params,
        max_tokens: Some(max_tokens),
        cache_retention: options.cache_retention,
        session_id: options.session_id,
        metadata: options.metadata,
        tool_choice: options.tool_choice,
        reasoning_effort,
        thinking_budgets: options.thinking_budgets,
    };

    stream(model, context, Some(completions_options))
}

/// Wraps [`SseDataEvents`] to yield parsed JSON chunk values, stopping at
/// the `data: [DONE]` sentinel. Shared by the OpenAI-family adapters.
pub struct SseJsonEvents {
    inner: SseDataEvents,
    done: bool,
}

impl SseJsonEvents {
    pub fn new(inner: SseDataEvents) -> Self {
        Self { inner, done: false }
    }
}

impl futures::Stream for SseJsonEvents {
    type Item = Result<Value, AiError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.done {
            return std::task::Poll::Ready(None);
        }
        loop {
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(payload))) => {
                    if payload.trim() == "[DONE]" {
                        self.done = true;
                        return std::task::Poll::Ready(None);
                    }
                    if payload.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&payload) {
                        Ok(value) => return std::task::Poll::Ready(Some(Ok(value))),
                        Err(error) => {
                            return std::task::Poll::Ready(Some(Err(AiError::Other(format!(
                                "invalid SSE chunk JSON: {error}"
                            )))));
                        }
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    return std::task::Poll::Ready(Some(Err(error)));
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}
