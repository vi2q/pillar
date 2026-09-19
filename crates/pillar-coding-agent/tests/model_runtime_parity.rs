//! Parity tests for model-runtime.ts + runtime-credentials.ts (pi v0.84.3):
//! provider composition over builtins, snapshot derivation, credential
//! overlay semantics, runtime API keys, auth status, and prepared requests.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use pillar_ai::auth_types::{ApiKeyCredential, Credential, CredentialInfo, CredentialStore};
use pillar_ai::error::AiError;
use pillar_ai::models::{CreateProviderOptions, ProviderApi};
use pillar_ai::types::{Model, ModelCost, ModelCostRates};
use pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore;
use pillar_coding_agent::core::model_runtime::{
    CreateModelRuntimeOptions, CredentialSynchronizationOperation, ModelRuntime,
};
use pillar_coding_agent::core::provider_composer::{AuthStatusSource, ProviderConfigInput};
use pillar_coding_agent::core::runtime_credentials::RuntimeCredentials;

#[allow(dead_code)]
fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://x.test".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates::default(),
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-runtime-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn runtime_with_empty_models_json(name: &str) -> ModelRuntime {
    let dir = temp_dir(name);
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").unwrap();
    ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap()
}

/// In-memory credential store fixture.
#[derive(Default)]
struct MemCredentials(std::sync::Mutex<BTreeMap<String, Credential>>);

#[async_trait]
impl CredentialStore for MemCredentials {
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        Ok(self.0.lock().unwrap().get(provider_id).cloned())
    }
    async fn list(
        &self,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, AiError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(id, credential)| CredentialInfo {
                provider_id: id.clone(),
                kind: match credential {
                    Credential::ApiKey(_) => "api_key".to_string(),
                    Credential::OAuth(_) => "oauth".to_string(),
                },
            })
            .collect())
    }
    async fn modify(
        &self,
        provider_id: &str,
        f: pillar_ai::auth_types::CredentialModifier<'_>,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        let current = self.0.lock().unwrap().get(provider_id).cloned();
        let next = f(current)
            .await
            .map_err(|e| AiError::Other(e.to_string()))?;
        let mut map = self.0.lock().unwrap();
        if let Some(credential) = next {
            map.insert(provider_id.to_string(), credential);
        }
        Ok(map.get(provider_id).cloned())
    }
    async fn delete(
        &self,
        provider_id: &str,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<(), AiError> {
        self.0.lock().unwrap().remove(provider_id);
        Ok(())
    }
}

// --- creation and composition ---------------------------------------------------

#[test]
fn runtime_composes_builtin_providers() {
    let runtime = runtime_with_empty_models_json("empty");
    let providers = runtime.get_providers();
    assert!(!providers.is_empty(), "builtin providers composed");
    let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"anthropic"));
    assert!(ids.contains(&"openai"));
    // Static catalogs are populated.
    assert!(!runtime.get_models(Some("anthropic")).is_empty());
}

#[test]
fn runtime_models_json_config_overrides_base_url() {
    let dir = temp_dir("override");
    let models_path = dir.join("models.json");
    std::fs::write(
        &models_path,
        r#"{"providers":{"anthropic":{"baseUrl":"https://proxy.test/v1","apiKey":"sk-test"}}}"#,
    )
    .unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap();
    let anthropic = runtime
        .get_model("anthropic", "claude-opus-4-8")
        .expect("default model");
    assert_eq!(anthropic.base_url, "https://proxy.test/v1");
    // Config error is absent.
    assert!(runtime.get_error().is_none());
}

#[test]
fn runtime_invalid_models_json_surfaces_error() {
    let dir = temp_dir("invalid");
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "not json").unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap();
    let error = runtime.get_error().expect("config error");
    assert!(error.contains("models.json"), "{error}");
}

#[test]
fn runtime_invalid_models_json_provider_keeps_base_and_records_error() {
    let dir = temp_dir("badext");
    let models_path = dir.join("models.json");
    // A provider overlay that names nothing concrete is rejected.
    std::fs::write(
        &models_path,
        r#"{"providers":{"anthropic":{"headers":{"X-Nothing":"1"},"apiKey":"sk"}},"providers2":{}}"#,
    )
    .unwrap();
    // Valid but incomplete provider (only headers) is accepted; use a truly
    // broken one: provider with empty models + no other keys.
    std::fs::write(&models_path, r#"{"providers":{"custom-p":{"models":[]}}}"#).unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path.clone()),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap();
    let error = runtime.get_error().expect("composition error");
    assert!(error.contains("custom-p"), "{error}");
    assert!(runtime.get_composition_error("custom-p").is_some());
}

#[test]
fn runtime_delete_provider_when_no_base_no_config_no_extension() {
    let mut runtime = runtime_with_empty_models_json("delete");
    // Unregister an unknown provider id is a no-op.
    runtime.unregister_provider("never-registered");
    assert!(runtime.get_provider("never-registered").is_none());
}

// --- registered providers -----------------------------------------------------------

#[test]
fn register_provider_merges_defined_values() {
    let mut runtime = runtime_with_empty_models_json("merge");
    runtime
        .register_provider(
            "anthropic",
            ProviderConfigInput {
                base_url: Some("https://first.test".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    runtime
        .register_provider(
            "anthropic",
            ProviderConfigInput {
                name: Some("Renamed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    let config = runtime
        .get_registered_provider_config("anthropic")
        .expect("registered");
    // Second registration keeps the first baseUrl and adds the name.
    assert_eq!(config.base_url.as_deref(), Some("https://first.test"));
    assert_eq!(config.name.as_deref(), Some("Renamed"));
    assert_eq!(runtime.get_registered_provider_ids(), vec!["anthropic"]);
}

#[test]
fn register_provider_empty_models_is_accepted() {
    // Upstream treats an empty extension models array as a valid (empty)
    // replacement list, not a structural error.
    let mut runtime = runtime_with_empty_models_json("validate");
    let empty = ProviderConfigInput {
        api: Some("test-api".to_string()),
        base_url: Some("https://x.test".to_string()),
        models: Some(vec![]),
        ..Default::default()
    };
    assert!(runtime.register_provider("x-p", empty).is_ok());
    assert!(runtime.get_registered_provider_config("x-p").is_some());
    assert!(
        (runtime.get_provider("x-p").expect("provider"))
            .models()
            .is_empty()
    );
}

#[test]
fn register_native_provider_rejects_empty_id() {
    let mut runtime = runtime_with_empty_models_json("native");
    let provider = pillar_ai::models::create_provider(CreateProviderOptions {
        id: "  ".to_string(),
        name: None,
        base_url: None,
        headers: None,
        auth: pillar_ai::auth_types::ProviderAuth::default(),
        models: vec![],
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::None,
    });
    // register_native_provider takes ownership; validation rejects the empty id.
    let error = runtime.register_native_provider_arc(provider).unwrap_err();
    assert_eq!(error.0, "Provider id must not be empty.");
}

#[test]
fn unregister_provider_removes_config_and_native() {
    let mut runtime = runtime_with_empty_models_json("unreg");
    runtime
        .register_provider(
            "custom",
            ProviderConfigInput {
                api_key: Some("sk".to_string()),
                base_url: Some("https://c.test".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.unregister_provider("custom");
    assert!(runtime.get_registered_provider_config("custom").is_none());
    assert!(
        !runtime
            .get_registered_provider_ids()
            .contains(&"custom".to_string())
    );
}

// --- runtime credentials --------------------------------------------------------------

#[test]
fn runtime_credentials_overlay_shadows_store() {
    let store = Arc::new(MemCredentials::default());
    let credentials = RuntimeCredentials::new(store.clone());
    assert!(!credentials.has_runtime_api_key("p"));

    credentials.set_runtime_api_key("p", "runtime-key");
    assert!(credentials.has_runtime_api_key("p"));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Read returns the override, not the stored credential.
    let stored = runtime.block_on(store.read("p", None)).unwrap();
    assert!(stored.is_none());
    let read = runtime.block_on(credentials.read("p", None)).unwrap();
    assert_eq!(
        read,
        Some(Credential::ApiKey(ApiKeyCredential {
            key: Some("runtime-key".to_string()),
            env: None,
        }))
    );

    // List merges overrides over stored entries.
    store.0.lock().unwrap().insert(
        "q".to_string(),
        Credential::ApiKey(ApiKeyCredential {
            key: Some("stored".to_string()),
            env: None,
        }),
    );
    let listed = runtime.block_on(credentials.list(None)).unwrap();
    assert_eq!(listed.len(), 2);
    let p_entry = listed.iter().find(|e| e.provider_id == "p").unwrap();
    assert_eq!(p_entry.kind, "api_key");

    // Remove falls back to the stored credential (none for p).
    credentials.remove_runtime_api_key("p");
    assert!(!credentials.has_runtime_api_key("p"));
    let read = runtime.block_on(credentials.read("p", None)).unwrap();
    assert!(read.is_none());

    // Delete removes the override too.
    credentials.set_runtime_api_key("q", "override");
    runtime.block_on(credentials.delete("q", None)).unwrap();
    assert!(!credentials.has_runtime_api_key("q"));
}

// --- auth status ------------------------------------------------------------------------

#[tokio::test]
async fn provider_auth_status_precedence() {
    let dir = temp_dir("status");
    let models_path = dir.join("models.json");
    std::fs::write(
        &models_path,
        r#"{"providers":{"cfg-key":{"baseUrl":"https://x.test","apiKey":"sk-literal"}}}"#,
    )
    .unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        credentials: Some(Arc::new(MemCredentials::default())),
        ..Default::default()
    })
    .unwrap();

    // Runtime key wins.
    runtime
        .set_runtime_api_key("cfg-key", "runtime")
        .await
        .unwrap();
    let status = runtime.get_provider_auth_status("cfg-key");
    assert_eq!(status.source, Some(AuthStatusSource::Runtime));
    runtime.remove_runtime_api_key("cfg-key").await.unwrap();

    // models.json literal key -> models_json_key.
    let status = runtime.get_provider_auth_status("cfg-key");
    assert_eq!(status.source, Some(AuthStatusSource::ModelsJsonKey));

    // Unknown provider -> not configured.
    let status = runtime.get_provider_auth_status("nope");
    assert!(!status.configured);
}

#[test]
fn set_runtime_api_key_synchronizes_snapshot() {
    let dir = temp_dir("sync");
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        credentials: Some(Arc::new(MemCredentials::default())),
        ..Default::default()
    })
    .unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        runtime
            .set_runtime_api_key("anthropic", "sk-runtime")
            .await
            .unwrap();
    });
    // `synchronizeCredentialState` ends with `refreshProviderAvailability`
    // (upstream), so the snapshot is already configured when the call
    // resolves — no separate availability pass is needed.
    assert!(runtime.has_configured_auth("anthropic"));
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.refresh_availability(None))
        .unwrap();
    assert!(runtime.has_configured_auth("anthropic"));
    let available = runtime.get_available_snapshot();
    assert!(
        available.iter().any(|m| m.provider == "anthropic"),
        "anthropic models become available"
    );
}

// --- refresh -----------------------------------------------------------------------------

#[tokio::test]
async fn refresh_reloads_models_json() {
    let dir = temp_dir("refresh");
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path.clone()),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap();
    assert!(runtime.get_model("newprov", "custom").is_none());
    std::fs::write(
        &models_path,
        r#"{"providers":{"newprov":{"baseUrl":"https://n.test","apiKey":"sk","models":[{"id":"custom","api":"test-api","baseUrl":"https://n.test","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":100}]}}}"#,
    )
    .unwrap();
    let result = runtime.refresh(None).await.unwrap();
    assert!(!result.aborted);
    assert!(result.errors.is_empty());
    assert_eq!(
        runtime.get_model("newprov", "custom").unwrap().base_url,
        "https://n.test"
    );
}

// --- misc -----------------------------------------------------------------------------------

#[test]
fn sync_error_operation_strings_match_upstream() {
    assert_eq!(CredentialSynchronizationOperation::Login.as_str(), "login");
    assert_eq!(
        CredentialSynchronizationOperation::Logout.as_str(),
        "logout"
    );
    assert_eq!(
        CredentialSynchronizationOperation::SetRuntimeApiKey.as_str(),
        "setRuntimeApiKey"
    );
    assert_eq!(
        CredentialSynchronizationOperation::RemoveRuntimeApiKey.as_str(),
        "removeRuntimeApiKey"
    );
}

#[test]
fn default_auth_context_available_via_pillar_ai() {
    // The runtime wires DefaultAuthContext when no auth_context is supplied.
    let _ = pillar_ai::auth_context::DefaultAuthContext;
}
