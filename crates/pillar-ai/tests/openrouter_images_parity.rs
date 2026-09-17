//! Port of the upstream openrouter-images tests (pi v0.84.3): final-output
//! assembly (text + data-URL images), request params (modalities, stream),
//! abort handling, and usage parsing.
//!
//! divergence: upstream mocks the `openai` npm SDK; the Rust port injects a
//! `FetchFn` fake and inspects captured `FetchRequest`s. Live E2E
//! (images.test.ts, requires OPENROUTER_API_KEY) is not portable.

#![cfg(feature = "providers")]

use std::sync::{Arc, Mutex};

use pillar_ai::api::openrouter_images::{
    AssistantImages, ImagesContent, ImagesContext, ImagesModel, ImagesOptions, ImagesStopReason,
    generate_images,
};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{ModelCost, ModelCostRates, ProviderHeaders};

// --- Helpers ---------------------------------------------------------------

struct CapturedFetch {
    requests: Mutex<Vec<FetchRequest>>,
    response_factory: Box<dyn Fn() -> Result<FetchResponse, AiError> + Send + Sync>,
}

impl CapturedFetch {
    fn new(
        response_factory: impl Fn() -> Result<FetchResponse, AiError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            response_factory: Box::new(response_factory),
        })
    }

    fn requests(&self) -> Vec<FetchRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl FetchFn for CapturedFetch {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError> {
        self.requests.lock().unwrap().push(request);
        (self.response_factory)()
    }
}

fn json_response(body: serde_json::Value) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(
            serde_json::to_vec(&body).unwrap()
        )])),
    }
}

fn image_response_body() -> serde_json::Value {
    serde_json::json!({
        "id": "img-1",
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 34,
            "prompt_tokens_details": { "cached_tokens": 0 },
        },
        "choices": [
            {
                "message": {
                    "content": "Here is your image.",
                    "images": [{ "image_url": "data:image/png;base64,ZmFrZS1wbmc=" }],
                },
            },
        ],
    })
}

fn images_model(output: &[&str]) -> ImagesModel {
    ImagesModel {
        id: "black-forest-labs/flux.2-pro".to_string(),
        name: "FLUX.2 Pro".to_string(),
        api: "openrouter-images".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        input: vec!["text".to_string(), "image".to_string()],
        output: output.iter().map(|entry| entry.to_string()).collect(),
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.015,
                output: 0.03,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: None,
        },
        headers: None,
    }
}

fn text_context(text: &str) -> ImagesContext {
    ImagesContext {
        input: vec![ImagesContent::Text {
            text: text.to_string(),
        }],
    }
}

// --- Tests -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn returns_text_plus_images_in_final_output() {
    let fetch = CapturedFetch::new(|| Ok(json_response(image_response_body())));
    let model = images_model(&["text", "image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("Generate a dog"), Some(options)).await;
    assert_eq!(output.stop_reason, ImagesStopReason::Stop);
    assert_eq!(output.response_id.as_deref(), Some("img-1"));
    assert_eq!(
        output.output[0],
        ImagesContent::Text {
            text: "Here is your image.".to_string()
        }
    );
    assert_eq!(
        output.output[1],
        ImagesContent::Image {
            mime_type: "image/png".to_string(),
            data: "ZmFrZS1wbmc=".to_string(),
        }
    );

    let requests = fetch.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://openrouter.ai/api/v1/chat/completions"
    );
    let params: serde_json::Value =
        serde_json::from_slice(requests[0].body.as_deref().expect("body")).unwrap();
    assert_eq!(params.get("stream"), Some(&serde_json::json!(false)));
    assert_eq!(
        params.get("modalities"),
        Some(&serde_json::json!(["image", "text"]))
    );
    assert_eq!(
        params.pointer("/messages/0/content/0"),
        Some(&serde_json::json!({ "type": "text", "text": "Generate a dog" }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn omits_text_modality_when_model_output_is_image_only() {
    let fetch = CapturedFetch::new(|| Ok(json_response(image_response_body())));
    let model = images_model(&["image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    generate_images(model, text_context("Generate a dog"), Some(options)).await;
    let requests = fetch.requests();
    let params: serde_json::Value =
        serde_json::from_slice(requests[0].body.as_deref().expect("body")).unwrap();
    assert_eq!(
        params.get("modalities"),
        Some(&serde_json::json!(["image"]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_through_abort_signal_and_returns_aborted_result() {
    // Scripted fetch that fails like an aborted request would.
    let fetch = CapturedFetch::new(|| Err(AiError::Other("Request aborted".to_string())));
    let model = images_model(&["image"]);
    let signal = pillar_ai::AbortSignal::new();
    signal.abort(None);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        signal: Some(signal),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("Generate a dog"), Some(options)).await;
    assert_eq!(output.stop_reason, ImagesStopReason::Aborted);
    assert_eq!(output.error_message.as_deref(), Some("Request aborted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_error_results_to_error_stop_reason() {
    let fetch = CapturedFetch::new(|| Err(AiError::Other("quota exhausted".to_string())));
    let model = images_model(&["image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("Generate a dog"), Some(options)).await;
    assert_eq!(output.stop_reason, ImagesStopReason::Error);
    let error_message = output.error_message.unwrap_or_default();
    assert!(
        error_message.contains("quota exhausted"),
        "unexpected error: {error_message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn parses_usage_with_cached_token_accounting() {
    let fetch = CapturedFetch::new(|| Ok(json_response(image_response_body())));
    let model = images_model(&["text", "image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output: AssistantImages =
        generate_images(model, text_context("Generate a dog"), Some(options)).await;
    let usage = output.usage.expect("usage parsed");
    assert_eq!(usage.input, 12);
    assert_eq!(usage.output, 34);
    assert_eq!(usage.total_tokens, 46);
    // 0.015/1M * 12
    assert!((usage.cost.input - 0.015 / 1_000_000.0 * 12.0).abs() < 1e-12);
}

#[tokio::test(flavor = "multi_thread")]
async fn computes_cache_read_minus_cache_write() {
    let body = serde_json::json!({
        "id": "img-2",
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 20 },
        },
        "choices": [],
    });
    let fetch = CapturedFetch::new(move || Ok(json_response(body.clone())));
    let model = images_model(&["image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("x"), Some(options)).await;
    let usage = output.usage.expect("usage parsed");
    // cacheRead = 30 - 20 = 10; input = 100 - 10 - 20 = 70
    assert_eq!(usage.cache_read, 10);
    assert_eq!(usage.cache_write, 20);
    assert_eq!(usage.input, 70);
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_object_shaped_image_urls_and_skips_non_data_urls() {
    let body = serde_json::json!({
        "id": "img-3",
        "choices": [
            {
                "message": {
                    "content": "",
                    "images": [
                        { "image_url": { "url": "data:image/jpeg;base64,anBlZw==" } },
                        { "image_url": "https://example.com/not-data.png" },
                        { "image_url": "data:text/plain;base64,bm90LWltYWdl" },
                    ],
                },
            },
        ],
    });
    let fetch = CapturedFetch::new(move || Ok(json_response(body.clone())));
    let model = images_model(&["image"]);
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("x"), Some(options)).await;
    // Upstream only checks the `data:` prefix (no MIME allow-list): the
    // object-shaped URL and the text/plain data URL are kept, the https URL
    // is dropped.
    assert_eq!(output.output.len(), 2);
    assert_eq!(
        output.output[0],
        ImagesContent::Image {
            mime_type: "image/jpeg".to_string(),
            data: "anBlZw==".to_string(),
        }
    );
    assert_eq!(
        output.output[1],
        ImagesContent::Image {
            mime_type: "text/plain".to_string(),
            data: "bm90LWltYWdl".to_string(),
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lets_explicit_headers_override_defaults() {
    let fetch = CapturedFetch::new(|| Ok(json_response(image_response_body())));
    let mut model = images_model(&["image"]);
    model.headers = Some(ProviderHeaders::from([(
        "HTTP-Referer".to_string(),
        Some("https://example.com".to_string()),
    )]));
    let options = ImagesOptions {
        api_key: Some("test".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    generate_images(model, text_context("x"), Some(options)).await;
    let requests = fetch.requests();
    let referer = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("HTTP-Referer"))
        .map(|(_, value)| value.clone());
    assert_eq!(referer.as_deref(), Some("https://example.com"));
    let authorization = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
        .map(|(_, value)| value.clone());
    assert_eq!(authorization.as_deref(), Some("Bearer test"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_missing_api_key() {
    let fetch = CapturedFetch::new(|| Ok(json_response(image_response_body())));
    let model = images_model(&["image"]);
    let options = ImagesOptions {
        fetch: Some(fetch.clone()),
        ..Default::default()
    };

    let output = generate_images(model, text_context("x"), Some(options)).await;
    assert_eq!(output.stop_reason, ImagesStopReason::Error);
    assert!(
        output
            .error_message
            .unwrap_or_default()
            .contains("No API key for provider: openrouter")
    );
}
