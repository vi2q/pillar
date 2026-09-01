//! Port of packages/ai/src/api (pi v0.84.3) — provider API adapters.
//!
//! divergence: upstream adapters call provider SDKs; the Rust adapters build
//! request JSON directly and send it through [`crate::transport::FetchFn`],
//! with the SSE framing parsed locally.

pub mod anthropic_messages;
pub mod azure_openai_responses;
pub mod bedrock_converse_stream;
pub mod github_copilot_headers;
pub mod google_generative_ai;
pub mod google_shared;
pub mod mistral_conversations;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_prompt_cache;
pub mod openai_responses;
pub mod openai_responses_shared;
pub mod openrouter_images;
pub mod pi_messages;

use std::sync::Arc;

use crate::abort::AbortSignal;
use crate::auth_types::BoxFuture;
use crate::provider_retry::ProviderRequestError;
use crate::transport::{FetchResponse, SharedFetchFn};
use crate::types::{CacheRetention, Model, ProviderEnv, ProviderHeaders, Transport, Usage};

/// Upstream `ProviderResponse` — HTTP response metadata handed to
/// `onResponse`.
#[derive(Debug, Clone)]
pub struct ProviderResponseInfo {
    pub status: u16,
    /// Header name/value pairs (lowercased names).
    pub headers: Vec<(String, String)>,
}

/// Upstream `onPayload`: inspect or replace the provider request payload
/// before sending. Returning `None` keeps the payload unchanged.
pub type OnPayloadFn = Arc<
    dyn Fn(&Model, serde_json::Value) -> BoxFuture<'static, Option<serde_json::Value>>
        + Send
        + Sync,
>;

/// Upstream `onResponse`: invoked after an HTTP response is received.
pub type OnResponseFn =
    Arc<dyn Fn(ProviderResponseInfo, &Model) -> BoxFuture<'static, ()> + Send + Sync>;

/// Base options shared by all provider stream calls (upstream
/// `ProviderRequestOptions` + `StreamOptions`).
#[derive(Default)]
pub struct BaseStreamOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<SharedFetchFn>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    /// Arbitrary sampling parameters merged into the request body as-is,
    /// after the named fields, so keys here override them. Merged over
    /// `Model.samplingParams` per key. Only applied by OpenAI-compatible
    /// adapters.
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Resolved user agent for provider requests (upstream
/// `getPiUserAgent()`).
///
/// divergence: upstream uses the Node `os` module (platform, release,
/// architecture); the Rust port uses compile-time `std::env::consts` and has
/// no OS release string.
pub fn get_pi_user_agent() -> String {
    format!("pi ({} {})", std::env::consts::OS, std::env::consts::ARCH)
}

/// Resolve the effective cache retention from options and env (upstream
/// `resolveCacheRetention`).
pub fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    if let Some(cache_retention) = cache_retention {
        return cache_retention;
    }
    if crate::provider_env::get_provider_env_value("PI_CACHE_RETENTION", env).as_deref()
        == Some("long")
    {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// Merge provider default headers, model headers, and caller headers.
/// `None` values suppress a default header; caller values override by exact
/// key (upstream `Object.assign` semantics).
pub fn merge_request_headers(
    defaults: Vec<(String, String)>,
    model_headers: Option<&ProviderHeaders>,
    options_headers: Option<&ProviderHeaders>,
) -> Vec<(String, String)> {
    // HTTP header names are case-insensitive (upstream uses the WHATWG
    // Headers API); match overrides case-insensitively so an
    // `"Authorization": null` override suppresses a model-level
    // `"Authorization"` header.
    let mut headers: Vec<(String, String)> = Vec::new();
    let set_or_replace = |headers: &mut Vec<(String, String)>, name: String, value: String| {
        if let Some(slot) = headers
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        {
            slot.1 = value;
        } else {
            headers.push((name, value));
        }
    };

    for (name, value) in defaults {
        set_or_replace(&mut headers, name, value);
    }
    if let Some(model_headers) = model_headers {
        for (name, value) in model_headers {
            if let Some(value) = value {
                set_or_replace(&mut headers, name.clone(), value.clone());
            } else {
                headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            }
        }
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            if let Some(value) = value {
                set_or_replace(&mut headers, name.clone(), value.clone());
            } else {
                headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            }
        }
    }
    headers
}

/// Transport error carrying HTTP context for the retry layer.
pub(crate) async fn fetch_json_stream(
    fetch: &SharedFetchFn,
    request: crate::transport::FetchRequest,
    signal: Option<&AbortSignal>,
    timeout_ms: Option<u64>,
) -> Result<crate::transport::FetchResponse, ProviderRequestError> {
    let do_fetch = async {
        fetch
            .fetch(request)
            .await
            .map_err(|error| ProviderRequestError::transport(error.to_string()))
    };
    let raced = async {
        match signal {
            Some(signal) => {
                tokio::select! {
                    biased;
                    _ = signal.aborted_or_pending() => Err(ProviderRequestError {
                        status: None,
                        headers: Vec::new(),
                        message: "Request aborted".to_string(),
                        aborted: true,
                    }),
                    response = do_fetch => response,
                }
            }
            None => do_fetch.await,
        }
    };
    let response = match timeout_ms {
        Some(timeout_ms) => {
            let duration = std::time::Duration::from_millis(timeout_ms);
            tokio::time::timeout(duration, raced)
                .await
                .map_err(|_| ProviderRequestError::transport("Request timed out".to_string()))?
        }
        None => raced.await,
    }?;

    let FetchResponse {
        status,
        headers,
        body,
    } = response;
    if (200..300).contains(&status) {
        return Ok(FetchResponse {
            status,
            headers,
            body,
        });
    }

    // Non-2xx: fold the body into the message like the provider SDKs do.
    let body_text = crate::transport::FetchResponse {
        status,
        headers: headers.clone(),
        body,
    }
    .text()
    .await
    .unwrap_or_default();
    let message = if body_text.trim().is_empty() {
        format!("Request failed with status code {status}")
    } else {
        body_text
    };
    Err(ProviderRequestError::http(status, headers, message))
}

/// Default error surface for stream failures (upstream:
/// `formatProviderError(normalizeProviderError(error))`).
pub(crate) fn format_stream_error(error: &ProviderRequestError) -> String {
    let norm =
        crate::error_body::normalize_provider_error(error.message.clone(), error.status, None);
    crate::error_body::format_provider_error(&norm, None)
}

/// Usage constructor with zeroed cost, shared by chunk parsers.
pub(crate) fn zeroed_usage() -> Usage {
    Usage::default()
}
