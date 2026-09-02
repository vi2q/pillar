//! Port of the upstream providers/all + models-catalog tests (pi v0.84.3)
//! that are mockable: builtin provider registration, catalog shape, ambient
//! auth resolution, and the dynamic Radius provider placeholder.

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
