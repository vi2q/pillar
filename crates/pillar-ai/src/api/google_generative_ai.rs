//! Port of packages/ai/src/api/google-generative-ai.ts (pi v0.84.3).
//!
//! Google Generative AI (Gemini API) adapter.
//!
//! divergence: upstream talks to the @google/genai SDK client; the Rust port
//! performs requests via `FetchFn` against `{baseUrl}/models/{model}:stream
//! GenerateContent?alt=sse` and decodes SSE directly. Wire format follows
//! @google/genai 1.52.0 (mldev converters): body is `{contents, generation
//! Config}` with `systemInstruction`/`tools`/`toolConfig` lifted to the top
//! level; auth via the `x-goog-api-key` header.

use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Map, Value, json};

use crate::api::google_shared::{
    GeminiPart, GeminiStreamChunk, convert_messages, convert_tools, is_thinking_part,
    map_stop_reason_string, resolve_google_function_calling_mode, resolve_google_thinking_level,
    retain_thought_signature, supports_google_strict_tool_sampling,
};
use crate::api::impl_from_request_options;
use crate::event_stream::assistant_message_event_stream;
use crate::text::sanitize_surrogates;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, Model, StopReason, Usage,
};

// --- Options ---------------------------------------------------------------

/// Upstream `GoogleOptions` (stream options for google-generative-ai).
#[derive(Default)]
pub struct GoogleOptions {
    pub signal: Option<crate::AbortSignal>,
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
    pub tool_choice: Option<String>,
    /// Upstream `thinking?: { enabled, budgetTokens?, level? }`.
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<i64>,
    pub thinking_level: Option<String>,
}

impl_from_request_options!(GoogleOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for google-generative-ai.
#[derive(Default)]
pub struct SimpleStreamOptions {
    pub signal: Option<crate::AbortSignal>,
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
    pub tool_choice: Option<String>,
    pub reasoning: Option<crate::types::ThinkingLevel>,
    pub thinking_budgets: Option<crate::types::ThinkingBudgets>,
}

/// Counter for generating unique tool call IDs (upstream module-level
/// `toolCallCounter`).
static TOOL_CALL_COUNTER: AtomicU32 = AtomicU32::new(0);

// --- Stream entry ------------------------------------------------------------

fn default_fetch() -> crate::transport::SharedFetchFn {
    std::sync::Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

/// Upstream `stream()`: builds the mldev request, then delegates to the
/// shared stream loop. Returns an event stream; the request runs on a
/// spawned task against the default or injected transport.
pub fn stream(
    model: Model,
    context: Context,
    options: Option<GoogleOptions>,
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
        usage: Usage::default(),
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
    options: GoogleOptions,
    stream: crate::event_stream::AssistantMessageEventStream,
) {
    let mut output = fresh_output(&model);

    let result = run_stream_task(&model, &context, &options, &mut output, &stream).await;
    if let Err(error) = result {
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        fail_stream(&mut output, &stream, error, aborted);
    }
}

/// Full mldev request lifecycle: build params, apply onPayload, build the
/// HTTP request, then delegate to the shared stream loop.
async fn run_stream_task(
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) -> RunResult {
    // Upstream validates the API key before creating the client.
    let Some(api_key) = options.api_key.as_deref().filter(|k| !k.is_empty()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };

    let mut params = build_params(model, context, options)?;
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let url = format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        model.base_url.trim_end_matches('/'),
        model.id
    );

    let headers = {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        headers.push(("x-goog-api-key".to_string(), api_key.to_string()));
        crate::api::merge_request_headers(headers, model.headers.as_ref(), options.headers.as_ref())
    };

    let body = serde_json::to_vec(&params)
        .map_err(|error| format!("failed to serialize request body: {error}"))?;
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url,
        headers,
        body: Some(body),
    };

    stream_with_request(model, context, options, request, output, stream).await
}

type RunResult = Result<(), String>;

/// Shared stream loop: takes a pre-built request, executes it with retry,
/// decodes SSE chunks, and processes them into the output. Used by both
/// google-generative-ai and google-vertex.
pub async fn stream_with_request(
    model: &Model,
    _context: &Context,
    options: &GoogleOptions,
    request: crate::transport::FetchRequest,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
) -> RunResult {
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let fetch_for_retry = std::sync::Arc::clone(&fetch);
    let request_for_retry = request;
    let signal = options.signal.clone();
    let timeout_ms = options.timeout_ms;
    let response = crate::provider_retry::retry_provider_request(
        || {
            let fetch = std::sync::Arc::clone(&fetch_for_retry);
            let request = request_for_retry.clone();
            let signal = signal.clone();
            async move {
                crate::api::fetch_json_stream(&fetch, request, signal.as_ref(), timeout_ms).await
            }
        },
        crate::provider_retry::ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            signal: options.signal.clone(),
        },
    )
    .await
    .map_err(|error| crate::api::format_stream_error(&error))?;

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

    let sse_events = decode_fetch_response(response, options.signal.as_ref()).await?;

    // Process each SSE data line as a GenerateContentResponse chunk.
    let mut current_block: Option<CurrentBlock> = None;
    for sse in &sse_events {
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data.clone());
        }
        let Ok(chunk) = serde_json::from_str::<GeminiStreamChunk>(&sse.data) else {
            continue;
        };
        process_chunk(chunk, model, output, stream, &mut current_block)?;
    }

    // Flush the trailing open block.
    flush_current_block(output, stream, &mut current_block);

    if options
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err("Request was aborted".to_string());
    }
    if output.stop_reason == StopReason::Pending {
        return Err("Google stream ended without a finish reason".to_string());
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        let error_message = match &output.raw_stop_reason {
            Some(raw) => format!("Provider stopped with: {raw}"),
            None => "An unknown error occurred".to_string(),
        };
        return Err(error_message);
    }

    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

/// Open streaming block scratch state (upstream `currentBlock`).
enum CurrentBlock {
    Text {
        text: String,
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        thinking_signature: Option<String>,
    },
}

impl CurrentBlock {
    fn is_thinking(&self) -> bool {
        matches!(self, CurrentBlock::Thinking { .. })
    }
}

fn block_index(output: &AssistantMessage) -> usize {
    output.content.len().saturating_sub(1)
}

fn push_block_start(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    is_thinking: bool,
) {
    if is_thinking {
        output.content.push(Content::Thinking {
            thinking: String::new(),
            thinking_signature: None,
            redacted: None,
        });
        stream.push(AssistantMessageEvent::ThinkingStart {
            content_index: block_index(output),
            partial: output.clone(),
        });
    } else {
        output.content.push(Content::Text {
            text: String::new(),
            text_signature: None,
        });
        stream.push(AssistantMessageEvent::TextStart {
            content_index: block_index(output),
            partial: output.clone(),
        });
    }
}

fn flush_current_block(
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    current_block: &mut Option<CurrentBlock>,
) {
    if let Some(block) = current_block.take() {
        match block {
            CurrentBlock::Text { text, .. } => {
                stream.push(AssistantMessageEvent::TextEnd {
                    content_index: block_index(output),
                    content: text,
                    partial: output.clone(),
                });
            }
            CurrentBlock::Thinking { thinking, .. } => {
                stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: block_index(output),
                    content: thinking,
                    partial: output.clone(),
                });
            }
        }
    }
}

fn process_chunk(
    chunk: GeminiStreamChunk,
    model: &Model,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    current_block: &mut Option<CurrentBlock>,
) -> RunResult {
    // Keep the first non-empty responseId from the stream.
    if output.response_id.is_none() {
        if let Some(response_id) = chunk.response_id.as_deref().filter(|id| !id.is_empty()) {
            output.response_id = Some(response_id.to_string());
        }
    }

    let candidate = chunk.candidates.as_ref().and_then(|c| c.first());

    if let Some(parts) = candidate.and_then(|c| c.content.as_ref()).map(|c| &c.parts) {
        for part in parts {
            process_part(part, model, output, stream, current_block)?;
        }
    }

    if let Some(finish_reason) = candidate.and_then(|c| c.finish_reason.as_deref()) {
        output.raw_stop_reason = Some(finish_reason.to_string());
        output.stop_reason = map_stop_reason_string(finish_reason);
        let has_tool_call = output
            .content
            .iter()
            .any(|b| matches!(b, Content::ToolCall { .. }));
        if has_tool_call && output.stop_reason == StopReason::Stop {
            output.stop_reason = StopReason::ToolUse;
        }
    }

    if let Some(usage_metadata) = chunk.usage_metadata {
        output.usage = Usage {
            input: usage_metadata.prompt_token_count - usage_metadata.cached_content_token_count,
            output: usage_metadata.candidates_token_count + usage_metadata.thoughts_token_count,
            cache_read: usage_metadata.cached_content_token_count,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: Some(usage_metadata.thoughts_token_count),
            total_tokens: usage_metadata.total_token_count,
            cost: Default::default(),
        };
        crate::models::calculate_cost(model, &mut output.usage);
    }

    Ok(())
}

fn process_part(
    part: &GeminiPart,
    model: &Model,
    output: &mut AssistantMessage,
    stream: &crate::event_stream::AssistantMessageEventStream,
    current_block: &mut Option<CurrentBlock>,
) -> RunResult {
    let _ = model;
    if let Some(text) = &part.text {
        let is_thinking = is_thinking_part(part);
        let needs_new_block = match current_block {
            None => true,
            Some(block) => {
                (is_thinking && !block.is_thinking()) || (!is_thinking && block.is_thinking())
            }
        };
        if needs_new_block {
            flush_current_block(output, stream, current_block);
            push_block_start(output, stream, is_thinking);
            *current_block = Some(if is_thinking {
                CurrentBlock::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                }
            } else {
                CurrentBlock::Text {
                    text: String::new(),
                    text_signature: None,
                }
            });
        }

        match current_block.as_mut().unwrap() {
            CurrentBlock::Thinking {
                thinking,
                thinking_signature,
            } => {
                thinking.push_str(text);
                *thinking_signature = retain_thought_signature(
                    thinking_signature.as_deref(),
                    part.thought_signature.as_deref(),
                );
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: block_index(output),
                    delta: text.clone(),
                    partial: output.clone(),
                });
            }
            CurrentBlock::Text {
                text: block_text,
                text_signature,
            } => {
                block_text.push_str(text);
                *text_signature = retain_thought_signature(
                    text_signature.as_deref(),
                    part.thought_signature.as_deref(),
                );
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: block_index(output),
                    delta: text.clone(),
                    partial: output.clone(),
                });
            }
        }
    }

    if let Some(function_call) = &part.function_call {
        flush_current_block(output, stream, current_block);

        // Generate unique ID if not provided or if it's a duplicate.
        let provided_id = function_call.id.as_deref();
        let needs_new_id = match provided_id {
            None => true,
            Some(id) => output.content.iter().any(|b| match b {
                Content::ToolCall { id: existing, .. } => existing == id,
                _ => false,
            }),
        };
        let tool_call_id = if needs_new_id {
            let counter = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            format!("{}_{}_{}", function_call.name, now_ms(), counter)
        } else {
            provided_id.unwrap().to_string()
        };

        let arguments = if function_call.args.is_null() {
            json!({})
        } else {
            function_call.args.clone()
        };
        let tool_call = Content::ToolCall {
            id: tool_call_id,
            name: if function_call.name.is_empty() {
                String::new()
            } else {
                function_call.name.clone()
            },
            arguments,
            thought_signature: part.thought_signature.clone(),
            namespace: None,
        };

        let Content::ToolCall { arguments, .. } = &tool_call else {
            unreachable!("just constructed a tool call");
        };
        let arguments_json = serde_json::to_string(arguments)
            .map_err(|error| format!("failed to serialize tool call arguments: {error}"))?;

        output.content.push(tool_call.clone());
        stream.push(AssistantMessageEvent::ToolcallStart {
            content_index: block_index(output),
            partial: output.clone(),
        });
        stream.push(AssistantMessageEvent::ToolcallDelta {
            content_index: block_index(output),
            delta: arguments_json,
            partial: output.clone(),
        });
        stream.push(AssistantMessageEvent::ToolcallEnd {
            content_index: block_index(output),
            tool_call,
            partial: output.clone(),
        });
    }

    Ok(())
}

// --- SSE decoding --------------------------------------------------------------

/// Reads the fetch response body and decodes it into raw SSE events
/// (upstream `iterateSseMessages` over the SDK's streaming response).
async fn decode_fetch_response(
    response: crate::transport::FetchResponse,
    signal: Option<&crate::AbortSignal>,
) -> Result<Vec<crate::api::anthropic_messages::ServerSentEvent>, String> {
    use futures::StreamExt;

    let mut state = crate::api::anthropic_messages::SseDecoderState::default();
    let mut buffer = String::new();
    let mut events: Vec<crate::api::anthropic_messages::ServerSentEvent> = Vec::new();
    let byte_stream = response.body;
    tokio::pin!(byte_stream);

    loop {
        // Race the abort: a quiet body must not delay the cancellation
        // (upstream the SDK's `abortSignal` cancelling the request).
        let next = match crate::api::race_abort(signal, byte_stream.next()).await {
            Ok(next) => next,
            Err(_) => return Err("Request was aborted".to_string()),
        };
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => return Err(error.to_string()),
            None => break,
        };
        let text = String::from_utf8_lossy(&chunk);
        events.extend(crate::api::anthropic_messages::decode_sse_chunk(
            &text,
            &mut state,
            &mut buffer,
        ));
    }
    events.extend(crate::api::anthropic_messages::finish_sse_body(
        &mut state,
        &mut buffer,
    ));
    Ok(events)
}

// --- Params building --------------------------------------------------------------

/// Upstream `buildParams`: assemble GenerateContentParameters (mldev wire
/// format: contents + generationConfig; systemInstruction/tools/toolConfig
/// lifted to the top level by the SDK converters).
pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
) -> Result<Value, String> {
    let contents = convert_messages(model, context);
    let contents_json: Vec<Value> = contents
        .iter()
        .map(|content| serde_json::to_value(content).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;

    // generationConfig: temperature + maxOutputTokens.
    let mut generation_config = Map::new();
    if let Some(temperature) = options.temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }

    let supports_strict_mode = supports_google_strict_tool_sampling(&model.id);
    let function_calling_mode = if !context.tools.is_empty() {
        resolve_google_function_calling_mode(
            &context.tools,
            options.tool_choice.as_deref(),
            supports_strict_mode,
        )
        .map_err(|e| e.to_string())?
    } else {
        None
    };

    let mut params = Map::new();
    params.insert("model".to_string(), json!(model.id));
    params.insert("contents".to_string(), Value::Array(contents_json));

    if !generation_config.is_empty() {
        params.insert(
            "generationConfig".to_string(),
            Value::Object(generation_config),
        );
    }

    if let Some(system_prompt) = &context.system_prompt {
        if !system_prompt.is_empty() {
            params.insert(
                "systemInstruction".to_string(),
                json!({
                    "role": "user",
                    "parts": [{ "text": sanitize_surrogates(system_prompt) }],
                }),
            );
        }
    }

    if !context.tools.is_empty() {
        let tools = convert_tools(&context.tools, false, supports_strict_mode)
            .map_err(|e| e.to_string())?;
        if let Some(tools) = tools {
            params.insert("tools".to_string(), tools);
        }
    }

    if let Some(mode) = function_calling_mode {
        params.insert(
            "toolConfig".to_string(),
            json!({
                "functionCallingConfig": { "mode": mode.as_str() },
            }),
        );
    }

    // thinkingConfig.
    if options.thinking_enabled.unwrap_or(false) && model.reasoning {
        let mut thinking_config = Map::new();
        thinking_config.insert("includeThoughts".to_string(), json!(true));
        if let Some(level) = &options.thinking_level {
            thinking_config.insert("thinkingLevel".to_string(), json!(level));
        } else if let Some(budget) = options.thinking_budget_tokens {
            thinking_config.insert("thinkingBudget".to_string(), json!(budget));
        }
        params.insert("thinkingConfig".to_string(), Value::Object(thinking_config));
    } else if model.reasoning && options.thinking_enabled == Some(false) {
        let disabled = get_disabled_thinking_config(&model.id);
        params.insert("thinkingConfig".to_string(), disabled);
    }

    Ok(Value::Object(params))
}

// --- Model detection helpers -------------------------------------------------------

pub fn is_gemma4_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    // /gemma-?4/
    lower.contains("gemma-4") || lower.contains("gemma4")
}

/// `/gemini-3(?:\.\d+)?-pro/` against the lowercased id.
fn matches_gemini_3_variant(model_id: &str, variant: &str) -> bool {
    let Some(rest) = model_id.strip_prefix("gemini-3") else {
        return false;
    };
    // Optional ".\d+" minor.
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = &rest[digits.len()..];
    rest.starts_with(variant)
}

pub fn is_gemini_3_pro_model(model_id: &str) -> bool {
    matches_gemini_3_variant(&model_id.to_lowercase(), "-pro")
}

pub fn is_gemini_3_flash_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    if id == "gemini-flash-latest" || id == "gemini-flash-lite-latest" {
        return true;
    }
    // /gemini-3(?:\.\d+)?-flash/
    matches_gemini_3_variant(&id, "-flash")
}

/// Upstream `getDisabledThinkingConfig` (google-generative-ai variant).
fn get_disabled_thinking_config(model_id: &str) -> Value {
    if is_gemini_3_pro_model(model_id) {
        return json!({ "thinkingLevel": "LOW" });
    }
    if is_gemini_3_flash_model(model_id) || is_gemma4_model(model_id) {
        return json!({ "thinkingLevel": "MINIMAL" });
    }
    // Gemini 2.x supports disabling via thinkingBudget = 0.
    json!({ "thinkingBudget": 0 })
}

/// Upstream `getThinkingLevel` (google-generative-ai variant).
pub fn get_thinking_level(
    effort: crate::api::google_shared::ResolvedGoogleThinkingLevel,
    model_id: &str,
) -> &'static str {
    use crate::api::google_shared::ResolvedGoogleThinkingLevel as L;
    if is_gemini_3_pro_model(model_id) {
        return match effort {
            L::Minimal | L::Low => "LOW",
            L::Medium | L::High => "HIGH",
        };
    }
    if is_gemma4_model(model_id) {
        return match effort {
            L::Minimal | L::Low => "MINIMAL",
            L::Medium | L::High => "HIGH",
        };
    }
    match effort {
        L::Minimal => "MINIMAL",
        L::Low => "LOW",
        L::Medium => "MEDIUM",
        L::High => "HIGH",
    }
}

/// Upstream `getGoogleBudget` (google-generative-ai variant).
pub fn get_google_budget(
    model_id: &str,
    level: crate::api::google_shared::ResolvedGoogleThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> i64 {
    use crate::api::google_shared::ResolvedGoogleThinkingLevel as L;
    if let Some(budgets) = custom_budgets {
        let value = match level {
            L::Minimal => budgets.minimal,
            L::Low => budgets.low,
            L::Medium => budgets.medium,
            L::High => budgets.high,
        };
        if let Some(value) = value {
            return value as i64;
        }
    }

    if model_id.contains("2.5-pro") {
        return match level {
            L::Minimal => 128,
            L::Low => 2048,
            L::Medium => 8192,
            L::High => 32768,
        };
    }
    if model_id.contains("2.5-flash-lite") {
        return match level {
            L::Minimal => 512,
            L::Low => 2048,
            L::Medium => 8192,
            L::High => 24576,
        };
    }
    if model_id.contains("2.5-flash") {
        return match level {
            L::Minimal => 128,
            L::Low => 2048,
            L::Medium => 8192,
            L::High => 24576,
        };
    }
    -1
}

// --- streamSimple -------------------------------------------------------------------

/// Upstream `streamSimple()`: maps reasoning levels onto the full options
/// shape, then delegates to [`stream`].
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let options = options.unwrap_or_default();

    // Upstream validates the API key synchronously before streaming.
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

    let mut google_options = GoogleOptions {
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
        thinking_enabled: Some(false),
        thinking_budget_tokens: None,
        thinking_level: None,
    };

    if let Some(reasoning) = options.reasoning {
        let clamped =
            crate::models::clamp_thinking_level(&model, to_model_thinking_level(reasoning));
        let resolved_level = resolve_google_thinking_level(&model, clamped)
            .unwrap_or(crate::api::google_shared::ResolvedGoogleThinkingLevel::High);
        if is_gemini_3_pro_model(&model.id)
            || is_gemini_3_flash_model(&model.id)
            || is_gemma4_model(&model.id)
        {
            google_options.thinking_enabled = Some(true);
            google_options.thinking_level =
                Some(get_thinking_level(resolved_level, &model.id).to_string());
        } else {
            google_options.thinking_enabled = Some(true);
            google_options.thinking_budget_tokens = Some(get_google_budget(
                &model.id,
                resolved_level,
                options.thinking_budgets.as_ref(),
            ));
        }
    }

    stream(model, context, Some(google_options))
}

pub(crate) fn to_model_thinking_level(
    level: crate::types::ThinkingLevel,
) -> crate::types::ModelThinkingLevel {
    match level {
        crate::types::ThinkingLevel::Minimal => crate::types::ModelThinkingLevel::Minimal,
        crate::types::ThinkingLevel::Low => crate::types::ModelThinkingLevel::Low,
        crate::types::ThinkingLevel::Medium => crate::types::ModelThinkingLevel::Medium,
        crate::types::ThinkingLevel::High => crate::types::ModelThinkingLevel::High,
        crate::types::ThinkingLevel::Xhigh => crate::types::ModelThinkingLevel::Xhigh,
        crate::types::ThinkingLevel::Max => crate::types::ModelThinkingLevel::Max,
    }
}
