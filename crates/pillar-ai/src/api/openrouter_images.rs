//! Port of packages/ai/src/types.ts image-generation types and
//! packages/ai/src/api/openrouter-images.ts (pi v0.84.3).
//!
//! Image generation types (`ImagesModel`, `ImagesContext`, `AssistantImages`,
//! ...) live here alongside the openrouter-images adapter, the only images
//! API upstream ships.
//!
//! divergence: upstream goes through the `openai` npm SDK's
//! `chat.completions.create().withResponse()`; the port issues a raw
//! non-streaming POST to `<base_url>/chat/completions`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::{ProviderHeaders, ProviderResponseInfo, get_pi_user_agent, merge_request_headers};

/// Upstream `onPayload` for images: inspect or replace the request payload
/// before sending. Return `Some(next)` to replace, `None` to keep.
pub type ImagesOnPayloadFn = Arc<
    dyn Fn(&ImagesModel, serde_json::Value) -> BoxFuture<'static, Option<serde_json::Value>>
        + Send
        + Sync,
>;
use crate::AbortSignal;
use futures::future::BoxFuture;

/// Upstream `onResponse` for images: invoked after an HTTP response is
/// received.
pub type ImagesOnResponseFn =
    Arc<dyn Fn(ProviderResponseInfo, &ImagesModel) -> BoxFuture<'static, ()> + Send + Sync>;
use crate::error_body;
use crate::provider_retry::{ProviderRequestError, retry_provider_request};
use crate::text::sanitize_surrogates;
use crate::transport::FetchRequest;
use crate::types::{ModelCost, ProviderEnv, Usage};

// ===========================================================================
// Image-generation types (upstream types.ts)
// ===========================================================================

/// Upstream `TextContent` / `ImageContent` for images input/output.
#[derive(Debug, Clone, PartialEq)]
pub enum ImagesContent {
    Text { text: String },
    Image { data: String, mime_type: String },
}

/// Upstream `ImagesModel`.
#[derive(Debug, Clone)]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub cost: ModelCost,
    pub headers: Option<ProviderHeaders>,
}

/// Upstream `ImagesContext`.
#[derive(Debug, Clone, Default)]
pub struct ImagesContext {
    pub input: Vec<ImagesContent>,
}

/// Upstream `ImagesStopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

/// Upstream `AssistantImages`.
#[derive(Debug, Clone)]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesContent>,
    pub response_id: Option<String>,
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    pub error_message: Option<String>,
    pub timestamp: u64,
}

/// Upstream `ImagesOptions`.
#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<ImagesOnPayloadFn>,
    pub on_response: Option<ImagesOnResponseFn>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
}

// ===========================================================================
// openrouter-images adapter
// ===========================================================================

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_error(error: &ProviderRequestError) -> String {
    let norm = error_body::normalize_provider_error(error.message.clone(), error.status, None);
    error_body::format_provider_error(&norm, None)
}

/// Upstream `parseUsage`: cached tokens subtracted from prompt tokens.
fn parse_usage(raw_usage: &Value, model: &ImagesModel) -> Usage {
    let prompt_tokens = raw_usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reported_cached_tokens = raw_usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .pointer("/prompt_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_tokens = if cache_write_tokens > 0 {
        reported_cached_tokens.saturating_sub(cache_write_tokens)
    } else {
        reported_cached_tokens
    };
    let input = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    let output = raw_usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let rates = &model.cost.rates;
    let cost_input = rates.input / 1_000_000.0 * input as f64;
    let cost_output = rates.output / 1_000_000.0 * output as f64;
    let cost_cache_read = rates.cache_read / 1_000_000.0 * cache_read_tokens as f64;
    let cost_cache_write = rates.cache_write / 1_000_000.0 * cache_write_tokens as f64;
    Usage {
        input,
        output,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        total_tokens: input + output + cache_read_tokens + cache_write_tokens,
        cost: crate::types::UsageCost {
            input: cost_input,
            output: cost_output,
            cache_read: cost_cache_read,
            cache_write: cost_cache_write,
            total: cost_input + cost_output + cost_cache_read + cost_cache_write,
        },
        ..Usage::default()
    }
}

/// Upstream `buildParams`: single user message with text/image_url parts and
/// the `modalities` extension field.
pub fn build_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            ImagesContent::Text { text } => json!({
                "type": "text",
                "text": sanitize_surrogates(text),
            }),
            ImagesContent::Image { data, mime_type } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            }),
        })
        .collect();

    json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": if model.output.iter().any(|entry| entry == "text") {
            json!(["image", "text"])
        } else {
            json!(["image"])
        },
    })
}

/// Upstream `generateImages` for the `openrouter-images` API.
pub async fn generate_images(
    model: ImagesModel,
    context: ImagesContext,
    options: Option<ImagesOptions>,
) -> AssistantImages {
    let options = options.unwrap_or_default();
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };

    let result = generate_images_inner(&model, &context, &options, &mut output).await;
    if let Err(error) = result {
        output.stop_reason = if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            ImagesStopReason::Aborted
        } else {
            ImagesStopReason::Error
        };
        output.error_message = Some(format_error(&error));
    }
    output
}

async fn generate_images_inner(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    output: &mut AssistantImages,
) -> Result<(), ProviderRequestError> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            ProviderRequestError::transport(format!("No API key for provider: {}", model.provider))
        })?
        .to_string();

    let mut params = build_params(model, context);
    if let Some(on_payload) = &options.on_payload {
        if let Some(next_params) = on_payload(model, params.clone()).await {
            params = next_params;
        }
    }

    let headers = merge_request_headers(
        vec![
            ("User-Agent".to_string(), get_pi_user_agent()),
            ("Authorization".to_string(), format!("Bearer {api_key}")),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        model.headers.as_ref(),
        options.headers.as_ref(),
    );

    let request = FetchRequest {
        method: "POST".to_string(),
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers,
        body: Some(serde_json::to_vec(&params).map_err(|error| {
            ProviderRequestError::transport(format!("failed to serialize request body: {error}"))
        })?),
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

    let body = collect_body(response.body).await?;
    let image_response: Value = serde_json::from_slice(&body).map_err(|error| {
        ProviderRequestError::transport(format!("invalid response JSON: {error}"))
    })?;

    if let Some(id) = image_response.get("id").and_then(Value::as_str) {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = image_response.get("usage").filter(|usage| !usage.is_null()) {
        output.usage = Some(parse_usage(usage, model));
    }

    let choice = image_response
        .pointer("/choices/0")
        .cloned()
        .unwrap_or(Value::Null);
    if !choice.is_null() {
        if let Some(content) = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
        {
            output.output.push(ImagesContent::Text {
                text: content.to_string(),
            });
        }

        for image in choice
            .pointer("/message/images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let image_url = match image.get("image_url") {
                Some(Value::String(url)) => Some(url.clone()),
                Some(Value::Object(object)) => object
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            };
            let Some(image_url) = image_url.filter(|url| url.starts_with("data:")) else {
                continue;
            };
            // data:<mime>;base64,<data>
            let Some((mime_part, data_part)) = image_url
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
            else {
                continue;
            };
            output.output.push(ImagesContent::Image {
                mime_type: mime_part.to_string(),
                data: data_part.to_string(),
            });
        }
    }

    Ok(())
}

/// Collect a byte stream into a single buffer.
async fn collect_body(body: crate::transport::ByteStream) -> Result<Vec<u8>, ProviderRequestError> {
    use futures::StreamExt;
    let mut buffered = Vec::new();
    let mut stream = body;
    while let Some(chunk) = stream.next().await {
        buffered.extend_from_slice(
            &chunk.map_err(|error| ProviderRequestError::transport(error.to_string()))?,
        );
    }
    Ok(buffered)
}

fn default_fetch() -> crate::transport::SharedFetchFn {
    Arc::new(
        crate::transport::ReqwestFetch::new()
            .unwrap_or_else(|error| panic!("default transport unavailable: {error}")),
    )
}
