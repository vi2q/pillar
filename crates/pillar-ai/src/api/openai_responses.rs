//! Port of packages/ai/src/api/openai-responses.ts (pi v0.84.3).
//!
//! divergence: the OpenAI SDK is replaced by direct JSON requests through
//! [`crate::transport::FetchFn`] with local SSE parsing, mirroring the
//! openai-completions adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::abort::AbortSignal;
use crate::api::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::openai_completions::SseJsonEvents;
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    ProcessResponsesStreamOptions, convert_responses_messages, convert_responses_tools,
    process_responses_stream,
};
use crate::api::{
    OnPayloadFn, OnResponseFn, ProviderResponseInfo, fetch_json_stream, get_pi_user_agent,
    merge_request_headers, resolve_cache_retention,
};
use crate::constrained_sampling::create_grammar_tool_input_properties;
use crate::deferred_tools::split_deferred_tools;
use crate::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use crate::models::clamp_thinking_level;
use crate::provider_retry::{ProviderRequestError, retry_provider_request};
use crate::simple_options::clamp_max_tokens_to_context;
use crate::types::{
    AssistantMessage, CacheRetention, Context, Model, ModelThinkingLevel, ProviderHeaders,
    StopReason, ThinkingLevel, Usage,
};

const OPENAI_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
/// OpenAI Responses rejects max_output_tokens below 16.
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

// --- Options -------------------------------------------------------------

/// Upstream `OpenAIResponsesOptions`.
#[derive(Default)]
pub struct OpenaiResponsesOptions {
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
    pub reasoning_effort: Option<ThinkingLevel>,
    /// "auto" | "detailed" | "concise" | null.
    pub reasoning_summary: Option<Option<String>>,
    /// "flex" | "priority" | "default" (passed through).
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
}

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
}

// --- Compat --------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ResolvedResponsesCompat {
    pub supports_developer_role: bool,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAffinityFormat {
    Openai,
    OpenaiNosession,
    Openrouter,
}

fn detect_session_affinity_format(model: &Model) -> SessionAffinityFormat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        SessionAffinityFormat::Openrouter
    } else {
        SessionAffinityFormat::Openai
    }
}

/// Resolve compat from `model.compat` with defaults (upstream `getCompat`;
/// all fields resolved, unlike the completions adapter's auto-detection).
pub fn get_compat(model: &Model) -> ResolvedResponsesCompat {
    let compat = model.compat.as_ref();
    let _ = compat; // compat typing is per-API; responses fields live on OpenaiResponsesCompat
    let responses_compat = responses_compat_of(model);
    ResolvedResponsesCompat {
        supports_developer_role: responses_compat.supports_developer_role.unwrap_or(true),
        session_affinity_format: match responses_compat.session_affinity_format.as_deref() {
            Some("openrouter") => SessionAffinityFormat::Openrouter,
            Some("openai-nosession") => SessionAffinityFormat::OpenaiNosession,
            Some("openai") => SessionAffinityFormat::Openai,
            _ => detect_session_affinity_format(model),
        },
        supports_long_cache_retention: responses_compat
            .supports_long_cache_retention
            .unwrap_or(true),
        supports_strict_mode: responses_compat.supports_strict_mode.unwrap_or(false),
        supports_openai_grammar_tools: responses_compat
            .supports_openai_grammar_tools
            .unwrap_or(false),
        supports_additional_tools: responses_compat.supports_additional_tools.unwrap_or(false),
        supports_tool_search: responses_compat.supports_tool_search.unwrap_or(false),
        supports_explicit_prompt_cache_mode: responses_compat
            .supports_explicit_prompt_cache_mode
            .unwrap_or(false),
    }
}

/// Read the responses-shaped compat fields off the model's compat JSON
/// (upstream: `model.compat as OpenAIResponsesCompat`).
fn responses_compat_of(model: &Model) -> crate::types::OpenaiResponsesCompat {
    model
        .compat
        .as_ref()
        .and_then(|compat| serde_json::to_value(compat).ok())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn get_prompt_cache_retention(
    compat: &ResolvedResponsesCompat,
    cache_retention: CacheRetention,
) -> Option<&'static str> {
    (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then_some("24h")
}

fn format_openai_responses_error(error: &ProviderRequestError) -> String {
    let norm =
        crate::error_body::normalize_provider_error(error.message.clone(), error.status, None);
    crate::error_body::format_provider_error(&norm, Some("OpenAI API error"))
}

// --- Stream --------------------------------------------------------------

pub fn stream(
    model: Model,
    context: Context,
    options: Option<OpenaiResponsesOptions>,
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

async fn run_stream(
    model: Model,
    context: Context,
    options: OpenaiResponsesOptions,
    stream: AssistantMessageEventStream,
) {
    let mut output = AssistantMessage {
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
    };

    let result = run_stream_inner(&model, &context, &options, &mut output, &stream).await;
    if let Err(error) = result {
        output.stop_reason = if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(format_openai_responses_error(&error));
        stream.push(crate::types::AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        });
        stream.end(Some(output));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn run_stream_inner(
    model: &Model,
    context: &Context,
    options: &OpenaiResponsesOptions,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
) -> Result<(), ProviderRequestError> {
    let api_key = get_client_api_key(
        &model.provider,
        options.api_key.as_deref(),
        options.headers.as_ref(),
    )
    .map_err(|error| ProviderRequestError::transport(error.to_string()))?;
    let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let cache_session_id = match cache_retention {
        CacheRetention::None => None,
        _ => options.session_id.clone(),
    };
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        Some(&context.tools),
        compat.supports_openai_grammar_tools,
    );
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        &grammar_tool_input_properties,
    );
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: format!("{}/responses", model.base_url.trim_end_matches('/')),
        headers: build_request_headers(
            model,
            context,
            options,
            &compat,
            &api_key,
            cache_session_id.as_deref(),
        ),
        body: Some(serde_json::to_vec(&params).map_err(|error| {
            ProviderRequestError::transport(format!("failed to serialize request body: {error}"))
        })?),
    };

    let fetch_for_retry = Arc::clone(&fetch);
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

    stream.push(crate::types::AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let model_for_stream = model.id.clone();
    let pricing = move |usage: &mut Usage, service_tier: Option<&str>| {
        apply_service_tier_pricing(usage, service_tier, &model_for_stream);
    };
    let sse = SseJsonEvents::new(crate::api::openai_completions::SseDataEvents::new(
        response.body,
    ));
    tokio::pin!(sse);
    process_responses_stream(
        &mut sse,
        output,
        stream,
        model,
        Some(ProcessResponsesStreamOptions {
            grammar_tool_input_properties: &grammar_tool_input_properties,
            apply_service_tier_pricing: Some(Box::new(pricing)),
        }),
    )
    .await
    .map_err(|error| ProviderRequestError::transport(error.to_string()))?;

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
    if output.stop_reason == StopReason::Pending {
        return Err(ProviderRequestError::transport(
            "OpenAI Responses stream ended without a stop reason".to_string(),
        ));
    }
    if output.stop_reason == StopReason::Aborted || output.stop_reason == StopReason::Error {
        return Err(ProviderRequestError::transport(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_string()),
        ));
    }

    stream.push(crate::types::AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

fn default_fetch() -> crate::transport::SharedFetchFn {
    Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, crate::error::AiError> {
    if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        return Ok(api_key.to_string());
    }
    let has_auth_header = headers.is_some_and(|headers| {
        headers.iter().any(|(key, value)| {
            (key.to_lowercase() == "authorization" || key.to_lowercase() == "cf-aig-authorization")
                && value.as_ref().is_some_and(|value| !value.trim().is_empty())
        })
    });
    if has_auth_header {
        return Ok("unused".to_string());
    }
    Err(crate::error::AiError::Other(format!(
        "No API key for provider: {provider}"
    )))
}

fn build_request_headers(
    model: &Model,
    context: &Context,
    options: &OpenaiResponsesOptions,
    compat: &ResolvedResponsesCompat,
    api_key: &str,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut defaults = vec![
        ("User-Agent".to_string(), get_pi_user_agent()),
        ("Authorization".to_string(), format!("Bearer {api_key}")),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if model.provider == "github-copilot" {
        let has_images = has_copilot_vision_input(&context.messages);
        defaults.extend(build_copilot_dynamic_headers(&context.messages, has_images));
    }
    if let Some(session_id) = session_id {
        match compat.session_affinity_format {
            SessionAffinityFormat::Openrouter => {
                defaults.push(("x-session-id".to_string(), session_id.to_string()));
            }
            format @ (SessionAffinityFormat::Openai | SessionAffinityFormat::OpenaiNosession) => {
                if format == SessionAffinityFormat::Openai {
                    defaults.push(("session_id".to_string(), session_id.to_string()));
                }
                defaults.push(("x-client-request-id".to_string(), session_id.to_string()));
            }
        }
    }
    merge_request_headers(defaults, model.headers.as_ref(), options.headers.as_ref())
}

// --- Params --------------------------------------------------------------

pub fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenaiResponsesOptions,
    compat: &ResolvedResponsesCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Value {
    let deferred_tools_mode = if compat.supports_additional_tools {
        Some("additional-tools")
    } else if compat.supports_tool_search {
        Some("tool-search")
    } else {
        None
    };
    let tool_placement = split_deferred_tools(context, deferred_tools_mode.is_some());
    let allowed_providers: std::collections::BTreeSet<String> = OPENAI_TOOL_CALL_PROVIDERS
        .iter()
        .map(|provider| provider.to_string())
        .collect();
    let messages = convert_responses_messages(
        model,
        context,
        &allowed_providers,
        Some(
            crate::api::openai_responses_shared::ConvertResponsesMessagesOptions {
                grammar_tool_input_properties: Some(grammar_tool_input_properties),
                deferred_tools: Some(&tool_placement.deferred),
                deferred_tools_mode,
                tool_options: Some(
                    crate::api::openai_responses_shared::ConvertResponsesToolsOptions {
                        supports_strict_mode: Some(compat.supports_strict_mode),
                        supports_openai_grammar_tools: Some(compat.supports_openai_grammar_tools),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        ),
    );

    let cache_retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let disable_implicit_prompt_cache =
        cache_retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode;

    let mut params = Map::new();
    params.insert("model".to_string(), Value::String(model.id.clone()));
    params.insert("input".to_string(), Value::Array(messages));
    params.insert("stream".to_string(), Value::Bool(true));
    if cache_retention != CacheRetention::None {
        if let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
            params.insert("prompt_cache_key".to_string(), Value::String(key));
        }
    }
    if let Some(retention) = get_prompt_cache_retention(compat, cache_retention) {
        params.insert(
            "prompt_cache_retention".to_string(),
            Value::String(retention.to_string()),
        );
    }
    if disable_implicit_prompt_cache {
        params.insert(
            "prompt_cache_options".to_string(),
            json!({ "mode": "explicit" }),
        );
    }
    params.insert("store".to_string(), Value::Bool(false));

    if let Some(max_tokens) = options.max_tokens {
        params.insert(
            "max_output_tokens".to_string(),
            json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS)),
        );
    }

    if let Some(temperature) = options.temperature {
        params.insert("temperature".to_string(), json!(temperature));
    }

    if let Some(service_tier) = &options.service_tier {
        params.insert(
            "service_tier".to_string(),
            Value::String(service_tier.clone()),
        );
    }

    if !tool_placement.immediate.is_empty() {
        let converted = convert_responses_tools(
            &tool_placement.immediate,
            Some(
                &crate::api::openai_responses_shared::ConvertResponsesToolsOptions {
                    supports_strict_mode: Some(compat.supports_strict_mode),
                    supports_openai_grammar_tools: Some(compat.supports_openai_grammar_tools),
                    ..Default::default()
                },
            ),
        );
        params.insert("tools".to_string(), Value::Array(converted));
    }

    if let Some(tool_choice) = &options.tool_choice {
        params.insert("tool_choice".to_string(), tool_choice.clone());
    }

    if model.reasoning {
        let effort_from_level = |level: Option<ThinkingLevel>| -> Option<String> {
            let level = level?;
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&to_model_thinking_level(level)).cloned().flatten())
                .or_else(|| Some(level_to_string(level)))
        };
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = match options.reasoning_effort {
                Some(level) => {
                    effort_from_level(Some(level)).unwrap_or_else(|| level_to_string(level))
                }
                None => "medium".to_string(),
            };
            let summary = options
                .reasoning_summary
                .clone()
                .flatten()
                .unwrap_or_else(|| "auto".to_string());
            params.insert(
                "reasoning".to_string(),
                json!({ "effort": effort, "summary": summary }),
            );
            params.insert(
                "include".to_string(),
                json!(["reasoning.encrypted_content"]),
            );
        } else if model.provider != "github-copilot" {
            let off = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ModelThinkingLevel::Off).cloned().flatten());
            // thinkingLevelMap.off === null marks "off" as unsupported; only
            // send the explicit effort when the mapping is not null.
            let off_is_null = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ModelThinkingLevel::Off))
                .map(|value| value.is_none())
                .unwrap_or(false);
            if !off_is_null {
                let effort = off.unwrap_or_else(|| "none".to_string());
                params.insert("reasoning".to_string(), json!({ "effort": effort }));
            }
        }
        if model.provider == "xai" {
            params.insert(
                "include".to_string(),
                json!(["reasoning.encrypted_content"]),
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

// --- Service tier pricing -------------------------------------------------

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
        let stream = assistant_message_event_stream();
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
            error_message: Some(error.to_string()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        };
        stream.push(crate::types::AssistantMessageEvent::Error {
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

    let responses_options = OpenaiResponsesOptions {
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
        sampling_params: options.sampling_params,
        max_tokens: Some(max_tokens),
        cache_retention: options.cache_retention,
        session_id: options.session_id,
        metadata: options.metadata,
        tool_choice: options.tool_choice,
        reasoning_effort,
        reasoning_summary: None,
        service_tier: None,
    };

    stream(model, context, Some(responses_options))
}
