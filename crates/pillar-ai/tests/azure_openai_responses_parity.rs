//! Port of the upstream azure-openai tests (pi v0.84.3) that run without
//! live Azure credentials: base URL normalization, prompt-cache-key clamping,
//! store:false, strict-mode compat, resource-name defaults, user agent
//! handling, tool choice forwarding, and reasoning replay.
//!
//! divergence: upstream mocks the `openai` npm SDK's `AzureOpenAI` client;
//! the Rust port injects a `FetchFn` fake and inspects captured
//! `FetchRequest`s (URL, headers, JSON body) instead.

use std::sync::{Arc, Mutex};

use pillar_ai::api::azure_openai_responses::{
    AzureOpenAIResponsesOptions, SimpleStreamOptions, resolve_deployment_name, stream,
    stream_simple,
};
use pillar_ai::api::{OnPayloadFn, get_user_agent};
use pillar_ai::error::AiError;
use pillar_ai::transport::{FetchFn, FetchRequest, FetchResponse};
use pillar_ai::types::{Context, Model, ModelCompat, ModelCost, StopReason};

// --- Helpers ---------------------------------------------------------------

fn azure_normalize(base_url: &str) -> Result<String, String> {
    pillar_ai::api::azure_openai_responses::normalize_azure_base_url_pub(base_url)
}

struct CapturedFetch {
    requests: Mutex<Vec<FetchRequest>>,
    /// Factory invoked per fetch to build a fresh response stream.
    response_factory: Box<dyn Fn() -> FetchResponse + Send + Sync>,
}

impl CapturedFetch {
    fn always(response_factory: impl Fn() -> FetchResponse + Send + Sync + 'static) -> Arc<Self> {
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
        Ok((self.response_factory)())
    }
}

fn sse_response(body: impl Into<String>) -> FetchResponse {
    FetchResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(vec![Ok(body.into().into_bytes())])),
    }
}

fn minimal_sse() -> FetchResponse {
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": { "cached_tokens": 0 },
            },
        },
    });
    let body = format!("data: {}\n\ndata: [DONE]\n\n", completed);
    sse_response(body)
}

fn azure_model() -> Model {
    Model {
        id: "gpt-4o-mini".to_string(),
        name: "GPT-4o mini".to_string(),
        api: "azure-openai-responses".to_string(),
        provider: "azure-openai-responses".to_string(),
        base_url: "https://my-resource.openai.azure.com/openai/v1".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 16_384,
        sampling_params: None,
        headers: None,
        compat: Some(ModelCompat::OpenaiResponses(Default::default())),
    }
}

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: 1,
        }],
        tools: Vec::new(),
    }
}
use pillar_ai::types::{Message, UserContent};

fn options_with(fetch: Arc<CapturedFetch>) -> AzureOpenAIResponsesOptions {
    AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        fetch: Some(fetch),
        ..Default::default()
    }
}

fn last_body(fetch: &CapturedFetch) -> serde_json::Value {
    let requests = fetch.requests();
    let request = requests.last().expect("captured request");
    serde_json::from_slice(request.body.as_deref().expect("request body"))
        .expect("request body is JSON")
}

// --- Base URL normalization -------------------------------------------------

#[test]
fn normalizes_cognitive_services_root_endpoints() {
    assert_eq!(
        azure_normalize("https://marc-quicktests-resource.cognitiveservices.azure.com").unwrap(),
        "https://marc-quicktests-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[test]
fn normalizes_microsoft_foundry_root_endpoints() {
    assert_eq!(
        azure_normalize("https://marc-quicktests-resource.ai.azure.com").unwrap(),
        "https://marc-quicktests-resource.ai.azure.com/openai/v1"
    );
}

#[test]
fn normalizes_azure_openai_root_endpoints() {
    assert_eq!(
        azure_normalize("https://my-resource.openai.azure.com").unwrap(),
        "https://my-resource.openai.azure.com/openai/v1"
    );
}

#[test]
fn normalizes_openai_path_to_openai_v1() {
    assert_eq!(
        azure_normalize("https://my-resource.cognitiveservices.azure.com/openai").unwrap(),
        "https://my-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[test]
fn preserves_openai_v1_endpoints() {
    assert_eq!(
        azure_normalize("https://my-resource.cognitiveservices.azure.com/openai/v1").unwrap(),
        "https://my-resource.cognitiveservices.azure.com/openai/v1"
    );
}

#[test]
fn normalizes_openai_v1_responses_to_openai_v1() {
    assert_eq!(
        azure_normalize("https://my-resource.services.ai.azure.com/openai/v1/responses").unwrap(),
        "https://my-resource.services.ai.azure.com/openai/v1"
    );
}

#[test]
fn preserves_explicit_non_azure_proxy_paths() {
    assert_eq!(
        azure_normalize("https://my-proxy.example.com/v1").unwrap(),
        "https://my-proxy.example.com/v1"
    );
}

#[test]
fn strips_query_params_when_normalizing_azure_host_urls() {
    assert_eq!(
        azure_normalize("https://my-resource.openai.azure.com/openai?api-version=2024-12-01")
            .unwrap(),
        "https://my-resource.openai.azure.com/openai/v1"
    );
}

#[test]
fn preserves_query_params_on_non_azure_proxy_urls() {
    assert_eq!(
        azure_normalize("https://my-proxy.example.com/v1?custom=true").unwrap(),
        "https://my-proxy.example.com/v1?custom=true"
    );
}

#[test]
fn rejects_invalid_urls() {
    assert!(azure_normalize("not-a-url").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn error_result_carries_invalid_base_url_message() {
    let fetch = CapturedFetch::always(minimal_sse);
    let model = Model {
        base_url: "not-a-url".to_string(),
        ..azure_model()
    };
    let message = stream(model, context(), Some(options_with(fetch)))
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.unwrap_or_default();
    assert!(
        error_message.contains("Invalid Azure OpenAI base URL"),
        "unexpected error: {error_message}"
    );
}

// --- Request params ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn clamps_prompt_cache_key_to_openai_64_character_limit() {
    let fetch = CapturedFetch::always(minimal_sse);
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        session_id: Some("x".repeat(67)),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(azure_model(), context(), Some(options))
        .result()
        .await;
    let body = last_body(&fetch);
    assert_eq!(
        body.get("prompt_cache_key")
            .and_then(|value| value.as_str()),
        Some("x".repeat(64).as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disables_server_side_response_storage() {
    let fetch = CapturedFetch::always(minimal_sse);
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(azure_model(), context(), Some(options))
        .result()
        .await;
    assert_eq!(
        last_body(&fetch).get("store"),
        Some(&serde_json::json!(false))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn honors_supports_strict_mode_false() {
    let fetch = CapturedFetch::always(minimal_sse);
    let model = Model {
        compat: Some(ModelCompat::OpenaiResponses(Box::new(
            pillar_ai::types::OpenaiResponsesCompat {
                supports_strict_mode: Some(false),
                ..Default::default()
            },
        ))),
        ..azure_model()
    };
    let mut context = context();
    context.tools = vec![pillar_ai::types::Tool {
        name: "preferred".to_string(),
        description: "Preferred constrained tool".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
        }),
        constrained_sampling: Some(pillar_ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: pillar_ai::types::ConstrainedStrictness::Prefer,
        }),
    }];
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(model, context, Some(options)).result().await;
    let body = last_body(&fetch);
    let tool = &body["tools"][0];
    assert!(tool.get("strict").is_none(), "unexpected strict: {tool}");
}

#[tokio::test(flavor = "multi_thread")]
async fn builds_correct_default_url_from_resource_name_env() {
    // Scoped env override standing in for the process environment.
    let fetch = CapturedFetch::always(minimal_sse);
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        env: Some(pillar_ai::ProviderEnv::from([(
            "AZURE_OPENAI_RESOURCE_NAME".to_string(),
            "my-resource".to_string(),
        )])),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    let model = Model {
        base_url: String::new(),
        ..azure_model()
    };
    stream(model, context(), Some(options)).result().await;
    let requests = fetch.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://my-resource.openai.azure.com/openai/v1/responses?api-version=v1"
    );
}

// --- User agent -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn uses_pi_user_agent_by_default() {
    let fetch = CapturedFetch::always(minimal_sse);
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(azure_model(), context(), Some(options))
        .result()
        .await;
    let requests = fetch.requests();
    let user_agent = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
        .map(|(_, value)| value.clone())
        .expect("User-Agent header");
    assert_eq!(user_agent, get_user_agent());
}

#[tokio::test(flavor = "multi_thread")]
async fn lets_explicit_headers_override_the_default_user_agent() {
    let fetch = CapturedFetch::always(minimal_sse);
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        azure_base_url: Some("https://my-resource.openai.azure.com".to_string()),
        headers: Some(pillar_ai::types::ProviderHeaders::from([(
            "User-Agent".to_string(),
            Some("custom-agent".to_string()),
        )])),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(azure_model(), context(), Some(options))
        .result()
        .await;
    let requests = fetch.requests();
    let user_agent = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
        .map(|(_, value)| value.clone())
        .expect("User-Agent header");
    assert_eq!(user_agent, "custom-agent");
}

// --- Deployment name --------------------------------------------------------

#[test]
fn resolves_deployment_name_from_options_over_model_id() {
    let model = azure_model();
    let options = AzureOpenAIResponsesOptions {
        azure_deployment_name: Some("my-deployment".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_deployment_name(&model, Some(&options)),
        "my-deployment"
    );
}

#[test]
fn resolves_deployment_name_from_env_map() {
    let model = Model {
        id: "gpt-4o-mini".to_string(),
        ..azure_model()
    };
    let options = AzureOpenAIResponsesOptions {
        env: Some(pillar_ai::types::ProviderEnv::from([(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".to_string(),
            "gpt-4o=deploy-a, gpt-4o-mini=deploy-b".to_string(),
        )])),
        ..Default::default()
    };
    assert_eq!(resolve_deployment_name(&model, Some(&options)), "deploy-b");
}

#[test]
fn falls_back_to_model_id_when_unmapped() {
    let model = azure_model();
    assert_eq!(resolve_deployment_name(&model, None), "gpt-4o-mini");
}

// --- Tool choice ------------------------------------------------------------

/// onPayload hook that records the payload and replaces it with an empty
/// object so the request fails fast without touching a server.
fn capture_payload(sink: Arc<Mutex<Option<serde_json::Value>>>) -> OnPayloadFn {
    Arc::new(move |_model, request_payload| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            *sink.lock().unwrap() = Some(request_payload);
            Some(serde_json::Value::Object(serde_json::Map::new()))
        })
    })
}

fn tool_context() -> Context {
    let mut context = context();
    context.tools = vec![pillar_ai::types::Tool {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
        }),
        constrained_sampling: None,
    }];
    context
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_provider_specific_tool_choice_preserving_tool_definitions() {
    let fetch = CapturedFetch::always(minimal_sse);
    let payload = Arc::new(Mutex::new(None::<serde_json::Value>));
    let payload_for_hook = Arc::clone(&payload);
    let model = Model {
        id: "test-deployment".to_string(),
        name: "Test Deployment".to_string(),
        base_url: "http://127.0.0.1:9/openai/v1".to_string(),
        reasoning: false,
        ..azure_model()
    };
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-key".to_string()),
        tool_choice: Some(serde_json::json!("required")),
        on_payload: Some(capture_payload(payload_for_hook)),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream(model, tool_context(), Some(options)).result().await;

    let payload = payload.lock().unwrap().clone().expect("payload captured");
    assert_eq!(
        payload.get("tool_choice"),
        Some(&serde_json::json!("required"))
    );
    assert_eq!(
        payload
            .get("tools")
            .and_then(|tools| tools.as_array())
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_provider_neutral_tool_choice_from_simple_options() {
    let fetch = CapturedFetch::always(minimal_sse);
    let payload = Arc::new(Mutex::new(None::<serde_json::Value>));
    let payload_for_hook = Arc::clone(&payload);
    let model = Model {
        id: "test-deployment".to_string(),
        name: "Test Deployment".to_string(),
        base_url: "http://127.0.0.1:9/openai/v1".to_string(),
        reasoning: false,
        ..azure_model()
    };
    let options = SimpleStreamOptions {
        api_key: Some("test-key".to_string()),
        tool_choice: Some(serde_json::json!("none")),
        on_payload: Some(capture_payload(payload_for_hook)),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    stream_simple(model, tool_context(), Some(options))
        .result()
        .await;

    let payload = payload.lock().unwrap().clone().expect("payload captured");
    assert_eq!(payload.get("tool_choice"), Some(&serde_json::json!("none")));
    assert_eq!(
        payload
            .get("tools")
            .and_then(|tools| tools.as_array())
            .map(Vec::len),
        Some(1)
    );
}

// --- Reasoning replay -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn preserves_existing_encrypted_content_from_output_item_done() {
    let events = reasoning_replay_sse("from-output-item-done", "from-output-item-done");
    let events_for_fetch = events.clone();
    let fetch = CapturedFetch::always(move || sse_response(events_for_fetch.clone()));
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    let message = stream(reasoning_model(), context(), Some(options))
        .result()
        .await;

    let reasoning = message
        .content
        .iter()
        .find_map(|block| match block {
            pillar_ai::types::Content::Thinking {
                thinking_signature, ..
            } => Some(thinking_signature),
            _ => None,
        })
        .and_then(|signature| signature.clone())
        .expect("reasoning block replayed");
    let stored: serde_json::Value = serde_json::from_str(&reasoning).unwrap();
    assert_eq!(
        stored
            .get("encrypted_content")
            .and_then(|value| value.as_str()),
        Some("from-output-item-done")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fills_encrypted_content_when_output_item_done_omitted_it() {
    let events = reasoning_replay_sse("", "from-response-completed");
    let events_for_fetch = events.clone();
    let fetch = CapturedFetch::always(move || sse_response(events_for_fetch.clone()));
    let options = AzureOpenAIResponsesOptions {
        api_key: Some("test-api-key".to_string()),
        fetch: Some(fetch.clone()),
        ..Default::default()
    };
    let message = stream(reasoning_model(), context(), Some(options))
        .result()
        .await;

    let reasoning = message
        .content
        .iter()
        .find_map(|block| match block {
            pillar_ai::types::Content::Thinking {
                thinking_signature, ..
            } => Some(thinking_signature),
            _ => None,
        })
        .and_then(|signature| signature.clone())
        .expect("reasoning block replayed");
    let stored: serde_json::Value = serde_json::from_str(&reasoning).unwrap();
    assert_eq!(
        stored
            .get("encrypted_content")
            .and_then(|value| value.as_str()),
        Some("from-response-completed")
    );
}

fn reasoning_model() -> Model {
    Model {
        id: "gpt-5-mini".to_string(),
        name: "GPT-5 Mini".to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: true,
        ..azure_model()
    }
}

/// SSE for the reasoning-replay scenario: output_item.added, output_item.done
/// with `done_encrypted`, then response.completed with `completed_encrypted`.
fn reasoning_replay_sse(done_encrypted: &str, completed_encrypted: &str) -> String {
    let item = |encrypted: &str| {
        serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": if encrypted.is_empty() { serde_json::Value::Null } else { serde_json::json!(encrypted) },
        })
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "reasoning", "id": "rs_1", "summary": [] },
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": item(done_encrypted),
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_test",
                "status": "completed",
                "output": [item(completed_encrypted)],
            },
        }),
    )
}
