//! Port of packages/ai/src/api/google-vertex.ts (pi v0.84.3).
//!
//! Google Vertex AI adapter. Shares the streaming loop with
//! google-generative-ai; differs in client construction (project/location +
//! ADC or API key), API version pinning (v1), base-URL resource scope, and
//! the thinking-budget table (no 2.5-flash-lite entry).
//!
//! divergence: upstream talks to the @google/genai SDK; the Rust port builds
//! raw requests via `FetchFn` against `{baseUrl}/projects/{project}/
//! locations/{location}/publishers/google/models/{model}:streamGenerate
//! Content?alt=sse` (ADC path) or the API-key express endpoint, and decodes
//! SSE directly. ADC token minting (google-auth-library OAuth2 JWT→access
//! token exchange) is NOT ported — the ADC path requires a pre-minted
//! Bearer token supplied via options/env; only the API-key path is fully
//! self-sufficient (see docs/INSTRUCTIONS.md #52).

use serde_json::Value;

use crate::api::google_generative_ai::{
    GoogleOptions, build_params as build_params_common, is_gemini_3_flash_model,
    is_gemini_3_pro_model, stream_with_request, to_model_thinking_level,
};
use crate::api::google_shared::{ResolvedGoogleThinkingLevel, resolve_google_thinking_level};
use crate::api::impl_from_request_options;
use crate::event_stream::assistant_message_event_stream;
use crate::provider_env::get_provider_env_value;
use crate::types::{AssistantMessageEvent, StopReason};

pub const API_VERSION: &str = "v1";
pub const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

/// Upstream `GoogleVertexOptions`.
#[derive(Default)]
pub struct GoogleVertexOptions {
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
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<i64>,
    pub thinking_level: Option<String>,
    pub project: Option<String>,
    pub location: Option<String>,
}

impl_from_request_options!(GoogleVertexOptions);
impl_from_request_options!(SimpleStreamOptions);

/// Upstream `SimpleStreamOptions` for google-vertex.
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

/// Upstream `resolveApiKey`: API key unless it is empty, the
/// gcp-vertex-credentials marker, or a `<placeholder>`.
pub fn resolve_api_key(options: &GoogleVertexOptions) -> Option<String> {
    let key = options.api_key.as_deref()?.trim();
    if key.is_empty() || key == GCP_VERTEX_CREDENTIALS_MARKER || is_placeholder_api_key(key) {
        return None;
    }
    Some(key.to_string())
}

fn is_placeholder_api_key(api_key: &str) -> bool {
    // /^<[^>]+>$/
    api_key.len() > 2
        && api_key.starts_with('<')
        && api_key.ends_with('>')
        && api_key[1..api_key.len() - 1].chars().all(|c| c != '>')
}

/// Upstream `resolveProject`.
pub fn resolve_project(options: &GoogleVertexOptions) -> Result<String, String> {
    let project = options
        .project
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_PROJECT", options.env.as_ref()))
        .or_else(|| get_provider_env_value("GCLOUD_PROJECT", options.env.as_ref()));
    project.ok_or_else(|| {
        "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
            .to_string()
    })
}

/// Upstream `resolveLocation`.
pub fn resolve_location(options: &GoogleVertexOptions) -> Result<String, String> {
    options
        .location
        .clone()
        .filter(|l| !l.is_empty())
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_LOCATION", options.env.as_ref()))
        .ok_or_else(|| {
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
                .to_string()
        })
}

/// Upstream `resolveCustomBaseUrl`: trim; `{location}` placeholders are
/// treated as "no custom base URL".
pub fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() || trimmed.contains("{location}") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Upstream `baseUrlIncludesApiVersion`: does any path segment match
/// `/^v\d+(?:beta\d*)?$/`? URL parse failure falls back to a regex-like scan.
pub fn base_url_includes_api_version(base_url: &str) -> bool {
    // Fallback: scan raw string for /(?:^|\/)v\d+(?:beta\d*)?(?:\/|$)/
    let bytes = base_url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        if is_version_segment(&base_url[start..i]) {
            return true;
        }
    }
    false
}

fn is_version_segment(part: &str) -> bool {
    let Some(rest) = part.strip_prefix('v') else {
        return false;
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let after = &rest[digits.len()..];
    after.is_empty()
        || after
            .strip_prefix("beta")
            .is_some_and(|b| b.chars().all(|c| c.is_ascii_digit()))
}

/// Upstream `buildHttpOptions`: resolve base URL (resource scope
/// COLLECTION), apiVersion suppression when the base URL already includes a
/// version, and merged headers. Returns (baseUrl, apiVersion, headers).
pub fn build_http_options(
    model: &crate::types::Model,
    options_headers: Option<&crate::types::ProviderHeaders>,
) -> (Option<String>, Option<String>, Vec<(String, String)>) {
    let mut base_url: Option<String> = None;
    let mut api_version: Option<String> = Some(API_VERSION.to_string());
    if let Some(custom) = resolve_custom_base_url(&model.base_url) {
        base_url = Some(custom);
        if base_url_includes_api_version(base_url.as_deref().unwrap()) {
            api_version = Some(String::new());
        }
    }
    let headers = crate::api::merge_request_headers(
        vec![("User-Agent".to_string(), crate::api::get_pi_user_agent())],
        model.headers.as_ref(),
        options_headers,
    );
    (base_url, api_version, headers)
}

/// Build the streaming URL for a Vertex request.
///
/// - API-key path (upstream Vertex Express): `https://aiplatform.googleapis.com/`
///   with no project/location path segments.
/// - ADC path: `{baseUrl}/{apiVersion}/projects/{project}/locations/{location}/
///   publishers/google/models/{model}:streamGenerateContent?alt=sse`
pub fn build_vertex_url(
    model: &crate::types::Model,
    options: &GoogleVertexOptions,
    api_key: Option<&str>,
) -> Result<String, String> {
    let (base_url, api_version, _headers) = build_http_options(model, options.headers.as_ref());
    let base = base_url.unwrap_or_else(|| "https://aiplatform.googleapis.com".to_string());
    let base = base.trim_end_matches('/');
    let path_model = format!(
        "publishers/google/models/{}:streamGenerateContent",
        model.id
    );
    let version = api_version.unwrap_or_else(|| API_VERSION.to_string());

    if api_key.is_some() {
        // Express endpoint: no project/location path.
        return Ok(format!("{base}/{version}/{path_model}?alt=sse"));
    }
    let project = resolve_project(options)?;
    let location = resolve_location(options)?;
    Ok(format!(
        "{base}/{version}/projects/{project}/locations/{location}/{path_model}?alt=sse"
    ))
}

/// Build the request the shared Google stream runner will execute.
pub fn build_vertex_request(
    model: &crate::types::Model,
    context: &crate::types::Context,
    options: &GoogleVertexOptions,
) -> Result<crate::transport::FetchRequest, String> {
    let api_key = resolve_api_key(options);
    let url = build_vertex_url(model, options, api_key.as_deref())?;

    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(key) = &api_key {
        headers.push(("x-goog-api-key".to_string(), key.clone()));
    }
    let headers = crate::api::merge_request_headers(
        headers,
        model.headers.as_ref(),
        options.headers.as_ref(),
    );

    let params = build_params(model, context, options)?;
    let body = serde_json::to_vec(&params)
        .map_err(|error| format!("failed to serialize request body: {error}"))?;

    Ok(crate::transport::FetchRequest {
        method: "POST".to_string(),
        url,
        headers,
        body: Some(body),
    })
}

fn build_params(
    model: &crate::types::Model,
    context: &crate::types::Context,
    options: &GoogleVertexOptions,
) -> Result<Value, String> {
    let common: GoogleOptions = GoogleOptions {
        signal: options.signal.clone(),
        api_key: options.api_key.clone(),
        fetch: options.fetch.clone(),
        env: options.env.clone(),
        on_payload: options.on_payload.clone(),
        on_response: options.on_response.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        tool_choice: options.tool_choice.clone(),
        thinking_enabled: options.thinking_enabled,
        thinking_budget_tokens: options.thinking_budget_tokens,
        thinking_level: options.thinking_level.clone(),
    };
    build_params_common(model, context, &common)
}

// --- stream / streamSimple -----------------------------------------------------

/// Upstream `stream()` — returns an event stream; the request runs on a
/// spawned task against the default or injected transport.
pub fn stream(
    model: crate::types::Model,
    context: crate::types::Context,
    options: Option<GoogleVertexOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let stream = assistant_message_event_stream();
    let task_stream = stream.clone_stream();
    tokio::spawn(async move {
        let options = options.unwrap_or_default();
        let mut output = fresh_output(&model);

        // ADC path: project/location are required; validated before the
        // request build so the error surfaces through the same channel.
        let preflight = if resolve_api_key(&options).is_none() {
            resolve_project(&options).and_then(|_| resolve_location(&options))
        } else {
            Ok(String::new())
        };
        let result = async {
            preflight?;
            let request = build_vertex_request(&model, &context, &options)?;
            stream_with_request(
                &model,
                &context,
                &to_common_options(&options),
                request,
                &mut output,
                &task_stream,
            )
            .await
        };
        if let Err(error) = result.await {
            let aborted = options
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted());
            output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(error);
            task_stream.push(AssistantMessageEvent::Error {
                reason: output.stop_reason,
                error: output.clone(),
            });
            task_stream.end(Some(output.clone()));
        }
    });
    stream
}

fn fresh_output(model: &crate::types::Model) -> crate::types::AssistantMessage {
    crate::types::AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn to_common_options(options: &GoogleVertexOptions) -> GoogleOptions {
    GoogleOptions {
        signal: options.signal.clone(),
        api_key: options.api_key.clone(),
        fetch: options.fetch.clone(),
        env: options.env.clone(),
        on_payload: options.on_payload.clone(),
        on_response: options.on_response.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        tool_choice: options.tool_choice.clone(),
        thinking_enabled: options.thinking_enabled,
        thinking_budget_tokens: options.thinking_budget_tokens,
        thinking_level: options.thinking_level.clone(),
    }
}

/// Upstream `streamSimple()` for google-vertex.
pub fn stream_simple(
    model: crate::types::Model,
    context: crate::types::Context,
    options: Option<SimpleStreamOptions>,
) -> crate::event_stream::AssistantMessageEventStream {
    let options = options.unwrap_or_default();

    let mut vertex_options = GoogleVertexOptions {
        signal: options.signal,
        api_key: options.api_key.clone(),
        fetch: options.fetch.clone(),
        env: options.env.clone(),
        on_payload: options.on_payload.clone(),
        on_response: options.on_response.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        temperature: options.temperature,
        max_tokens: None,
        tool_choice: options.tool_choice.clone(),
        thinking_enabled: Some(false),
        thinking_budget_tokens: None,
        thinking_level: None,
        project: None,
        location: None,
    };

    // buildBaseOptions: clamp maxTokens to the remaining context window.
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    vertex_options.max_tokens = Some(crate::simple_options::clamp_max_tokens_to_context(
        model.context_window,
        &context,
        base_max_tokens,
    ));

    if let Some(reasoning) = options.reasoning {
        let clamped =
            crate::models::clamp_thinking_level(&model, to_model_thinking_level(reasoning));
        let resolved_level = resolve_google_thinking_level(&model, clamped)
            .unwrap_or(ResolvedGoogleThinkingLevel::High);
        if is_gemini_3_pro_model(&model.id) || is_gemini_3_flash_model(&model.id) {
            vertex_options.thinking_enabled = Some(true);
            vertex_options.thinking_level =
                Some(get_gemini_3_thinking_level(resolved_level, &model.id).to_string());
        } else {
            vertex_options.thinking_enabled = Some(true);
            vertex_options.thinking_budget_tokens = Some(get_google_budget(
                &model.id,
                resolved_level,
                options.thinking_budgets.as_ref(),
            ));
        }
    }

    stream(model, context, Some(vertex_options))
}

/// Upstream `getGemini3ThinkingLevel` (vertex variant: no Gemma 4 branch).
pub fn get_gemini_3_thinking_level(
    effort: ResolvedGoogleThinkingLevel,
    model_id: &str,
) -> &'static str {
    use ResolvedGoogleThinkingLevel as L;
    if is_gemini_3_pro_model(model_id) {
        return match effort {
            L::Minimal | L::Low => "LOW",
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

/// Upstream `getGoogleBudget` (vertex variant: no 2.5-flash-lite entry).
pub fn get_google_budget(
    model_id: &str,
    level: ResolvedGoogleThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> i64 {
    use ResolvedGoogleThinkingLevel as L;
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

/// ADC credential file check (upstream `buildGoogleAuthOptions` +
/// google-auth-library). The Rust port does not mint OAuth tokens; callers
/// must supply a Bearer via headers. This only surfaces the credential path
/// the way upstream would read it.
pub fn adc_credentials_path(env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env)
        .or_else(|| Some(VERTEX_ADC_PATH.to_string()))
}

/// Whether the ADC path exists on disk (upstream google-auth-library reads
/// it to mint a token).
pub fn adc_credentials_available(env: Option<&crate::types::ProviderEnv>) -> bool {
    adc_credentials_path(env)
        .map(|path| {
            let expanded = if let Some(rest) = path.strip_prefix("~/") {
                dirs_home()
                    .map(|home| format!("{home}/{rest}"))
                    .unwrap_or(path)
            } else {
                path
            };
            std::path::Path::new(&expanded).exists()
        })
        .unwrap_or(false)
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}
