//! Port of the upstream providers/all + models-catalog tests (pi v0.84.3)
//! that are mockable: builtin provider registration, catalog shape, ambient
//! auth resolution, and the dynamic Radius provider placeholder.

#![cfg(feature = "providers")]

use std::collections::BTreeMap;
use std::sync::Arc;

use pillar_ai::api::openrouter_images::{AssistantImages, ImagesModel, ImagesStopReason};
use pillar_ai::auth_types::{AuthContext, CredentialStore};
use pillar_ai::credential_store::InMemoryCredentialStore;
use pillar_ai::images_models::{CreateImagesModelsOptions, ImagesModels};
use pillar_ai::providers_all::builtin_providers;

#[derive(Default)]
struct MapAuthContext {
    env: BTreeMap<String, String>,
}

#[async_trait::async_trait]
impl AuthContext for MapAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }
    async fn file_exists(&self, _path: &str) -> bool {
        false
    }
}

fn auth_context(env: &[(&str, &str)]) -> Arc<dyn AuthContext> {
    let mut map = BTreeMap::new();
    for (name, value) in env {
        map.insert(name.to_string(), value.to_string());
    }
    Arc::new(MapAuthContext { env: map })
}

fn openrouter_image_model() -> ImagesModel {
    ImagesModel {
        id: "google/gemini-2.5-flash-image".to_string(),
        name: "Gemini 2.5 Flash Image".to_string(),
        api: "openrouter-images".to_string(),
        provider: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        input: vec!["text".to_string(), "image".to_string()],
        output: vec!["image".to_string(), "text".to_string()],
        cost: pillar_ai::types::ModelCost::default(),
        headers: None,
    }
}

// --- Tests -----------------------------------------------------------------

#[test]
fn builtin_provider_registry_covers_the_generated_catalog() {
    let providers = builtin_providers();
    let ids: Vec<&str> = providers
        .iter()
        .map(|provider| provider.id.as_str())
        .collect();
    // Upstream providers/all.ts builtinProviders list.
    for expected in [
        "amazon-bedrock",
        "anthropic",
        "azure-openai-responses",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "github-copilot",
        "google",
        "google-vertex",
        "kimi-coding",
        "mistral",
        "openai",
        "openai-codex",
        "openrouter",
        "radius",
        "xai",
    ] {
        assert!(
            ids.contains(&expected),
            "missing builtin provider: {expected} (got {ids:?})"
        );
    }
    // Provider ids are unique.
    let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn builtin_catalog_models_parse_and_match_their_provider() {
    let providers = builtin_providers();
    let mut checked = 0;
    for provider in &providers {
        let models = provider.models();
        for model in &models {
            assert_eq!(model.provider, provider.id, "catalog provider mismatch");
            assert!(!model.id.is_empty());
            assert!(model.max_tokens > 0);
            checked += 1;
        }
    }
    assert!(checked > 100, "catalog unexpectedly small: {checked}");
}

#[test]
fn openai_catalog_models_speak_openai_apis() {
    let providers = builtin_providers();
    let openai = providers
        .iter()
        .find(|provider| provider.id == "openai")
        .expect("openai builtin");
    let models = openai.models();
    assert!(!models.is_empty());
    assert!(models.iter().all(|model| model.api == "openai-responses"));
    // Codex models come from the static catalog.
    let codex = providers
        .iter()
        .find(|provider| provider.id == "openai-codex")
        .expect("openai-codex builtin");
    let codex_models = codex.models();
    let codex_ids: Vec<String> = codex_models.iter().map(|model| model.id.clone()).collect();
    assert!(
        codex_ids.iter().any(|id| id == "gpt-5.5") || codex_ids.iter().any(|id| id == "gpt-5.4"),
        "codex catalog missing pinned ids: {codex_ids:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_images_models_resolves_openrouter_auth_from_env() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = ImagesModels::new(CreateImagesModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        auth_context: Some(auth_context(&[("OPENROUTER_API_KEY", "or-key")])),
    });

    // Register the builtin openrouter images provider equivalent.
    let provider = pillar_ai::images_models::ImagesProvider {
        id: "openrouter".to_string(),
        name: "OpenRouter".to_string(),
        auth: pillar_ai::auth_types::ProviderAuth {
            api_key: Some(Arc::new(pillar_ai::providers_all::EnvApiKeyAuth {
                name: "OpenRouter API key",
                env_vars: &["OPENROUTER_API_KEY"],
            })),
            oauth: None,
        },
        get_models: Box::new(|| vec![openrouter_image_model()]),
        refresh_models: None,
        api: pillar_ai::images_models::ImagesApiImpl {
            generate_images: Some(Arc::new(|model, _context, _options| {
                Box::pin(async move {
                    AssistantImages {
                        api: model.api.clone(),
                        provider: model.provider.clone(),
                        model: model.id.clone(),
                        output: Vec::new(),
                        response_id: None,
                        usage: None,
                        stop_reason: ImagesStopReason::Stop,
                        error_message: None,
                        timestamp: 1,
                    }
                })
            })),
        },
    };
    models.set_provider(Arc::new(provider));

    let list = models.get_models(Some("openrouter"));
    assert!(!list.is_empty());
    assert!(list.iter().all(|model| model.api == "openrouter-images"));

    let resolved = models
        .get_auth("openrouter", None)
        .await
        .expect("auth resolves")
        .expect("configured via env");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("or-key"));
}

// --- Built-in api-key login flows ------------------------------------------
//
// Port of the `login` halves of packages/ai/src/auth/helpers.ts
// (`envApiKeyAuth`), providers/anthropic.ts, cloudflare-auth.ts,
// amazon-bedrock.ts and google-vertex.ts. The prompt-driven flows are run
// through `Models.login`, so the stored credential is what the flow really
// produced (upstream: `models.login(...)`).

use std::collections::VecDeque;
use std::sync::Mutex;

use pillar_ai::abort::{AbortReason, AbortSignal};
use pillar_ai::auth_types::{ApiKeyCredential, AuthEvent, AuthInteraction, AuthPrompt, Credential};
use pillar_ai::models::{CreateModelsOptions, Models, create_models};
use pillar_ai::providers_all::provider_auth_result;

/// Answers a login flow's prompts from a script and records what it saw.
struct ScriptedInteraction {
    answers: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<AuthPrompt>>,
    events: Mutex<Vec<AuthEvent>>,
    signal: Option<AbortSignal>,
    /// Abort the flow from inside `prompt`, i.e. after the flow started.
    abort_on_prompt: bool,
}

impl ScriptedInteraction {
    fn new(answers: &[&str]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().map(|a| a.to_string()).collect()),
            prompts: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            signal: None,
            abort_on_prompt: false,
        }
    }

    fn aborting(answers: &[&str]) -> Self {
        Self {
            abort_on_prompt: true,
            ..Self::new(answers)
        }
    }

    fn prompts(&self) -> Vec<AuthPrompt> {
        self.prompts.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<AuthEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AuthInteraction for ScriptedInteraction {
    fn signal(&self) -> Option<&AbortSignal> {
        self.signal.as_ref()
    }

    async fn prompt(&self, prompt: &AuthPrompt) -> Result<String, pillar_ai::error::AiError> {
        self.prompts.lock().unwrap().push(prompt.clone());
        if self.abort_on_prompt {
            if let Some(signal) = &self.signal {
                signal.abort(Some(AbortReason::Aborted));
            }
        }
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| pillar_ai::error::AiError::Other("no scripted answer".to_string()))
    }

    async fn notify(&self, event: &AuthEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

/// The builtin providers over a fresh credential store.
fn builtin_models_with_store() -> (Models, Arc<InMemoryCredentialStore>) {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        auth_context: Some(auth_context(&[])),
        ..Default::default()
    });
    for provider in builtin_providers() {
        models.set_provider(provider);
    }
    (models, credentials)
}

fn api_key_login() -> String {
    "api_key".to_string()
}

// "envApiKeyAuth prompts for the key and stores it"
#[tokio::test(flavor = "multi_thread")]
async fn env_api_key_login_prompts_for_the_key_and_stores_it() {
    let (models, credentials) = builtin_models_with_store();
    let interaction = ScriptedInteraction::new(&["sk-test"]);

    let credential = models
        .login("openai", &api_key_login(), &interaction)
        .await
        .expect("login succeeds");

    assert_eq!(
        interaction.prompts(),
        vec![AuthPrompt::Secret {
            message: "Enter OpenAI API key".to_string(),
            placeholder: None,
        }]
    );
    assert_eq!(
        credential,
        Credential::ApiKey(ApiKeyCredential {
            key: Some("sk-test".to_string()),
            env: None,
        })
    );
    assert_eq!(
        credentials.read("openai", None).await.unwrap(),
        Some(credential)
    );
}

// "an abort during the prompt rejects the login and stores nothing"
#[tokio::test(flavor = "multi_thread")]
async fn an_abort_after_the_prompt_rejects_the_login() {
    let (models, credentials) = builtin_models_with_store();
    let controller = AbortSignal::new();
    let interaction = ScriptedInteraction {
        signal: Some(controller.clone()),
        ..ScriptedInteraction::aborting(&["sk-test"])
    };

    let result = models.login("openai", &api_key_login(), &interaction).await;
    assert!(result.is_err(), "an aborted login must not resolve");
    assert_eq!(credentials.read("openai", None).await.unwrap(), None);
}

// "anthropic's api-key auth is not the shared envApiKeyAuth"
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_auth_reads_its_documented_env_vars() {
    let credentials = InMemoryCredentialStore::new();
    let anthropic = builtin_providers()
        .into_iter()
        .find(|provider| provider.id == "anthropic")
        .expect("anthropic is a builtin provider");

    // A bare ANTHROPIC_AUTH_TOKEN is a bearer credential, and it wins over
    // the api-key env vars.
    let resolved = provider_auth_result(
        &anthropic,
        &credentials,
        &*auth_context(&[
            ("ANTHROPIC_AUTH_TOKEN", "bearer"),
            ("ANTHROPIC_API_KEY", "env-key"),
        ]),
    )
    .await
    .unwrap()
    .expect("configured");
    assert_eq!(
        resolved
            .auth
            .headers
            .as_ref()
            .and_then(|headers| headers.get("Authorization"))
            .and_then(|value| value.clone())
            .as_deref(),
        Some("Bearer bearer")
    );
    assert_eq!(resolved.auth.api_key, None);
    assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));

    // The api-key env vars are api keys.
    let resolved = provider_auth_result(
        &anthropic,
        &credentials,
        &*auth_context(&[("ANTHROPIC_OAUTH_TOKEN", "oauth-key")]),
    )
    .await
    .unwrap()
    .expect("configured");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("oauth-key"));
    assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_OAUTH_TOKEN"));

    // Regression: the port used to look up the *constant names* as env vars,
    // which left the whole provider ambient-only.
    let resolved = provider_auth_result(
        &anthropic,
        &credentials,
        &*auth_context(&[
            ("ANTHROPIC_API_KEY_ENV", "key"),
            ("ANTHROPIC_OAUTH_TOKEN_ENV", "token"),
        ]),
    )
    .await
    .unwrap();
    assert!(resolved.is_none(), "placeholder env names must not resolve");
}

// "anthropic's login is the same secret prompt"
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_login_prompts_for_the_key() {
    let (models, credentials) = builtin_models_with_store();
    let interaction = ScriptedInteraction::new(&["sk-ant"]);

    let credential = models
        .login("anthropic", &api_key_login(), &interaction)
        .await
        .expect("login succeeds");

    assert_eq!(
        interaction.prompts(),
        vec![AuthPrompt::Secret {
            message: "Enter Anthropic API key".to_string(),
            placeholder: None,
        }]
    );
    assert_eq!(
        credential,
        Credential::ApiKey(ApiKeyCredential {
            key: Some("sk-ant".to_string()),
            env: None,
        })
    );
    assert_eq!(
        credentials.read("anthropic", None).await.unwrap(),
        Some(credential)
    );
}

// "cloudflare login collects the account id, and the gateway id too"
#[tokio::test(flavor = "multi_thread")]
async fn cloudflare_login_collects_the_account_and_gateway_ids() {
    let (models, credentials) = builtin_models_with_store();

    let workers_ai = ScriptedInteraction::new(&["cf-key", "acct"]);
    models
        .login("cloudflare-workers-ai", &api_key_login(), &workers_ai)
        .await
        .expect("login succeeds");
    assert_eq!(
        workers_ai.prompts(),
        vec![
            AuthPrompt::Secret {
                message: "Enter Cloudflare API key".to_string(),
                placeholder: None,
            },
            AuthPrompt::Text {
                message: "Enter Cloudflare account ID".to_string(),
                placeholder: None,
            },
        ]
    );
    assert_eq!(
        credentials
            .read("cloudflare-workers-ai", None)
            .await
            .unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("cf-key".to_string()),
            env: Some(BTreeMap::from([(
                "CLOUDFLARE_ACCOUNT_ID".to_string(),
                "acct".to_string(),
            )])),
        }))
    );

    let gateway = ScriptedInteraction::new(&["cf-key", "acct", "gw"]);
    models
        .login("cloudflare-ai-gateway", &api_key_login(), &gateway)
        .await
        .expect("login succeeds");
    assert_eq!(gateway.prompts().len(), 3);
    assert_eq!(
        credentials
            .read("cloudflare-ai-gateway", None)
            .await
            .unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("cf-key".to_string()),
            env: Some(BTreeMap::from([
                ("CLOUDFLARE_ACCOUNT_ID".to_string(), "acct".to_string()),
                ("CLOUDFLARE_GATEWAY_ID".to_string(), "gw".to_string()),
            ])),
        }))
    );
}

// "bedrock login stores the bearer token, the profile, or nothing"
#[tokio::test(flavor = "multi_thread")]
async fn bedrock_login_follows_the_selected_method() {
    let (models, credentials) = builtin_models_with_store();

    let bearer = ScriptedInteraction::new(&["bearer-token", "bedrock-token"]);
    models
        .login("amazon-bedrock", &api_key_login(), &bearer)
        .await
        .expect("login succeeds");
    assert_eq!(
        credentials.read("amazon-bedrock", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("bedrock-token".to_string()),
            env: None,
        }))
    );
    assert!(
        bearer.events().is_empty(),
        "the bearer path notifies nothing"
    );

    let profile = ScriptedInteraction::new(&["aws-profile", "my-profile"]);
    models
        .login("amazon-bedrock", &api_key_login(), &profile)
        .await
        .expect("login succeeds");
    assert_eq!(
        credentials.read("amazon-bedrock", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: None,
            env: Some(BTreeMap::from([(
                "AWS_PROFILE".to_string(),
                "my-profile".to_string(),
            )])),
        }))
    );
    assert_eq!(
        profile.events().len(),
        1,
        "the ambient paths explain themselves"
    );

    let chain = ScriptedInteraction::new(&["credential-chain", ""]);
    models
        .login("amazon-bedrock", &api_key_login(), &chain)
        .await
        .expect("login succeeds");
    assert_eq!(
        credentials.read("amazon-bedrock", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential::default()))
    );

    let unknown = ScriptedInteraction::new(&["nonsense"]);
    assert!(
        models
            .login("amazon-bedrock", &api_key_login(), &unknown)
            .await
            .is_err(),
        "an unknown method must not store a credential"
    );
}

// "vertex login collects project, location and the service account path"
#[tokio::test(flavor = "multi_thread")]
async fn vertex_login_collects_the_adc_inputs() {
    let (models, credentials) = builtin_models_with_store();

    let api_key = ScriptedInteraction::new(&["api-key", "gcp-key"]);
    models
        .login("google-vertex", &api_key_login(), &api_key)
        .await
        .expect("login succeeds");
    assert_eq!(
        credentials.read("google-vertex", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("gcp-key".to_string()),
            env: None,
        }))
    );

    let service_account =
        ScriptedInteraction::new(&["service-account", "proj", "us-central1", "/tmp/sa.json"]);
    models
        .login("google-vertex", &api_key_login(), &service_account)
        .await
        .expect("login succeeds");
    assert_eq!(
        credentials.read("google-vertex", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential {
            key: None,
            env: Some(BTreeMap::from([
                ("GOOGLE_CLOUD_PROJECT".to_string(), "proj".to_string()),
                (
                    "GOOGLE_CLOUD_LOCATION".to_string(),
                    "us-central1".to_string()
                ),
                (
                    "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
                    "/tmp/sa.json".to_string(),
                ),
            ])),
        }))
    );
    assert_eq!(service_account.events().len(), 1);
}
