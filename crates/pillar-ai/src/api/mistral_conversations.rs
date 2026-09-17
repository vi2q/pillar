//! Port of packages/ai/src/api/mistral-conversations.ts (pi v0.84.3).
//!
//! Native Mistral Chat Completions streaming adapter.
//!
//! divergence: upstream uses the mistral SDK client; the Rust port performs
//! requests via `FetchFn` and parses the SSE stream itself. The wire payload
//! conversion (`toMistralWirePayload` camelCase→snake_case remapping) is
//! reproduced exactly, including response_format json_schema handling.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::api::impl_from_request_options;
use crate::api::{OnPayloadFn, OnResponseFn, get_user_agent, merge_request_headers};
use crate::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::event_stream::assistant_message_event_stream;
use crate::hash::short_hash;
use crate::json_parse::parse_streaming_json;
use crate::models::clamp_thinking_level;
use crate::text::sanitize_surrogates;
use crate::transform_messages::transform_messages;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, Content, Context, Model,
    ModelThinkingLevel, ProviderHeaders, StopReason, ThinkingBudgets, ThinkingLevel, Tool, Usage,
};

fn default_fetch() -> crate::transport::SharedFetchFn {
    std::sync::Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

fn block_index(output: &AssistantMessage) -> usize {
    output.content.len().saturating_sub(1)
}

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;

// --- Options -----------------------------------------------------------------

/// Upstream `MistralOptions`.
#[derive(Default)]
pub struct MistralOptions {
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
    /// "auto" | "none" | "any" | "required" | {"type":"function","function":{"name":...}}
    pub tool_choice: Option<Value>,
    pub prompt_mode: Option<String>,
    /// "none" | "high"
    pub reasoning_effort: Option<String>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
}

impl_from_request_options!(MistralOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for mistral-conversations.
#[derive(Default)]
pub struct SimpleStreamOptions {
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
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
}

// --- Stream entry ---------------------------------------------------------------

/// Upstream `stream()` — returns an event stream; the request runs on a
/// spawned task against the default or injected transport.
pub fn stream(
    model: Model,
    context: Context,
    options: Option<MistralOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
        usage: crate::api::zeroed_usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

fn fail_stream(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    error: String,
    aborted: bool,
) {
    output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(error);
    stream.push(AssistantMessageEvent::Error {
        reason: output.stop_reason,
        error: output.clone(),
    });
    stream.end(Some(output.clone()));
}

async fn run_stream(
    model: Model,
    context: Context,
    options: MistralOptions,
    stream: crate::event_stream::AssistantMessageEventStream,
) {
    let mut output = fresh_output(&model);

    let result = run_stream_inner(&model, &context, &options, &mut output, &stream).await;
    if let Err(error) = result {
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        fail_stream(&mut output, &stream, error, aborted);
    }
}

type RunResult = Result<(), String>;

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &MistralOptions,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) -> RunResult {
    let Some(api_key) = options.api_key.as_deref().filter(|k| !k.is_empty()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };

    let normalize_id = create_mistral_tool_call_id_normalizer();
    let transformed = transform_messages(context.messages.clone(), model, Some(&normalize_id));

    let mut payload = build_chat_payload(model, context, &transformed, options);
    if let Some(on_payload) = &options.on_payload {
        if let Some(next) = on_payload(model, payload.clone()).await {
            payload = next;
        }
    }

    let url = format!(
        "{}/v1/chat/completions",
        model.base_url.trim_end_matches('/')
    );
    let headers = build_mistral_headers(model, api_key, options);

    let wire = to_mistral_wire_payload(&payload);
    let body = serde_json::to_vec(&wire)
        .map_err(|error| format!("failed to serialize request body: {error}"))?;

    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url,
        headers,
        body: Some(body),
    };

    // Upstream combines the caller signal with AbortSignal.timeout (60s
    // default) — both the fetch and the body read abort with it.
    let timeout_ms = Some(options.timeout_ms.unwrap_or(60_000));

    let response =
        crate::api::fetch_json_stream(&fetch, request, options.signal.as_ref(), timeout_ms)
            .await
            .map_err(|error| format_mistral_error(&error))?;

    if let Some(on_response) = &options.on_response {
        on_response(
            crate::api::ProviderResponseInfo {
                status: response.status,
                headers: response.headers.clone(),
            },
            model,
        )
        .await;
    }

    stream.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let events = read_mistral_events(response, options.signal.as_ref(), timeout_ms).await?;

    consume_chat_stream(model, output, stream, &events)?;

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err("Request was aborted".to_string());
    }
    if output.stop_reason == StopReason::Pending {
        return Err("Mistral stream ended without a finish reason".to_string());
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        return Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string()));
    }

    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

// --- Tool call ID normalization ------------------------------------------------

/// Upstream `createMistralToolCallIdNormalizer`: deterministic bijection
/// between arbitrary IDs and 9-char alphanumeric Mistral IDs.
pub fn create_mistral_tool_call_id_normalizer() -> impl Fn(&str, &Model, &AssistantMessage) -> String
{
    let id_map: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let reverse_map: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    move |id: &str, _model: &Model, _source: &AssistantMessage| -> String {
        let mut id_map = id_map.lock().expect("id map lock");
        if let Some(existing) = id_map.get(id) {
            return existing.clone();
        }
        let mut reverse = reverse_map.lock().expect("reverse map lock");
        let mut attempt = 0u64;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            match reverse.get(&candidate) {
                Some(owner) if owner != id => {
                    attempt += 1;
                }
                _ => {
                    id_map.insert(id.to_string(), candidate.clone());
                    reverse.insert(candidate.clone(), id.to_string());
                    return candidate;
                }
            }
        }
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: u64) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.chars().count() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id.to_string()
    } else {
        normalized
    };
    let seed = if attempt == 0 {
        seed_base
    } else {
        format!("{}:{}", seed_base, attempt)
    };
    let hashed: String = short_hash(&seed)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    hashed.chars().take(MISTRAL_TOOL_CALL_ID_LENGTH).collect()
}

// --- Error formatting -------------------------------------------------------------

fn truncate_error_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    let dropped = text.chars().count() - max_chars;
    format!("{kept}... [truncated {dropped} chars]")
}

fn format_mistral_error(error: &crate::provider_retry::ProviderRequestError) -> String {
    match error.status {
        Some(status) => {
            let body = error.message.trim();
            if body.is_empty() || body == format!("Request failed with status code {status}") {
                format!("Mistral API error ({status}): {}", error.message)
            } else {
                format!(
                    "Mistral API error ({}): {}",
                    status,
                    truncate_error_text(body, MAX_MISTRAL_ERROR_BODY_CHARS)
                )
            }
        }
        None => error.message.clone(),
    }
}

// --- Headers -------------------------------------------------------------------------

fn build_mistral_headers(
    model: &Model,
    api_key: &str,
    options: &MistralOptions,
) -> Vec<(String, String)> {
    let mut defaults = vec![
        ("User-Agent".to_string(), get_user_agent()),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let has_explicit_affinity = has_header_override(model.headers.as_ref(), "x-affinity")
        || has_header_override(options.headers.as_ref(), "x-affinity");
    if should_use_prompt_caching(options) && !has_explicit_affinity {
        if let Some(session_id) = &options.session_id {
            defaults.push(("x-affinity".to_string(), session_id.clone()));
        }
    }

    merge_request_headers(defaults, model.headers.as_ref(), options.headers.as_ref())
}

fn has_header_override(headers: Option<&ProviderHeaders>, target: &str) -> bool {
    headers
        .map(|headers| headers.keys().any(|name| name.eq_ignore_ascii_case(target)))
        .unwrap_or(false)
}

fn should_use_prompt_caching(options: &MistralOptions) -> bool {
    options.cache_retention != Some(CacheRetention::None)
        && options
            .session_id
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
}

// --- Wire payload -----------------------------------------------------------------

/// Upstream `toMistralWirePayload`: camelCase→snake_case remapping, message
/// conversion, and response_format json_schema handling.
pub fn to_mistral_wire_payload(payload: &Value) -> Value {
    let mut wire = payload.clone();
    let obj = wire.as_object_mut().expect("payload is an object");

    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap_property(obj, source, target);
    }

    if let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned() {
        let wire_messages: Vec<Value> = messages.iter().map(to_mistral_wire_message).collect();
        obj.insert("messages".to_string(), Value::Array(wire_messages));
    }

    if let Some(response_format) = obj.get_mut("response_format") {
        if let Some(rf_obj) = response_format.as_object_mut() {
            remap_property(rf_obj, "jsonSchema", "json_schema");
            if let Some(json_schema) = rf_obj.get_mut("json_schema") {
                if let Some(js_obj) = json_schema.as_object_mut() {
                    remap_property(js_obj, "schemaDefinition", "schema");
                }
            }
        }
    }

    wire
}

fn to_mistral_wire_message(message: &Value) -> Value {
    let mut wire = message.clone();
    if let Some(obj) = wire.as_object_mut() {
        remap_property(obj, "toolCalls", "tool_calls");
        remap_property(obj, "toolCallId", "tool_call_id");
        if let Some(content) = obj.get("content") {
            if content.is_array() {
                let chunks: Vec<Value> = content
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(to_mistral_wire_content_chunk)
                    .collect();
                obj.insert("content".to_string(), Value::Array(chunks));
            }
        }
    }
    wire
}

fn to_mistral_wire_content_chunk(chunk: &Value) -> Value {
    let mut wire = chunk.clone();
    if let Some(obj) = wire.as_object_mut() {
        for (source, target) in [
            ("imageUrl", "image_url"),
            ("documentUrl", "document_url"),
            ("documentName", "document_name"),
            ("fileId", "file_id"),
            ("referenceIds", "reference_ids"),
            ("inputAudio", "input_audio"),
        ] {
            remap_property(obj, source, target);
        }
    }
    wire
}

fn remap_property(record: &mut Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = record.remove(source) {
        record.insert(target.to_string(), value);
    }
}

// --- Stream event parsing -----------------------------------------------------------

/// Upstream `readMistralEvents` + `parseMistralEvent`: split the body into
/// SSE events on mixed CRLF/LF boundaries, take `data:` lines, stop at
/// `[DONE]`.
async fn read_mistral_events(
    response: crate::transport::FetchResponse,
    signal: Option<&crate::AbortSignal>,
    timeout_ms: Option<u64>,
) -> Result<Vec<Value>, String> {
    use futures::StreamExt;

    let mut byte_stream = response.body;
    let mut buffer: Vec<u8> = Vec::new();
    let mut events: Vec<Value> = Vec::new();
    let mut done = false;

    loop {
        let chunk = {
            let read_fut = byte_stream.next();
            tokio::pin!(read_fut);
            match read_guarded(&mut read_fut, signal, timeout_ms).await {
                Ok(chunk) => chunk,
                Err(error) => return Err(error),
            }
        };
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk);

        loop {
            let Some((index, length)) = find_mistral_event_boundary(&buffer) else {
                break;
            };
            let raw = String::from_utf8_lossy(&buffer[..index]).to_string();
            buffer.drain(..index + length);
            match parse_mistral_event(&raw)? {
                MistralEvent::Done => {
                    done = true;
                    break;
                }
                MistralEvent::Data(value) => events.push(value),
                MistralEvent::None => {}
            }
        }
        if done {
            break;
        }
    }

    if !done {
        // Tail: parse any remaining buffered event.
        let raw = String::from_utf8_lossy(&buffer).to_string();
        if !raw.trim().is_empty() {
            match parse_mistral_event(&raw)? {
                MistralEvent::Done => {}
                MistralEvent::Data(value) => events.push(value),
                MistralEvent::None => {}
            }
        }
    }

    Ok(events)
}

/// Read one body chunk with abort + timeout guards (upstream passes the
/// combined signal to fetch, so the body read aborts with it).
/// divergence: the timeout here is per-chunk (resets on each received
/// chunk) rather than whole-request.
async fn read_guarded(
    read_fut: &mut (
             impl std::future::Future<Output = Option<Result<Vec<u8>, crate::error::AiError>>> + Unpin
         ),
    signal: Option<&crate::AbortSignal>,
    timeout_ms: Option<u64>,
) -> Result<Option<Vec<u8>>, String> {
    let abort_fut = async {
        match signal {
            Some(s) => {
                let _ = s.aborted_or_pending().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(abort_fut);

    let raced = async {
        tokio::select! {
            biased;
            _ = &mut abort_fut => Err("Request was aborted".to_string()),
            chunk = read_fut => match chunk {
                Some(Ok(c)) => Ok(Some(c)),
                Some(Err(e)) => Err(e.to_string()),
                None => Ok(None),
            },
        }
    };
    tokio::pin!(raced);

    match timeout_ms {
        Some(ms) => {
            match crate::clock::timeout(std::time::Duration::from_millis(ms), &mut raced).await {
                Ok(result) => result,
                Err(_) => Err("Mistral request timed out".to_string()),
            }
        }
        None => raced.await,
    }
}

/// Find the next SSE boundary (mixed \r\n / \r / \n pairs), returning
/// (index, length) of the delimiter.
fn find_mistral_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    // Byte-level search: the buffer may end mid-UTF8-sequence, so scanning
    // via String::from_utf8_lossy can shift indices (the replacement char is
    // 3 bytes vs the original 1-4). The delimiters are all ASCII, so byte
    // search is safe and exact.
    let mut best: Option<(usize, usize)> = None;
    for sep in [
        b"\r\n\r\n".as_slice(),
        b"\r\n\r".as_slice(),
        b"\r\n\n".as_slice(),
        b"\r\r\n".as_slice(),
        b"\n\r\n".as_slice(),
        b"\r\r".as_slice(),
        b"\n\r".as_slice(),
        b"\n\n".as_slice(),
    ] {
        if let Some(i) = buffer.windows(sep.len()).position(|w| w == sep) {
            match best {
                Some((bi, _)) if bi <= i => {}
                _ => best = Some((i, sep.len())),
            }
        }
    }
    best
}

enum MistralEvent {
    Done,
    Data(Value),
    None,
}

fn parse_mistral_event(raw: &str) -> Result<MistralEvent, String> {
    let data = raw
        .split(['\r', '\n'])
        .filter(|line| line.starts_with("data:"))
        .map(|line| line[5..].trim_start())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if data.is_empty() {
        return Ok(MistralEvent::None);
    }
    if data == "[DONE]" {
        return Ok(MistralEvent::Done);
    }
    let parsed: Value = serde_json::from_str(&data)
        .map_err(|error| format!("Invalid Mistral streaming event: {error}"))?;
    if !parsed.is_object() || parsed.get("choices").and_then(Value::as_array).is_none() {
        return Err("Invalid Mistral streaming event".to_string());
    }
    Ok(MistralEvent::Data(parsed))
}

// --- Payload building ------------------------------------------------------------------

fn build_chat_payload(
    model: &Model,
    context: &Context,
    messages: &[crate::types::Message],
    options: &MistralOptions,
) -> Value {
    let supports_images = model.input.iter().any(|i| i == "image");
    let mut payload = Map::new();
    payload.insert("model".to_string(), json!(model.id));
    payload.insert("stream".to_string(), json!(true));
    payload.insert(
        "messages".to_string(),
        Value::Array(to_chat_messages(messages, supports_images)),
    );

    if !context.tools.is_empty() {
        payload.insert(
            "tools".to_string(),
            json!(to_function_tools(&context.tools)),
        );
    }
    if let Some(temperature) = options.temperature {
        payload.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        payload.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(tool_choice) = &options.tool_choice {
        payload.insert("toolChoice".to_string(), tool_choice.clone());
    }
    if let Some(prompt_mode) = &options.prompt_mode {
        payload.insert("promptMode".to_string(), json!(prompt_mode));
    }
    if let Some(effort) = &options.reasoning_effort {
        payload.insert("reasoningEffort".to_string(), json!(effort));
    }
    if should_use_prompt_caching(options) {
        if let Some(session_id) = &options.session_id {
            payload.insert("promptCacheKey".to_string(), json!(session_id));
        }
    }

    if let Some(system_prompt) = &context.system_prompt {
        if !system_prompt.is_empty() {
            let mut messages_arr: Vec<Value> = payload
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            messages_arr.insert(
                0,
                json!({ "role": "system", "content": sanitize_surrogates(system_prompt) }),
            );
            payload.insert("messages".to_string(), Value::Array(messages_arr));
        }
    }

    Value::Object(payload)
}

fn to_function_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, true)
                .ok()
                .flatten();
            let parameters = get_json_schema_tool_parameters(tool, strict)
                .unwrap_or_else(|_| tool.parameters.clone());
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters,
                    "strict": strict.unwrap_or(false),
                },
            })
        })
        .collect()
}

fn to_chat_messages(messages: &[crate::types::Message], supports_images: bool) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();

    for msg in messages {
        match msg {
            crate::types::Message::User { content, .. } => match content {
                crate::types::UserContent::Text(text) => {
                    result.push(json!({
                        "role": "user",
                        "content": sanitize_surrogates(text),
                    }));
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let had_images = blocks.iter().any(|b| matches!(b, Content::Image { .. }));
                    let mut chunks: Vec<Value> = Vec::new();
                    for item in blocks {
                        match item {
                            Content::Text { text, .. } => {
                                chunks.push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(text),
                                }));
                            }
                            Content::Image {
                                data, mime_type, ..
                            } => {
                                if supports_images {
                                    chunks.push(json!({
                                        "type": "image_url",
                                        "imageUrl": format!("data:{};base64,{}", mime_type, data),
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    if !chunks.is_empty() {
                        result.push(json!({ "role": "user", "content": chunks }));
                    } else if had_images && !supports_images {
                        result.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                }
            },
            crate::types::Message::Assistant(assistant) => {
                let mut content_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();

                for block in &assistant.content {
                    match block {
                        Content::Text { text, .. } => {
                            if !text.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(text),
                                }));
                            }
                        }
                        Content::Thinking { thinking, .. } => {
                            if !thinking.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "thinking",
                                    "thinking": [{ "type": "text", "text": sanitize_surrogates(thinking) }],
                                }));
                            }
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()),
                                },
                                "index": 0,
                            }));
                        }
                        _ => {}
                    }
                }

                let mut assistant_msg = Map::new();
                assistant_msg.insert("role".to_string(), json!("assistant"));
                assistant_msg.insert("prefix".to_string(), json!(false));
                if !content_parts.is_empty() {
                    assistant_msg.insert("content".to_string(), Value::Array(content_parts));
                }
                if !tool_calls.is_empty() {
                    assistant_msg.insert("toolCalls".to_string(), Value::Array(tool_calls));
                }
                if assistant_msg.contains_key("content") || assistant_msg.contains_key("toolCalls")
                {
                    result.push(Value::Object(assistant_msg));
                }
            }
            crate::types::Message::ToolResult(tool_result) => {
                let text_result = tool_result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::Image { .. }));

                let mut tool_content: Vec<Value> = Vec::new();
                let tool_text = build_tool_result_text(
                    &text_result,
                    has_images,
                    supports_images,
                    tool_result.is_error,
                );
                tool_content.push(json!({ "type": "text", "text": tool_text }));
                if supports_images {
                    for part in &tool_result.content {
                        if let Content::Image { data, mime_type } = part {
                            tool_content.push(json!({
                                "type": "image_url",
                                "imageUrl": format!("data:{};base64,{}", mime_type, data),
                            }));
                        }
                    }
                }
                result.push(json!({
                    "role": "tool",
                    "toolCallId": tool_result.tool_call_id,
                    "name": tool_result.tool_name,
                    "content": tool_content,
                }));
            }
        }
    }

    result
}

fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };

    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }

    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".to_string()
            } else {
                "(see attached image)".to_string()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".to_string()
        } else {
            "(image omitted: model does not support images)".to_string()
        };
    }

    if is_error {
        "[tool error] (no tool output)".to_string()
    } else {
        "(no tool output)".to_string()
    }
}

// --- Stream consumption ------------------------------------------------------------------

/// Upstream `consumeChatStream`: fold completion chunks into blocks/events.
fn consume_chat_stream(
    model: &Model,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    events: &[Value],
) -> RunResult {
    let mut current_kind: Option<BlockKind> = None;
    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let mut tool_blocks: Vec<ToolBlockState> = Vec::new();

    for chunk in events {
        // Keep the first non-empty id (upstream `output.responseId ||= chunk.id`).
        if output.response_id.is_none() {
            if let Some(id) = chunk
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                output.response_id = Some(id.to_string());
            }
        }

        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = get_mistral_cached_prompt_tokens(usage, prompt_tokens);
            let completion_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(prompt_tokens.saturating_sub(cached) + completion_tokens);
            output.usage = Usage {
                input: prompt_tokens.saturating_sub(cached),
                output: completion_tokens,
                cache_read: cached,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens,
                cost: Default::default(),
            };
            crate::models::calculate_cost(model, &mut output.usage);
        }

        let choice = match chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            Some(choice) => choice,
            None => continue,
        };

        // Upstream: `if (choice.finish_reason)` — null/absent is falsy and
        // does NOT set the stop reason (the stream may continue).
        if let Some(finish_reason) = choice.get("finish_reason") {
            if !finish_reason.is_null() {
                if let Some(reason) = finish_reason.as_str() {
                    output.raw_stop_reason = Some(reason.to_string());
                    let mapped = map_chat_stop_reason(Some(reason));
                    output.stop_reason = mapped.0;
                    if let Some(err) = mapped.1 {
                        output.error_message = Some(err);
                    }
                }
            }
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };

        // Content: string | chunk array | null.
        if let Some(content) = delta.get("content") {
            if !content.is_null() {
                let items: Vec<Value> = if let Some(text) = content.as_str() {
                    vec![json!(text)]
                } else {
                    content.as_array().cloned().unwrap_or_default()
                };
                for item in &items {
                    if let Some(text_delta) = item.as_str() {
                        let text_delta = sanitize_surrogates(text_delta);
                        if current_kind != Some(BlockKind::Text) {
                            finish_current_block(
                                output,
                                stream,
                                current_kind,
                                &current_text,
                                &current_thinking,
                            );
                            current_kind = Some(BlockKind::Text);
                            current_text = String::new();
                            current_thinking = String::new();
                            output.content.push(Content::Text {
                                text: String::new(),
                                text_signature: None,
                            });
                            stream.push(AssistantMessageEvent::TextStart {
                                content_index: block_index(output),
                                partial: output.clone(),
                            });
                        }
                        current_text.push_str(&text_delta);
                        stream.push(AssistantMessageEvent::TextDelta {
                            content_index: block_index(output),
                            delta: text_delta,
                            partial: output.clone(),
                        });
                        continue;
                    }

                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    if item_type == "thinking" {
                        let delta_text = item
                            .get("thinking")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                                    .filter(|t| !t.is_empty())
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        let thinking_delta = sanitize_surrogates(&delta_text);
                        if thinking_delta.is_empty() {
                            continue;
                        }
                        if current_kind != Some(BlockKind::Thinking) {
                            finish_current_block(
                                output,
                                stream,
                                current_kind,
                                &current_text,
                                &current_thinking,
                            );
                            current_kind = Some(BlockKind::Thinking);
                            current_text = String::new();
                            current_thinking = String::new();
                            output.content.push(Content::Thinking {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: None,
                            });
                            stream.push(AssistantMessageEvent::ThinkingStart {
                                content_index: block_index(output),
                                partial: output.clone(),
                            });
                        }
                        current_thinking.push_str(&thinking_delta);
                        stream.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: block_index(output),
                            delta: thinking_delta,
                            partial: output.clone(),
                        });
                        continue;
                    }

                    if item_type == "text" {
                        let text_delta = sanitize_surrogates(
                            item.get("text").and_then(Value::as_str).unwrap_or(""),
                        );
                        if current_kind != Some(BlockKind::Text) {
                            finish_current_block(
                                output,
                                stream,
                                current_kind,
                                &current_text,
                                &current_thinking,
                            );
                            current_kind = Some(BlockKind::Text);
                            current_text = String::new();
                            current_thinking = String::new();
                            output.content.push(Content::Text {
                                text: String::new(),
                                text_signature: None,
                            });
                            stream.push(AssistantMessageEvent::TextStart {
                                content_index: block_index(output),
                                partial: output.clone(),
                            });
                        }
                        current_text.push_str(&text_delta);
                        stream.push(AssistantMessageEvent::TextDelta {
                            content_index: block_index(output),
                            delta: text_delta,
                            partial: output.clone(),
                        });
                    }
                }
            }
        }

        // Tool calls.
        let tool_calls = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for tool_call in &tool_calls {
            if current_kind.is_some() {
                finish_current_block(
                    output,
                    stream,
                    current_kind,
                    &current_text,
                    &current_thinking,
                );
                current_kind = None;
                current_text = String::new();
                current_thinking = String::new();
            }

            let tc_id = tool_call.get("id").and_then(Value::as_str);
            let call_id = match tc_id {
                Some(id) if id != "null" && !id.is_empty() => id.to_string(),
                _ => {
                    let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    derive_mistral_tool_call_id(&format!("toolcall:{}", index), 0)
                }
            };
            let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let key = format!("{}:{}", call_id, index);

            let existing_pos = tool_blocks.iter().position(|b| b.key == key);
            let pos = match existing_pos {
                Some(pos) => pos,
                None => {
                    let name = tool_call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    output.content.push(Content::ToolCall {
                        id: call_id.clone(),
                        name,
                        arguments: json!({}),
                        thought_signature: None,
                        namespace: None,
                    });
                    tool_blocks.push(ToolBlockState {
                        key: key.clone(),
                        content_index: output.content.len() - 1,
                        partial_args: String::new(),
                    });
                    stream.push(AssistantMessageEvent::ToolcallStart {
                        content_index: output.content.len() - 1,
                        partial: output.clone(),
                    });
                    tool_blocks.len() - 1
                }
            };

            let args_delta = match tool_call.get("function").and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
                None => String::new(),
            };

            let block = &mut tool_blocks[pos];
            block.partial_args.push_str(&args_delta);
            let parsed = parse_streaming_json(Some(&block.partial_args));
            if let Some(Content::ToolCall { arguments, .. }) =
                output.content.get_mut(block.content_index)
            {
                *arguments = parsed;
            }
            stream.push(AssistantMessageEvent::ToolcallDelta {
                content_index: block.content_index,
                delta: args_delta,
                partial: output.clone(),
            });
        }
    }

    finish_current_block(
        output,
        stream,
        current_kind,
        &current_text,
        &current_thinking,
    );

    // Finalize tool blocks (upstream re-parses and emits toolcall_end).
    for block in &tool_blocks {
        let parsed = parse_streaming_json(Some(&block.partial_args));
        if let Some(Content::ToolCall { arguments, .. }) =
            output.content.get_mut(block.content_index)
        {
            *arguments = parsed;
        }
        let tool_call = output.content[block.content_index].clone();
        stream.push(AssistantMessageEvent::ToolcallEnd {
            content_index: block.content_index,
            tool_call,
            partial: output.clone(),
        });
    }

    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
}

struct ToolBlockState {
    key: String,
    content_index: usize,
    partial_args: String,
}

fn finish_current_block(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    kind: Option<BlockKind>,
    current_text: &str,
    current_thinking: &str,
) {
    // Upstream mutates the block objects in place; the port accumulates into
    // local buffers, so flush the accumulated text back into the output block
    // before emitting the end event.
    match kind {
        Some(BlockKind::Text) => {
            if let Some(Content::Text { text, .. }) = output.content.last_mut() {
                *text = current_text.to_string();
            }
            stream.push(AssistantMessageEvent::TextEnd {
                content_index: block_index(output),
                content: current_text.to_string(),
                partial: output.clone(),
            });
        }
        Some(BlockKind::Thinking) => {
            if let Some(Content::Thinking { thinking, .. }) = output.content.last_mut() {
                *thinking = current_thinking.to_string();
            }
            stream.push(AssistantMessageEvent::ThinkingEnd {
                content_index: block_index(output),
                content: current_thinking.to_string(),
                partial: output.clone(),
            });
        }
        None => {}
    }
}

/// Upstream `getMistralCachedPromptTokens`: probe every known shape of the
/// cached-token field, clamp to [0, promptTokens].
fn get_mistral_cached_prompt_tokens(usage: &Value, prompt_tokens: u64) -> u64 {
    let candidates = [
        usage.pointer("/promptTokensDetails/cachedTokens"),
        usage.pointer("/prompt_tokens_details/cached_tokens"),
        usage.pointer("/promptTokenDetails/cachedTokens"),
        usage.pointer("/prompt_token_details/cached_tokens"),
        usage.get("numCachedTokens"),
        usage.get("num_cached_tokens"),
    ];
    let raw = candidates
        .into_iter()
        .flatten()
        .find_map(|v| v.as_f64())
        .filter(|v| v.is_finite());
    let cached = raw.unwrap_or(0.0).max(0.0) as u64;
    cached.min(prompt_tokens)
}

/// Upstream `mapChatStopReason`.
fn map_chat_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        None => (StopReason::Stop, None),
        Some("stop") => (StopReason::Stop, None),
        Some("length") | Some("model_length") => (StopReason::Length, None),
        Some("tool_calls") => (StopReason::ToolUse, None),
        Some("error") => (
            StopReason::Error,
            Some("Provider stopped with: error".to_string()),
        ),
        Some(other) => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

// --- reasoning mode helpers -------------------------------------------------------------

/// Upstream `usesReasoningEffort`: models that take reasoning_effort.
pub fn uses_reasoning_effort(model: &Model) -> bool {
    matches!(
        model.id.as_str(),
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

/// Upstream `usesPromptModeReasoning`.
pub fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

/// Upstream `mapReasoningEffort`: model map value, defaulting to "high".
pub fn map_reasoning_effort(model: &Model, level: ThinkingLevel) -> String {
    let key = match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    };
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&key))
        .and_then(|v| v.clone())
        .unwrap_or_else(|| "high".to_string())
}

/// Upstream `streamSimple()` for mistral-conversations.
pub fn stream_simple_mistral(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let options = options.unwrap_or_default();

    if options
        .api_key
        .as_deref()
        .map(|k| k.is_empty())
        .unwrap_or(true)
    {
        let stream = assistant_message_event_stream();
        let mut message = fresh_output(&model);
        message.stop_reason = StopReason::Error;
        message.error_message = Some(format!("No API key for provider: {}", model.provider));
        stream.push(AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: message.clone(),
        });
        stream.end(Some(message));
        return stream;
    }

    // buildBaseOptions: clamp maxTokens to the remaining context window.
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = crate::simple_options::clamp_max_tokens_to_context(
        model.context_window,
        &context,
        base_max_tokens,
    );

    let should_use_reasoning = model.reasoning && options.reasoning.is_some();
    let clamped_reasoning = options
        .reasoning
        .map(|r| clamp_thinking_level(&model, mistral_to_model_level(r)));
    let reasoning = match clamped_reasoning {
        Some(ModelThinkingLevel::Off) | None => None,
        Some(level) => Some(to_thinking_level(level)),
    };

    let mistral_options = MistralOptions {
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
        tool_choice: options.tool_choice,
        prompt_mode: if should_use_reasoning && uses_prompt_mode_reasoning(&model) {
            Some("reasoning".to_string())
        } else {
            None
        },
        reasoning_effort: if should_use_reasoning && uses_reasoning_effort(&model) {
            reasoning.map(|level| map_reasoning_effort(&model, level))
        } else {
            None
        },
        cache_retention: options.cache_retention,
        session_id: options.session_id,
    };

    stream(model, context, Some(mistral_options))
}

fn to_thinking_level(level: ModelThinkingLevel) -> ThinkingLevel {
    match level {
        ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        ModelThinkingLevel::Low => ThinkingLevel::Low,
        ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        ModelThinkingLevel::High => ThinkingLevel::High,
        ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        ModelThinkingLevel::Max => ThinkingLevel::Max,
        ModelThinkingLevel::Off => ThinkingLevel::Low,
    }
}

fn mistral_to_model_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}
