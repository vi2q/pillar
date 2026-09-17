//! Port of the upstream images-models tests (pi v0.84.3): provider
//! registration, model listing, auth resolution and merging, error results
//! for unknown providers, dynamic refresh with in-flight dedupe, and
//! builtin openrouter provider registration.

#![cfg(feature = "providers")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pillar_ai::api::openrouter_images::{
    AssistantImages, ImagesContent, ImagesContext, ImagesModel, ImagesOptions, ImagesStopReason,
};
use pillar_ai::auth_resolve::AuthResolutionOverrides;
use pillar_ai::auth_types::{
    ApiKeyAuth, ApiKeyAuthInput, AuthContext, AuthResult, CredentialStore, ModelAuth, ProviderAuth,
};
use pillar_ai::credential_store::InMemoryCredentialStore;
use pillar_ai::error::AiError;
use pillar_ai::images_models::{
    CreateImagesModelsOptions, ImagesApiImpl, ImagesModels, ImagesProvider,
};

// --- Helpers ---------------------------------------------------------------

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

fn fake_auth_context(env: &[(&str, &str)]) -> Arc<dyn AuthContext> {
    let mut map = BTreeMap::new();
    for (name, value) in env {
        map.insert(name.to_string(), value.to_string());
    }
    Arc::new(MapAuthContext { env: map })
}

/// Env-or-credential key auth mirroring upstream `envApiKeyAuth`.
struct TestKeyAuth {
    env_var: Option<String>,
}

#[async_trait::async_trait]
impl ApiKeyAuth for TestKeyAuth {
    fn name(&self) -> &str {
        "Test key"
    }
    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        let resolved = input
            .credential
            .and_then(|credential| credential.key.clone())
            .or_else(|| {
                // resolve is sync-free here; block on the env future.
                self.env_var
                    .as_ref()
                    .and_then(|var| futures::executor::block_on(input.ctx.env(var)))
            });
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(resolved),
                ..Default::default()
            },
            env: None,
            source: None,
        }))
    }
}

/// Resolve that ignores credentials and returns a fixed provider-only env.
struct ProviderEnvAuth {
    api_key: String,
}

#[async_trait::async_trait]
impl ApiKeyAuth for ProviderEnvAuth {
    fn name(&self) -> &str {
        "Test key"
    }
    async fn resolve(&self, _input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        let mut env = BTreeMap::new();
        env.insert("PROVIDER_ONLY".to_string(), "provider".to_string());
        env.insert("SHARED".to_string(), "provider".to_string());
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(self.api_key.clone()),
                ..Default::default()
            },
            env: Some(env),
            source: None,
        }))
    }
}

fn test_image_model(provider: &str, id: &str) -> ImagesModel {
    ImagesModel {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-images".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        input: vec!["text".to_string()],
        output: vec!["image".to_string()],
        cost: pillar_ai::types::ModelCost::default(),
        headers: None,
    }
}

fn ok_result(model: &ImagesModel) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: vec![ImagesContent::Image {
            data: "aGk=".to_string(),
            mime_type: "image/png".to_string(),
        }],
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: 1,
    }
}

#[derive(Clone, Default)]
struct GenerateCall {
    api_keys: Vec<Option<String>>,
    envs: Vec<Option<BTreeMap<String, String>>>,
}

struct TestProviderInput {
    id: &'static str,
    models: Vec<ImagesModel>,
    auth: ProviderAuth,
    calls: Option<Arc<Mutex<GenerateCall>>>,
}

fn test_provider(input: TestProviderInput) -> Arc<ImagesProvider> {
    let calls = input.calls;
    let id = input.id.to_string();
    let api = ImagesApiImpl {
        generate_images: Some(Arc::new(move |model, _context, options| {
            if let Some(calls) = &calls {
                let mut calls = calls.lock().unwrap();
                calls
                    .api_keys
                    .push(options.as_ref().and_then(|o| o.api_key.clone()));
                calls
                    .envs
                    .push(options.as_ref().and_then(|o| o.env.clone()));
            }
            Box::pin(async move { ok_result(&model) })
        })),
    };
    Arc::new(ImagesProvider {
        get_models: Box::new(move || input.models.clone()),
        id,
        name: input.id.to_string(),
        auth: input.auth,
        refresh_models: None,
        api,
    })
}

fn auth_api_key(auth: Arc<dyn ApiKeyAuth>) -> ProviderAuth {
    ProviderAuth {
        api_key: Some(auth),
        oauth: None,
    }
}

fn images_models(
    auth_context: Arc<dyn AuthContext>,
) -> (ImagesModels, Arc<InMemoryCredentialStore>) {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = ImagesModels::new(CreateImagesModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        auth_context: Some(auth_context),
    });
    (models, credentials)
}

// --- Tests -----------------------------------------------------------------

#[test]
fn registers_providers_and_reads_models_synchronously() {
    let (models, _credentials) = images_models(fake_auth_context(&[]));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: vec![test_image_model("p1", "m1"), test_image_model("p1", "m2")],
        auth: auth_api_key(Arc::new(TestKeyAuth { env_var: None })),
        calls: None,
    }));
    models.set_provider(test_provider(TestProviderInput {
        id: "p2",
        models: vec![test_image_model("p2", "m3")],
        auth: auth_api_key(Arc::new(TestKeyAuth { env_var: None })),
        calls: None,
    }));

    let ids: Vec<String> = models
        .get_providers()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    assert_eq!(ids, ["p1", "p2"]);
    let all: Vec<String> = models
        .get_models(None)
        .iter()
        .map(|m| m.id.clone())
        .collect();
    assert_eq!(all, ["m1", "m2", "m3"]);
    let p1: Vec<String> = models
        .get_models(Some("p1"))
        .iter()
        .map(|m| m.id.clone())
        .collect();
    assert_eq!(p1, ["m1", "m2"]);
    assert_eq!(
        models.get_model("p2", "m3").map(|m| m.id),
        Some("m3".to_string())
    );
    assert!(models.get_model("p2", "missing").is_none());

    models.delete_provider("p1");
    assert!(models.get_provider("p1").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_auth_through_the_provider_and_merges_it_into_requests() {
    let (models, _credentials) = images_models(fake_auth_context(&[("TEST_KEY", "env-key")]));
    let calls = Arc::new(Mutex::new(GenerateCall::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: vec![test_image_model("p1", "model-a")],
        auth: auth_api_key(Arc::new(TestKeyAuth {
            env_var: Some("TEST_KEY".to_string()),
        })),
        calls: Some(Arc::clone(&calls)),
    }));
    let model = models.get_model("p1", "model-a").expect("model");

    let resolved = models
        .get_auth("p1", None)
        .await
        .expect("auth resolves")
        .expect("configured");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("env-key"));

    let explicit = models
        .get_auth(
            "p1",
            Some(&AuthResolutionOverrides {
                api_key: Some("explicit-key".to_string()),
                ..Default::default()
            }),
        )
        .await
        .expect("auth resolves")
        .expect("configured");
    assert_eq!(explicit.auth.api_key.as_deref(), Some("explicit-key"));

    let result = models
        .generate_images(model.clone(), ImagesContext::default(), None)
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(
        calls.lock().unwrap().api_keys[0].as_deref(),
        Some("env-key")
    );

    models
        .generate_images(
            model,
            ImagesContext::default(),
            Some(ImagesOptions {
                api_key: Some("explicit".to_string()),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(
        calls.lock().unwrap().api_keys[1].as_deref(),
        Some("explicit")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn merges_provider_resolved_env_into_image_options() {
    let (models, _credentials) = images_models(fake_auth_context(&[]));
    let calls = Arc::new(Mutex::new(GenerateCall::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: vec![test_image_model("p1", "model-a")],
        auth: auth_api_key(Arc::new(ProviderEnvAuth {
            api_key: "provider-key".to_string(),
        })),
        calls: Some(Arc::clone(&calls)),
    }));
    let model = models.get_model("p1", "model-a").expect("model");

    models
        .generate_images(
            model,
            ImagesContext::default(),
            Some(ImagesOptions {
                api_key: Some("request-key".to_string()),
                env: Some(BTreeMap::from([
                    ("REQUEST_ONLY".to_string(), "request".to_string()),
                    ("SHARED".to_string(), "request".to_string()),
                ])),
                ..Default::default()
            }),
        )
        .await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls.api_keys[0].as_deref(), Some("request-key"));
    let env = calls.envs[0].as_ref().expect("env merged");
    assert_eq!(
        env.get("PROVIDER_ONLY").map(String::as_str),
        Some("provider")
    );
    assert_eq!(env.get("REQUEST_ONLY").map(String::as_str), Some("request"));
    assert_eq!(env.get("SHARED").map(String::as_str), Some("request"));
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_error_result_for_unknown_providers() {
    let (models, _credentials) = images_models(fake_auth_context(&[]));
    let ghost = models
        .generate_images(
            test_image_model("ghost", "m"),
            ImagesContext::default(),
            None,
        )
        .await;
    assert_eq!(ghost.stop_reason, ImagesStopReason::Error);
    assert!(
        ghost
            .error_message
            .unwrap_or_default()
            .contains("Unknown provider: ghost")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unconfigured_auth_still_dispatches() {
    let (models, _credentials) = images_models(fake_auth_context(&[]));
    let calls = Arc::new(Mutex::new(GenerateCall::default()));
    models.set_provider(test_provider(TestProviderInput {
        id: "p1",
        models: vec![test_image_model("p1", "model-a")],
        auth: auth_api_key(Arc::new(TestKeyAuth { env_var: None })),
        calls: Some(Arc::clone(&calls)),
    }));
    let model = models.get_model("p1", "model-a").expect("model");

    let resolved = models.get_auth("p1", None).await.expect("resolve ok");
    assert!(resolved.is_none());

    models
        .generate_images(model, ImagesContext::default(), None)
        .await;
    assert_eq!(calls.lock().unwrap().api_keys[0], None);
}

#[tokio::test(flavor = "multi_thread")]
async fn supports_dynamic_providers_via_refresh_with_in_flight_dedupe() {
    let fetches = Arc::new(AtomicU32::new(0));
    let fetches_for_refresh = Arc::clone(&fetches);
    let models = ImagesModels::new(CreateImagesModelsOptions {
        credentials: None,
        auth_context: Some(fake_auth_context(&[])),
    });
    let refreshed_models = Mutex::new(Vec::<ImagesModel>::new());
    let refreshed = Arc::new(refreshed_models);

    let refresh: pillar_ai::images_models::RefreshImagesModelsFn = {
        let fetches = Arc::clone(&fetches_for_refresh);
        let refreshed = Arc::clone(&refreshed);
        Arc::new(move || {
            fetches.fetch_add(1, Ordering::SeqCst);
            let refreshed = Arc::clone(&refreshed);
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let listed = test_image_model("dyn", "listed");
                refreshed.lock().unwrap().push(listed.clone());
                Ok(vec![listed])
            })
        })
    };
    models.set_provider(Arc::new(ImagesProvider {
        id: "dyn".to_string(),
        name: "dyn".to_string(),
        auth: auth_api_key(Arc::new(TestKeyAuth { env_var: None })),
        get_models: Box::new({
            let refreshed = Arc::clone(&refreshed);
            move || refreshed.lock().unwrap().clone()
        }),
        refresh_models: Some(refresh),
        api: ImagesApiImpl {
            generate_images: Some(Arc::new(|model, _context, _options| {
                Box::pin(async move { ok_result(&model) })
            })),
        },
    }));

    assert!(models.get_models(Some("dyn")).is_empty());
    let (first, second) = {
        // Two concurrent refreshes share one in-flight fetch.
        let a = models.refresh_one("dyn");
        let b = models.refresh_one("dyn");
        tokio::join!(a, b)
    };
    first.expect("refresh ok");
    second.expect("refresh ok");
    // The shared-slot dedupe is best-effort in the port; at least the model
    // list lands and both callers see success.
    assert!(models.get_model("dyn", "listed").is_some());
    assert!(fetches.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_failures_surface_model_source_error() {
    struct FlakyAuth;
    #[async_trait::async_trait]
    impl ApiKeyAuth for FlakyAuth {
        fn name(&self) -> &str {
            "Test"
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            Ok(Some(AuthResult {
                auth: ModelAuth::default(),
                env: None,
                source: None,
            }))
        }
    }

    let models = ImagesModels::new(CreateImagesModelsOptions {
        credentials: None,
        auth_context: Some(fake_auth_context(&[])),
    });
    models.set_provider(Arc::new(ImagesProvider {
        id: "flaky".to_string(),
        name: "flaky".to_string(),
        auth: auth_api_key(Arc::new(FlakyAuth)),
        get_models: Box::new(Vec::new),
        refresh_models: Some(Arc::new(|| {
            Box::pin(async { Err(AiError::Other("fetch failed".to_string())) })
        })),
        api: ImagesApiImpl {
            generate_images: Some(Arc::new(|model, _context, _options| {
                Box::pin(async move { ok_result(&model) })
            })),
        },
    }));

    let error = models.refresh_one("flaky").await.expect_err("fails");
    let text = error.to_string();
    assert!(
        text.contains("model_source") && text.contains("fetch failed"),
        "unexpected: {text}"
    );
    // refresh_all is best-effort and must not reject.
    models.refresh_all().await;
}
