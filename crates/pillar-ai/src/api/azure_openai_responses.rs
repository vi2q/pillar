//! Port of packages/ai/src/api/azure-openai-responses.ts (pi v0.84.3).
//!
//! Azure OpenAI Responses API. Uses the shared Responses message conversion
//! and stream processor with an Azure-specific base URL / deployment /
//! api-version resolution layer.
//!
//! divergence: upstream goes through the `openai` npm SDK's `AzureOpenAI`
//! client; the port builds requests directly (base URL + `/responses`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::AbortSignal;
use crate::api::fetch_json_stream;
use crate::api::impl_from_request_options;
use crate::api::openai_completions::SseDataEvents;
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, convert_responses_messages,
    convert_responses_tools, process_responses_stream,
};
use crate::api::{
    OnPayloadFn, OnResponseFn, ProviderHeaders, ProviderResponseInfo, get_user_agent,
    merge_request_headers,
};
use crate::assistant_message_event_stream;
use crate::constrained_sampling::create_grammar_tool_input_properties;
use crate::event_stream::AssistantMessageEventStream;
use crate::models::clamp_thinking_level;
use crate::provider_env::get_provider_env_value;
use crate::provider_retry::ProviderRequestError;
use crate::provider_retry::retry_provider_request;
use crate::types::AssistantMessageEvent;
use crate::types::{
    AssistantMessage, Context, Model, ModelThinkingLevel, StopReason, ThinkingLevel, Usage,
};
use crate::{error_body, simple_options};

const DEFAULT_AZURE_API_VERSION: &str = "v1";
// OpenAI Responses rejects max_output_tokens below 16.
const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;
const AZURE_TOOL_CALL_PROVIDERS: [&str; 4] = [
    "openai",
    "openai-codex",
    "opencode",
    "azure-openai-responses",
];

// --- Options --------------------------------------------------------------

/// Azure OpenAI Responses-specific options.
#[derive(Default)]
pub struct AzureOpenAIResponsesOptions {
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
    pub session_id: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning_effort: Option<ThinkingLevel>,
    /// "auto" | "detailed" | "concise" | null.
    pub reasoning_summary: Option<Option<String>>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

impl_from_request_options!(AzureOpenAIResponsesOptions);
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
    pub session_id: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning: Option<ThinkingLevel>,
}

// --- Deployment / URL resolution -------------------------------------------

fn parse_deployment_name_map(value: Option<&str>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(value) = value else {
        return map;
    };
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, '=');
        let (Some(model_id), Some(deployment_name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (model_id, deployment_name) = (model_id.trim(), deployment_name.trim());
        if model_id.is_empty() || deployment_name.is_empty() {
            continue;
        }
        map.insert(model_id.to_string(), deployment_name.to_string());
    }
    map
}

pub fn resolve_deployment_name(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> String {
    if let Some(name) = options.and_then(|options| options.azure_deployment_name.as_deref()) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    let mapped = options
        .and_then(|options| options.env.as_ref())
        .and_then(|env| get_provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", Some(env)))
        .and_then(|value| {
            parse_deployment_name_map(Some(&value))
                .get(&model.id)
                .cloned()
        });
    mapped.unwrap_or_else(|| model.id.clone())
}

fn is_azure_host(host: &str) -> bool {
    host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com")
}

/// Upstream `normalizeAzureBaseUrl`: Azure hosts always get `/openai/v1` as
/// the base path (the SDK appends `/deployments/<model>` + `?api-version=`).
/// Test-visible wrapper around the internal normalizer.
pub fn normalize_azure_base_url_pub(base_url: &str) -> Result<String, String> {
    normalize_azure_base_url(base_url)
}

pub(crate) fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("Invalid Azure OpenAI base URL: {base_url}"));
    }
    let (authority, path_and_query) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("Invalid Azure OpenAI base URL: {base_url}"));
    }
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };

    let normalized_path = path.trim_matches('/');
    if is_azure_host(authority.to_ascii_lowercase().rsplit_once(':').map_or(
        authority,
        |(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
                host
            } else {
                authority
            }
        },
    )) && (normalized_path.is_empty()
        || normalized_path == "openai"
        || normalized_path == "openai/v1/responses")
    {
        // Azure hosts always get /openai/v1; query params are dropped.
        return Ok(format!("{scheme}://{authority}/openai/v1"));
    }

    let mut url = format!("{scheme}://{authority}{path}");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    Ok(url.trim_end_matches('/').to_string())
}

fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

pub(crate) struct ResolvedAzureConfig {
    pub base_url: String,
    pub api_version: String,
}

pub(crate) fn resolve_azure_config(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> Result<ResolvedAzureConfig, String> {
    let env = options.and_then(|options| options.env.as_ref());
    let api_version: String = options
        .and_then(|options| options.azure_api_version.as_deref())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| get_provider_env_value("AZURE_OPENAI_API_VERSION", env))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());

    let base_url: Option<String> = options
        .and_then(|options| options.azure_base_url.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            get_provider_env_value("AZURE_OPENAI_BASE_URL", env)
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty());
    let resource_name: Option<String> = options
        .and_then(|options| options.azure_resource_name.as_deref())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| get_provider_env_value("AZURE_OPENAI_RESOURCE_NAME", env));

    let resolved_base_url = base_url
        .or_else(|| resource_name.as_deref().map(build_default_base_url))
        .or_else(|| {
            if model.base_url.is_empty() {
                None
            } else {
                Some(model.base_url.clone())
            }
        })
        .ok_or_else(|| {
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.".to_string()
        })?;

    Ok(ResolvedAzureConfig {
        base_url: normalize_azure_base_url(&resolved_base_url)?,
        api_version,
    })
}

fn format_azure_openai_error(error: &ProviderRequestError) -> String {
    let norm = error_body::normalize_provider_error(error.message.clone(), error.status, None);
    error_body::format_provider_error(&norm, Some("Azure OpenAI API error"))
}

// --- Params ----------------------------------------------------------------

pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    options: &AzureOpenAIResponsesOptions,
    deployment_name: &str,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Value {
    let allowed_providers: std::collections::BTreeSet<String> = AZURE_TOOL_CALL_PROVIDERS
        .iter()
        .map(|provider| provider.to_string())
        .collect();
    let messages = convert_responses_messages(
        model,
        context,
        &allowed_providers,
        Some(ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            ..Default::default()
        }),
    );

    let mut params = Map::new();
    params.insert(
        "model".to_string(),
        Value::String(deployment_name.to_string()),
    );
    params.insert("input".to_string(), Value::Array(messages));
    params.insert("stream".to_string(), Value::Bool(true));
    if let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
        params.insert("prompt_cache_key".to_string(), Value::String(key));
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

    if !context.tools.is_empty() {
        let converted = convert_responses_tools(
            &context.tools,
            Some(&ConvertResponsesToolsOptions {
                supports_strict_mode: Some(
                    crate::api::openai_responses::responses_compat_of(model)
                        .supports_strict_mode
                        .unwrap_or(true),
                ),
                ..Default::default()
            }),
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
        } else {
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

fn model_thinking_level_to_thinking(level: ModelThinkingLevel) -> Option<ThinkingLevel> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
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

// --- Stream ----------------------------------------------------------------

pub fn stream(
    model: Model,
    context: Context,
    options: Option<AzureOpenAIResponsesOptions>,
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
    options: AzureOpenAIResponsesOptions,
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
        output.error_message = Some(format_azure_openai_error(&error));
        stream.push(AssistantMessageEvent::Error {
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
    options: &AzureOpenAIResponsesOptions,
    output: &mut AssistantMessage,
    stream: &AssistantMessageEventStream,
) -> Result<(), ProviderRequestError> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            ProviderRequestError::transport(format!("No API key for provider: {}", model.provider))
        })?;
    let deployment_name = resolve_deployment_name(model, Some(options));
    let config =
        resolve_azure_config(model, Some(options)).map_err(ProviderRequestError::transport)?;
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        Some(&context.tools),
        crate::api::openai_responses::responses_compat_of(model)
            .supports_openai_grammar_tools
            .unwrap_or(false),
    );
    let mut params = build_params(
        model,
        context,
        options,
        &deployment_name,
        &grammar_tool_input_properties,
    );
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let headers = merge_request_headers(
        vec![
            ("User-Agent".to_string(), get_user_agent()),
            ("Authorization".to_string(), format!("Bearer {api_key}")),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        model.headers.as_ref(),
        options.headers.as_ref(),
    );

    // Upstream's AzureOpenAI SDK appends ?api-version=<version>.
    let request = crate::transport::FetchRequest {
        method: "POST".to_string(),
        url: format!(
            "{}/responses?api-version={}",
            config.base_url.trim_end_matches('/'),
            urlencode(&config.api_version)
        ),
        headers,
        body: Some(serde_json::to_vec(&params).map_err(|error| {
            ProviderRequestError::transport(format!("failed to serialize request body: {error}"))
        })?),
    };

    let fetch = options
        .fetch
        .clone()
        .unwrap_or_else(crate::api::openai_responses::default_fetch_shared);
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
                async move { fetch_json_stream(&fetch, request, signal.as_ref(), timeout_ms).await }
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
        partial: output.clone(),
    });

    let sse = crate::api::openai_completions::SseJsonEvents::new(SseDataEvents::new(response.body));
    tokio::pin!(sse);
    // Race the abort around the whole event pump (see openai_responses).
    crate::api::race_abort(
        options.signal.as_ref(),
        process_responses_stream(&mut sse, output, stream, model, None),
    )
    .await?
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
            "Azure OpenAI Responses stream ended without a stop reason".to_string(),
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

    stream.push(AssistantMessageEvent::Done {
        reason: output.stop_reason,
        message: output.clone(),
    });
    stream.end(Some(output.clone()));
    Ok(())
}

/// Minimal percent-encoding for a query parameter value.
fn urlencode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Upstream `streamSimple` with `buildBaseOptions`.
pub fn stream_simple(
    model: Model,
    context: Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.unwrap_or_default();
    let api_key = options.api_key.clone().unwrap_or_default();
    if api_key.is_empty() {
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

    // buildBaseOptions: clamp maxTokens to the remaining context window.
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = simple_options::clamp_max_tokens_to_context(
        model.context_window,
        &context,
        base_max_tokens,
    );

    let clamped_reasoning = options.reasoning.map(|reasoning| {
        clamp_thinking_level(
            &model,
            crate::api::openai_responses::to_model_thinking_level_pub(reasoning),
        )
    });

    stream(
        model,
        context,
        Some(AzureOpenAIResponsesOptions {
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
            session_id: options.session_id,
            tool_choice: options.tool_choice,
            // Upstream: "off" clamps to undefined reasoning effort.
            reasoning_effort: match clamped_reasoning {
                Some(ModelThinkingLevel::Off) => None,
                Some(level) => model_thinking_level_to_thinking(level),
                None => None,
            },
            ..Default::default()
        }),
    )
}
