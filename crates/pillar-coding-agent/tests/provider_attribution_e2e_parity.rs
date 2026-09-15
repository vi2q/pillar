//! Parity test for the provider attribution wiring (pi v0.84.3
//! `core/sdk.ts` streamFn `transformHeaders` + `core/provider-attribution.ts`):
//! a session talking to an OpenCode-hosted provider sends the session
//! routing headers, which the port previously dropped because the session
//! stream function never passed a header transform.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_ai::auth_types::{ApiKeyAuth, ApiKeyAuthInput, AuthResult, ModelAuth, ProviderAuth};
use pillar_ai::error::AiError;
use pillar_ai::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use pillar_ai::models::{Provider, ProviderApi, ProviderStreams, StreamFn, StreamRequestOptions};
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, Model, ProviderHeaders, StopReason,
    Usage,
};
use pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore;
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::sdk::{CreateAgentSessionOptions, create_agent_session};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

/// API-key auth that always resolves, so the runtime never needs a stored
/// credential.
struct StaticKeyAuth;

#[async_trait]
impl ApiKeyAuth for StaticKeyAuth {
    fn name(&self) -> &str {
        "Test API key"
    }
    async fn resolve(&self, _input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            env: None,
            source: Some("test".to_string()),
        }))
    }
}

type Captured = Arc<Mutex<Option<ProviderHeaders>>>;

/// Stream fn that records the request headers and answers with one text block.
fn capturing_stream(captured: Captured) -> StreamFn {
    Arc::new(
        move |model: &Model, _context: &Context, options: &StreamRequestOptions| {
            *captured.lock().expect("captured") = options.headers.clone();
            let stream: AssistantMessageEventStream = assistant_message_event_stream();
            let message = AssistantMessage {
                content: vec![Content::text("ok")],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1,
            };
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message,
            });
            stream
        },
    )
}

fn opencode_model() -> Model {
    Model {
        id: "omen-alpha".to_string(),
        name: "omen-alpha".to_string(),
        api: "openai-completions".to_string(),
        provider: "opencode-go".to_string(),
        base_url: "https://opencode.ai/zen/go/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 500_000,
        max_tokens: 32_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pillar-attribution-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn opencode_session_requests_carry_the_session_headers() {
    let captured: Captured = Arc::new(Mutex::new(None));
    let model = opencode_model();
    let dir = temp_dir("opencode");
    std::fs::write(dir.join("models.json"), "{}").unwrap();
    std::fs::write(dir.join("settings.json"), "{}").unwrap();

    let mut runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        auth_path: Some(dir.join("auth.json")),
        models_path: Some(dir.join("models.json")),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap();
    runtime
        .register_native_provider(Provider {
            id: "opencode-go".to_string(),
            name: "OpenCode Go".to_string(),
            base_url: Some(model.base_url.clone()),
            headers: None,
            auth: ProviderAuth {
                api_key: Some(Arc::new(StaticKeyAuth)),
                oauth: None,
            },
            get_models: Box::new({
                let model = model.clone();
                move || vec![model.clone()]
            }),
            refresh_models: None,
            filter_models: None,
            api: ProviderApi::Single(Arc::new(ProviderStreams {
                stream: capturing_stream(Arc::clone(&captured)),
                stream_simple: capturing_stream(Arc::clone(&captured)),
            })),
        })
        .unwrap();
    runtime.refresh_availability(None).await.unwrap();
    let runtime = Arc::new(runtime);

    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({ "retry": { "enabled": false } }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let session_manager = SessionManager::in_memory("", None).unwrap();
    let session_id = session_manager.session_id().to_string();
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: dir.to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: dir.to_string_lossy().to_string(),
        agent_dir: Some(dir.to_string_lossy().to_string()),
        model_runtime: runtime,
        settings_manager: Some(settings_manager),
        session_manager: Some(session_manager),
        resource_loader: Some(resource_loader),
        model: Some(model),
        thinking_level: None,
        scoped_models: Vec::new(),
        tools: Some(Vec::new()),
        no_tools: None,
        exclude_tools: Vec::new(),
        custom_tools: Vec::new(),
        extension_runner: Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
        session_start_event: None,
        system_prompt_rebuild: None,
        extension_runner_rebuild: None,
        stream_fn: None,
    })
    .await
    .expect("session created");

    created
        .session
        .prompt("hi", None)
        .await
        .expect("prompt succeeds");

    let headers = captured
        .lock()
        .expect("captured")
        .clone()
        .unwrap_or_default();
    assert_eq!(
        headers
            .get("x-opencode-session")
            .and_then(|value| value.as_deref()),
        Some(session_id.as_str()),
        "{headers:?}"
    );
    assert_eq!(
        headers
            .get("x-opencode-client")
            .and_then(|value| value.as_deref()),
        Some("pi"),
        "{headers:?}"
    );
}
