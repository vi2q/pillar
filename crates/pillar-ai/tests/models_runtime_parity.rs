#![cfg(feature = "providers")]

#![allow(clippy::type_complexity)]
//! Port of packages/ai/test/models-runtime.test.ts (pi v0.84.3) — runtime
//! behavior of the `Models` collection: registry, refresh, publication
//! chains, auth surface, and stream convenience. One Rust test per upstream
//! test case, same names in comments.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use pillar_ai::abort::AbortSignal;
use pillar_ai::auth_types::{
    ApiKeyAuth, ApiKeyAuthInput, ApiKeyCredential, AuthCheck, AuthEvent, AuthInteraction,
    AuthOperationOptions, AuthPrompt, AuthResult, Credential, CredentialInfo, CredentialStore,
    ModelAuth, OAuthAuth, OAuthCredential, ProviderAuth, ProviderAuthInteraction,
};
use pillar_ai::credential_store::InMemoryCredentialStore;
use pillar_ai::error::AiError;
use pillar_ai::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use pillar_ai::models::{
    AuthTarget, BoxFuture, CreateProviderOptions, FetchModelsFn, HeadersTransform,
    ModelsPublication, ModelsRefreshOptions, ModelsStreamOptions, Provider, ProviderApi,
    ProviderStreams, RefreshModelsContext, RefreshModelsFn, StreamFn, StreamRequestOptions,
    calculate_cost, create_models, create_provider, has_api,
};
use pillar_ai::models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions,
};
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, Message, Model, ModelCost,
    ModelCostTier, ProviderEnv, ProviderHeaders, StopReason, Usage, UserContent,
};

// --- Test fixtures -------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn test_model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.test/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 10000,
        max_tokens: 1000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn done_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
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
        timestamp: now_ms(),
    }
}

#[derive(Clone)]
struct ProviderCall {
    model: Model,
    options: StreamRequestOptions,
}

type Calls = Arc<Mutex<Vec<ProviderCall>>>;

fn respond(
    model: &Model,
    options: &StreamRequestOptions,
    calls: &Calls,
) -> AssistantMessageEventStream {
    calls.lock().unwrap().push(ProviderCall {
        model: model.clone(),
        options: options.clone(),
    });
    let stream = assistant_message_event_stream();
    let message = done_message(model);
    stream.push(AssistantMessageEvent::Start {
        partial: message.clone(),
    });
    stream.push(AssistantMessageEvent::Done {
        reason: StopReason::Stop,
        message: message.clone(),
    });
    stream.end(Some(message));
    stream
}

fn recording_stream_fn(calls: &Calls) -> StreamFn {
    let calls = Arc::clone(calls);
    Arc::new(
        move |model: &Model, _context: &Context, options: &StreamRequestOptions| {
            respond(model, options, &calls)
        },
    )
}

fn empty_stream_fn() -> StreamFn {
    Arc::new(
        |_model: &Model, _context: &Context, _options: &StreamRequestOptions| {
            assistant_message_event_stream()
        },
    )
}

#[derive(Default)]
struct TestProvider {
    id: String,
    models: Option<Vec<Model>>,
    auth: Option<ProviderAuth>,
    get_models: Option<Box<dyn Fn() -> Vec<Model> + Send + Sync>>,
    refresh_models: Option<RefreshModelsFn>,
    calls: Option<Calls>,
}

fn test_provider(input: TestProvider) -> Arc<Provider> {
    let id = input.id.clone();
    let calls = input
        .calls
        .clone()
        .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
    let get_models = input.get_models.unwrap_or_else(|| {
        let models = input
            .models
            .unwrap_or_else(|| vec![test_model(&id, "model-a")]);
        Box::new(move || models.clone())
    });
    let stream = recording_stream_fn(&calls);
    let stream_simple = Arc::clone(&stream);
    Arc::new(Provider {
        id: id.clone(),
        name: id.clone(),
        base_url: None,
        headers: None,
        auth: input
            .auth
            .unwrap_or_else(|| auth_api_key(Arc::new(AmbientAuth))),
        get_models,
        refresh_models: input.refresh_models,
        filter_models: None,
        api: ProviderApi::Single(Arc::new(ProviderStreams {
            stream,
            stream_simple,
        })),
    })
}

fn test_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User {
            content: UserContent::Text("hi".to_string()),
            timestamp: now_ms(),
        }],
        tools: Vec::new(),
    }
}

/// Ambient auth for keyless test providers; reports "configured" with no auth values.
struct AmbientAuth;

#[async_trait]
impl ApiKeyAuth for AmbientAuth {
    fn name(&self) -> &str {
        "Ambient"
    }
    async fn resolve(&self, _input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        Ok(Some(AuthResult::default()))
    }
}

/// Resolve from the stored credential's key, else the ambient key.
struct EnvKeyAuth {
    key: Option<String>,
}

impl EnvKeyAuth {
    fn ambient(key: &str) -> Arc<Self> {
        Arc::new(Self {
            key: Some(key.to_string()),
        })
    }

    fn unconfigured() -> Arc<Self> {
        Arc::new(Self { key: None })
    }
}

#[async_trait]
impl ApiKeyAuth for EnvKeyAuth {
    fn name(&self) -> &str {
        "Test API key"
    }
    async fn resolve(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthResult>, AiError> {
        let resolved = input
            .credential
            .and_then(|credential| credential.key.clone())
            .or_else(|| self.key.clone());
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(resolved),
                ..Default::default()
            },
            env: None,
            source: Some(
                if input.credential.is_some() {
                    "stored"
                } else {
                    "env"
                }
                .to_string(),
            ),
        }))
    }
}

/// OAuth auth with pluggable refresh behavior (default: returns credential).
struct TestOAuth {
    refresh: Arc<
        dyn Fn(
                &OAuthCredential,
                &AbortSignal,
            ) -> BoxFuture<'static, Result<OAuthCredential, AiError>>
            + Send
            + Sync,
    >,
}

impl TestOAuth {
    fn new(
        refresh: impl Fn(
            &OAuthCredential,
            &AbortSignal,
        ) -> BoxFuture<'static, Result<OAuthCredential, AiError>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            refresh: Arc::new(refresh),
        })
    }

    fn passthrough() -> Arc<Self> {
        Self::new(|credential, _signal| {
            let credential = credential.clone();
            Box::pin(async move { Ok(credential) })
        })
    }
}

#[async_trait]
impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }
    async fn login(
        &self,
        _interaction: &ProviderAuthInteraction,
    ) -> Result<OAuthCredential, AiError> {
        Err(AiError::Other("not used".to_string()))
    }
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AbortSignal,
    ) -> Result<OAuthCredential, AiError> {
        (self.refresh)(credential, signal).await
    }
    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, AiError> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

fn oauth_credential(access: &str, refresh: &str, expires: u64) -> Credential {
    Credential::OAuth(OAuthCredential {
        refresh: refresh.to_string(),
        access: access.to_string(),
        expires,
        extra: BTreeMap::new(),
    })
}

fn api_key_credential(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential {
        key: Some(key.to_string()),
        env: None,
    })
}

fn set_credential<'a>(
    credentials: &'a InMemoryCredentialStore,
    provider_id: &'a str,
    credential: Credential,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        credentials
            .modify(
                provider_id,
                Box::new(move |_current| {
                    let credential = credential.clone();
                    Box::pin(async move { Ok(Some(credential)) })
                }),
                None,
            )
            .await
            .unwrap();
    })
}

fn auth_api_key(auth: Arc<dyn ApiKeyAuth>) -> ProviderAuth {
    ProviderAuth {
        api_key: Some(auth),
        oauth: None,
    }
}

fn auth_oauth(auth: Arc<dyn OAuthAuth>) -> ProviderAuth {
    ProviderAuth {
        api_key: None,
        oauth: Some(auth),
    }
}

fn headers_from(entries: &[(&str, &str)]) -> ProviderHeaders {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), Some(value.to_string())))
        .collect()
}

// --- Tests ---------------------------------------------------------------

// "enumerates credential metadata without exposing secrets"
#[tokio::test]
async fn enumerates_credential_metadata_without_exposing_secrets() {
    let credentials = InMemoryCredentialStore::new();
    set_credential(&credentials, "api-provider", api_key_credential("secret")).await;
    set_credential(
        &credentials,
        "oauth-provider",
        oauth_credential("access", "refresh", now_ms() + 60_000),
    )
    .await;

    let listed = credentials.list(None).await.unwrap();
    assert_eq!(
        listed,
        vec![
            CredentialInfo {
                provider_id: "api-provider".to_string(),
                kind: "api_key".to_string(),
            },
            CredentialInfo {
                provider_id: "oauth-provider".to_string(),
                kind: "oauth".to_string(),
            },
        ]
    );
}

// "applies request-wide pricing tiers above the configured input threshold"
#[test]
fn applies_request_wide_pricing_tiers_above_the_configured_input_threshold() {
    let mut model = test_model("openai", "gpt-5.6-sol");
    model.cost = ModelCost {
        rates: Default::default(),
        tiers: Some(vec![ModelCostTier {
            input_tokens_above: 272_000,
            rates: pillar_ai::types::ModelCostRates {
                input: 10.0,
                output: 45.0,
                cache_read: 1.0,
                cache_write: 12.5,
            },
        }]),
    };
    model.cost.rates = pillar_ai::types::ModelCostRates {
        input: 5.0,
        output: 30.0,
        cache_read: 0.5,
        cache_write: 6.25,
    };
    let create_usage = |cache_write: u64| Usage {
        input: 200_000,
        output: 100_000,
        cache_read: 72_000,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 372_000 + cache_write,
        cost: Default::default(),
    };

    let short = calculate_cost(&model, &mut create_usage(0));
    assert_eq!(
        (
            short.input,
            short.output,
            short.cache_read,
            short.cache_write
        ),
        (1.0, 3.0, 0.036, 0.0)
    );

    let long = calculate_cost(&model, &mut create_usage(1));
    assert_eq!(long.input, 2.0);
    assert_eq!(long.output, 4.5);
    assert_eq!(long.cache_read, 0.072);
    assert_eq!(long.cache_write, 0.0000125);
}

// "registers, replaces, and deletes providers"
#[test]
fn registers_replaces_and_deletes_providers() {
    let models = create_models(Default::default());
    let p1 = test_provider(TestProvider {
        id: "p1".into(),
        ..Default::default()
    });
    models.set_provider(Arc::clone(&p1));
    models.set_provider(test_provider(TestProvider {
        id: "p2".into(),
        ..Default::default()
    }));
    assert_eq!(
        models
            .get_providers()
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        vec!["p1", "p2"]
    );

    let replacement = test_provider(TestProvider {
        id: "p1".into(),
        ..Default::default()
    });
    models.set_provider(Arc::clone(&replacement));
    assert!(Arc::ptr_eq(
        &models.get_provider("p1").unwrap(),
        &replacement
    ));
    assert_eq!(models.get_providers().len(), 2);

    models.delete_provider("p1");
    assert!(models.get_provider("p1").is_none());

    models.clear_providers();
    assert_eq!(models.get_providers().len(), 0);
}

// "lists and finds models per provider"
#[test]
fn lists_and_finds_models_per_provider() {
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        models: Some(vec![test_model("p1", "m1"), test_model("p1", "m2")]),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProvider {
        id: "p2".into(),
        models: Some(vec![test_model("p2", "m3")]),
        ..Default::default()
    }));

    assert_eq!(
        models
            .get_models(None)
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>(),
        vec!["m1", "m2", "m3"]
    );
    assert_eq!(
        models
            .get_models(Some("p1"))
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>(),
        vec!["m1", "m2"]
    );
    assert_eq!(models.get_models(Some("nope")).len(), 0);
    assert_eq!(models.get_model("p2", "m3").unwrap().id, "m3");
    assert!(models.get_model("p2", "missing").is_none());

    // has_api() narrows dynamically looked-up models with a runtime check
    let found = models.get_model("p2", "m3");
    assert!(!has_api(found.as_ref().unwrap(), "openai-completions"));
    assert!(has_api(found.as_ref().unwrap(), "test-api"));
}

// "swallows provider source failures for both all-provider and single-provider listing"
#[test]
fn swallows_provider_source_failures_for_both_all_provider_and_single_provider_listing() {
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "broken".into(),
        get_models: Some(Box::new(|| panic!("boom"))),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProvider {
        id: "ok".into(),
        models: Some(vec![test_model("ok", "m1")]),
        ..Default::default()
    }));

    assert_eq!(
        models
            .get_models(None)
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>(),
        vec!["m1"]
    );
    assert_eq!(models.get_models(Some("broken")), Vec::<Model>::new());
    // precise failures come from the provider directly
    let broken = models.get_provider("broken").unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (broken.get_models)()));
    assert!(result.is_err());
}

// "refresh() updates every configured dynamic provider and reports failures"
#[tokio::test]
async fn refresh_updates_every_configured_dynamic_provider_and_reports_failures() {
    let list = Arc::new(Mutex::new(vec![test_model("dyn", "before")]));
    let refreshes = Arc::new(Mutex::new(0u32));
    let models = create_models(Default::default());
    let refresh: RefreshModelsFn = {
        let list = Arc::clone(&list);
        let refreshes = Arc::clone(&refreshes);
        Arc::new(move |mut context: RefreshModelsContext| {
            let list = Arc::clone(&list);
            let refreshes = Arc::clone(&refreshes);
            Box::pin(async move {
                if !context.allow_network {
                    return Ok(());
                }
                *refreshes.lock().unwrap() += 1;
                let list_for_update = Arc::clone(&list);
                context
                    .publish
                    .call(ModelsPublication {
                        persist: None,
                        update: Some(Box::new(move || {
                            *list_for_update.lock().unwrap() = vec![test_model("dyn", "after")];
                        })),
                    })
                    .await;
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "dyn".into(),
        get_models: Some({
            let list = Arc::clone(&list);
            Box::new(move || list.lock().unwrap().clone())
        }),
        refresh_models: Some(refresh),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProvider {
        id: "static".into(),
        models: Some(vec![test_model("static", "s1")]),
        ..Default::default()
    }));

    assert!(models.get_model("dyn", "before").is_some());
    let first = models.refresh(None).await;
    assert_eq!(first.errors.len(), 0);
    assert_eq!(*refreshes.lock().unwrap(), 1);
    assert!(models.get_model("dyn", "after").is_some());
    assert!(models.get_model("dyn", "before").is_none());

    let flaky: RefreshModelsFn = Arc::new(|context: RefreshModelsContext| {
        Box::pin(async move {
            if context.allow_network {
                return Err(AiError::Other("fetch failed".to_string()));
            }
            Ok(())
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "flaky".into(),
        refresh_models: Some(flaky),
        ..Default::default()
    }));
    let second = models.refresh(None).await;
    assert_eq!(*refreshes.lock().unwrap(), 2);
    assert_eq!(
        second.errors.get("flaky").map(String::as_str),
        Some("fetch failed")
    );
}

// "restricts refresh work to selected providers"
#[tokio::test]
async fn restricts_refresh_work_to_selected_providers() {
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let models = create_models(Default::default());
    for id in ["one", "two"] {
        let recorded = Arc::clone(&calls);
        let id_owned = id.to_string();
        let refresh: RefreshModelsFn = Arc::new(move |context: RefreshModelsContext| {
            let recorded = Arc::clone(&recorded);
            let id = id_owned.clone();
            Box::pin(async move {
                recorded.lock().unwrap().push(format!(
                    "{id}:{}",
                    if context.allow_network {
                        "network"
                    } else {
                        "cache"
                    }
                ));
                Ok(())
            })
        });
        models.set_provider(test_provider(TestProvider {
            id: id.into(),
            refresh_models: Some(refresh),
            ..Default::default()
        }));
    }

    let result = models
        .refresh(Some(ModelsRefreshOptions {
            providers: Some(vec!["two".to_string(), "unknown".to_string()]),
            ..Default::default()
        }))
        .await;

    assert_eq!(result.errors.len(), 0);
    assert_eq!(*calls.lock().unwrap(), vec!["two:cache", "two:network"]);
}

// "restores cached models before waiting for network auth"
#[tokio::test]
async fn restores_cached_models_before_waiting_for_network_auth() {
    let store = InMemoryModelsStore::new();
    store
        .write(
            "dynamic",
            &ModelsStoreEntry {
                models: vec![test_model("dynamic", "cached")],
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    struct BlockedAuth {
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        finish: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }
    #[async_trait]
    impl ApiKeyAuth for BlockedAuth {
        fn name(&self) -> &str {
            "Blocked auth"
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let receiver = self.finish.lock().unwrap().take().unwrap();
            let _ = receiver.await;
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("key".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }))
        }
    }
    let must_not_fetch: FetchModelsFn =
        Arc::new(|_context| Box::pin(async { Err(AiError::Other("must not fetch".to_string())) }));
    let provider = create_provider(CreateProviderOptions {
        id: "dynamic".to_string(),
        auth: auth_api_key(Arc::new(BlockedAuth {
            started: Mutex::new(Some(started_tx)),
            finish: Mutex::new(Some(finish_rx)),
        })),
        models: Vec::new(),
        fetch_models: Some(must_not_fetch),
        api: ProviderApi::Single(Arc::new(ProviderStreams {
            stream: empty_stream_fn(),
            stream_simple: empty_stream_fn(),
        })),
        ..Default::default()
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        models_store: Some(Arc::new(store)),
        ..Default::default()
    });
    models.set_provider(provider);
    let controller = AbortSignal::new();
    // Upstream's refresh promise starts eagerly; drive the lazy future until
    // the provider's auth callback signals it started.
    let mut pending = Box::pin(models.refresh(Some(ModelsRefreshOptions {
        providers: Some(vec!["dynamic".to_string()]),
        signal: Some(controller.clone()),
        ..Default::default()
    })));
    tokio::select! {
        _ = started_rx => {}
        result = &mut pending => panic!("refresh completed early: {result:?}"),
    };

    assert!(models.get_model("dynamic", "cached").is_some());
    controller.abort(None);
    let result = pending.await;
    assert!(result.aborted);
    drop(finish_tx);
}

// "lets providers choose persistent deletion and ephemeral publication atomically"
#[tokio::test]
async fn lets_providers_choose_persistent_deletion_and_ephemeral_publication_atomically() {
    #[derive(Default)]
    struct MapStore {
        entry: Mutex<Option<ModelsStoreEntry>>,
    }
    #[async_trait]
    impl ModelsStore for MapStore {
        async fn read(
            &self,
            _provider_id: &str,
            _options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<Option<ModelsStoreEntry>, AiError> {
            Ok(self.entry.lock().unwrap().clone())
        }
        async fn write(
            &self,
            _provider_id: &str,
            next: &ModelsStoreEntry,
            _options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<(), AiError> {
            *self.entry.lock().unwrap() = Some(next.clone());
            Ok(())
        }
        async fn delete(
            &self,
            _provider_id: &str,
            _options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<(), AiError> {
            *self.entry.lock().unwrap() = None;
            Ok(())
        }
    }

    let store = Arc::new(MapStore {
        entry: Mutex::new(Some(ModelsStoreEntry {
            models: vec![test_model("dynamic", "stored")],
            ..Default::default()
        })),
    });
    let state = Arc::new(Mutex::new("initial".to_string()));
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn ModelsStore>),
        ..Default::default()
    });
    let refresh: RefreshModelsFn = {
        let state = Arc::clone(&state);
        let store = Arc::clone(&store);
        Arc::new(move |mut context: RefreshModelsContext| {
            let state = Arc::clone(&state);
            let store = Arc::clone(&store);
            Box::pin(async move {
                assert_eq!(context.stored.as_ref().unwrap().models[0].id, "stored");
                let store_for_update = Arc::clone(&store);
                let state_for_update = Arc::clone(&state);
                context
                    .publish
                    .call(ModelsPublication {
                        persist: Some(None),
                        update: Some(Box::new(move || {
                            assert!(store_for_update.entry.lock().unwrap().is_none());
                            *state_for_update.lock().unwrap() = "deleted".to_string();
                        })),
                    })
                    .await;
                context
                    .publish
                    .call(ModelsPublication {
                        persist: None,
                        update: Some(Box::new({
                            let state = Arc::clone(&state);
                            move || {
                                *state.lock().unwrap() = "ephemeral".to_string();
                            }
                        })),
                    })
                    .await;
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    let result = models
        .refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            ..Default::default()
        }))
        .await;

    assert_eq!(result.errors.len(), 0);
    assert!(store.entry.lock().unwrap().is_none());
    assert_eq!(*state.lock().unwrap(), "ephemeral");
}

// "persists dynamic catalogs and restores them without network access"
#[tokio::test]
async fn persists_dynamic_catalogs_and_restores_them_without_network_access() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models_store = Arc::new(InMemoryModelsStore::new());
    set_credential(&credentials, "dynamic", api_key_credential("key")).await;
    let create_dynamic_provider = |fetch: Option<FetchModelsFn>| {
        create_provider(CreateProviderOptions {
            id: "dynamic".to_string(),
            auth: auth_api_key(EnvKeyAuth::unconfigured()),
            models: Vec::new(),
            fetch_models: fetch,
            api: ProviderApi::Single(Arc::new(ProviderStreams {
                stream: empty_stream_fn(),
                stream_simple: empty_stream_fn(),
            })),
            ..Default::default()
        })
    };

    let online = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        models_store: Some(Arc::clone(&models_store) as Arc<dyn ModelsStore>),
        ..Default::default()
    });
    let fetched: FetchModelsFn =
        Arc::new(|_context| Box::pin(async { Ok(vec![test_model("dynamic", "fetched")]) }));
    online.set_provider(create_dynamic_provider(Some(fetched)));
    assert_eq!(online.refresh(None).await.errors.len(), 0);
    assert!(online.get_model("dynamic", "fetched").is_some());

    let offline = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        models_store: Some(Arc::clone(&models_store) as Arc<dyn ModelsStore>),
        ..Default::default()
    });
    let must_not_fetch: FetchModelsFn =
        Arc::new(|_context| Box::pin(async { Err(AiError::Other("must not fetch".to_string())) }));
    offline.set_provider(create_dynamic_provider(Some(must_not_fetch)));
    let result = offline
        .refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            ..Default::default()
        }))
        .await;
    assert_eq!(result.errors.len(), 0);
    assert!(offline.get_model("dynamic", "fetched").is_some());
}

// "passes effective API-key credentials and refresh options while skipping unconfigured providers"
#[tokio::test]
async fn passes_effective_api_key_credentials_and_refresh_options_while_skipping_unconfigured_providers()
 {
    let effective_credential: Arc<Mutex<Option<Credential>>> = Arc::new(Mutex::new(None));
    let force_refresh: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let unconfigured_refreshes = Arc::new(Mutex::new(0u32));
    let models = create_models(Default::default());

    let configured_refresh: RefreshModelsFn = {
        let effective_credential = Arc::clone(&effective_credential);
        let force_refresh = Arc::clone(&force_refresh);
        Arc::new(move |context: RefreshModelsContext| {
            let effective_credential = Arc::clone(&effective_credential);
            let force_refresh = Arc::clone(&force_refresh);
            Box::pin(async move {
                if !context.allow_network {
                    return Ok(());
                }
                *effective_credential.lock().unwrap() = context.credential.clone();
                *force_refresh.lock().unwrap() = context.force;
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "configured".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("ambient-key"))),
        refresh_models: Some(configured_refresh),
        ..Default::default()
    }));
    let unconfigured_refresh: RefreshModelsFn = {
        let unconfigured_refreshes = Arc::clone(&unconfigured_refreshes);
        Arc::new(move |context: RefreshModelsContext| {
            let unconfigured_refreshes = Arc::clone(&unconfigured_refreshes);
            Box::pin(async move {
                if context.allow_network {
                    *unconfigured_refreshes.lock().unwrap() += 1;
                }
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "unconfigured".into(),
        auth: Some(auth_api_key(EnvKeyAuth::unconfigured())),
        refresh_models: Some(unconfigured_refresh),
        ..Default::default()
    }));

    models
        .refresh(Some(ModelsRefreshOptions {
            force: Some(true),
            ..Default::default()
        }))
        .await;
    assert_eq!(
        effective_credential.lock().unwrap().clone(),
        Some(api_key_credential("ambient-key"))
    );
    assert_eq!(*force_refresh.lock().unwrap(), Some(true));
    assert_eq!(*unconfigured_refreshes.lock().unwrap(), 0);
}

// "refreshes expired OAuth before refreshing models"
#[tokio::test]
async fn refreshes_expired_oauth_before_refreshing_models() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let model_refresh_credential: Arc<Mutex<Option<Credential>>> = Arc::new(Mutex::new(None));
    set_credential(
        &credentials,
        "oauth-dynamic",
        oauth_credential("expired", "refresh", 0),
    )
    .await;
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    let oauth = TestOAuth::new(|_credential, _signal| {
        Box::pin(async move {
            Ok(OAuthCredential {
                refresh: "rotated".to_string(),
                access: "fresh".to_string(),
                expires: now_ms() + 60_000,
                extra: BTreeMap::new(),
            })
        })
    });
    let recorded = Arc::clone(&model_refresh_credential);
    let refresh: RefreshModelsFn = Arc::new(move |context: RefreshModelsContext| {
        let recorded = Arc::clone(&recorded);
        Box::pin(async move {
            if context.allow_network {
                *recorded.lock().unwrap() = context.credential.clone();
            }
            Ok(())
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "oauth-dynamic".into(),
        auth: Some(auth_oauth(oauth)),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    assert_eq!(models.refresh(None).await.errors.len(), 0);
    match model_refresh_credential.lock().unwrap().clone() {
        Some(Credential::OAuth(credential)) => {
            assert_eq!(credential.access, "fresh");
            assert_eq!(credential.refresh, "rotated");
        }
        other => panic!("expected oauth credential, got {other:?}"),
    }
    match credentials.read("oauth-dynamic", None).await.unwrap() {
        Some(Credential::OAuth(credential)) => {
            assert_eq!(credential.access, "fresh");
            assert_eq!(credential.refresh, "rotated");
        }
        other => panic!("expected oauth credential, got {other:?}"),
    }
}

// "always gives providers a concrete signal"
#[tokio::test]
async fn always_gives_providers_a_concrete_signal() {
    let received: Arc<Mutex<Option<AbortSignal>>> = Arc::new(Mutex::new(None));
    let models = create_models(Default::default());
    let refresh: RefreshModelsFn = {
        let received = Arc::clone(&received);
        Arc::new(move |context: RefreshModelsContext| {
            let received = Arc::clone(&received);
            Box::pin(async move {
                *received.lock().unwrap() = Some(context.signal.clone());
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    let result = models.refresh(None).await;
    assert!(!result.aborted);
    let signal = received.lock().unwrap().clone().unwrap();
    assert!(!signal.is_aborted());
}

// "binds model-store waits to the provider refresh signal"
#[tokio::test]
async fn binds_model_store_waits_to_the_provider_refresh_signal() {
    let storage_signals: Arc<Mutex<Vec<AbortSignal>>> = Arc::new(Mutex::new(Vec::new()));
    #[derive(Default)]
    struct RecordingStore {
        signals: Arc<Mutex<Vec<AbortSignal>>>,
    }
    #[async_trait]
    impl ModelsStore for RecordingStore {
        async fn read(
            &self,
            _provider_id: &str,
            options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<Option<ModelsStoreEntry>, AiError> {
            if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
                self.signals.lock().unwrap().push(signal.clone());
            }
            Ok(None)
        }
        async fn write(
            &self,
            _provider_id: &str,
            _entry: &ModelsStoreEntry,
            options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<(), AiError> {
            if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
                self.signals.lock().unwrap().push(signal.clone());
            }
            Ok(())
        }
        async fn delete(
            &self,
            _provider_id: &str,
            options: Option<&ModelsStoreOperationOptions>,
        ) -> Result<(), AiError> {
            if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
                self.signals.lock().unwrap().push(signal.clone());
            }
            Ok(())
        }
    }
    let provider_signal: Arc<Mutex<Option<AbortSignal>>> = Arc::new(Mutex::new(None));
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        models_store: Some(Arc::new(RecordingStore {
            signals: Arc::clone(&storage_signals),
        })),
        ..Default::default()
    });
    let recorded = Arc::clone(&provider_signal);
    let refresh: RefreshModelsFn = Arc::new(move |mut context: RefreshModelsContext| {
        let recorded = Arc::clone(&recorded);
        Box::pin(async move {
            *recorded.lock().unwrap() = Some(context.signal.clone());
            if !context.allow_network {
                return Ok(());
            }
            context
                .publish
                .call(ModelsPublication {
                    persist: Some(Some(ModelsStoreEntry {
                        models: vec![test_model("dynamic", "fresh")],
                        ..Default::default()
                    })),
                    update: None,
                })
                .await;
            Ok(())
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("key"))),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    let result = models
        .refresh(Some(ModelsRefreshOptions {
            providers: Some(vec!["dynamic".to_string()]),
            ..Default::default()
        }))
        .await;

    assert_eq!(result.errors.len(), 0);
    let signals = storage_signals.lock().unwrap();
    assert_eq!(signals.len(), 3);
    let provider = provider_signal.lock().unwrap().clone().unwrap();
    assert!(signals.iter().all(|signal| signal.same_as(&provider)));
}

// "returns aborted state without reporting cancellation as a provider error"
#[tokio::test]
async fn returns_aborted_state_without_reporting_cancellation_as_a_provider_error() {
    let controller = AbortSignal::new();
    let models = create_models(Default::default());
    let abort_source = controller.clone();
    let refresh: RefreshModelsFn = Arc::new(move |context: RefreshModelsContext| {
        let abort_source = abort_source.clone();
        Box::pin(async move {
            abort_source.abort(None);
            if context.signal.is_aborted() {
                return Ok(());
            }
            Ok(())
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    let result = models
        .refresh(Some(ModelsRefreshOptions {
            signal: Some(controller.clone()),
            ..Default::default()
        }))
        .await;
    assert!(result.aborted);
    assert_eq!(result.errors.len(), 0);
}

// "stops waiting on abort when a provider ignores its signal"
#[tokio::test]
async fn stops_waiting_on_abort_when_a_provider_ignores_its_signal() {
    let controller = AbortSignal::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (stalled_tx, stalled_rx) = tokio::sync::oneshot::channel::<Result<(), AiError>>();
    let stalled = Mutex::new(Some(stalled_rx));
    let calls = Arc::new(Mutex::new(0u32));
    let models = create_models(Default::default());
    let refresh: RefreshModelsFn = {
        let calls = Arc::clone(&calls);
        let started_slot = Mutex::new(Some(started_tx));
        Arc::new(move |_context: RefreshModelsContext| {
            let calls = Arc::clone(&calls);
            let started_tx = started_slot.lock().unwrap().take();
            let stalled_rx = stalled.lock().unwrap().take();
            Box::pin(async move {
                let call = {
                    let mut guard = calls.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                if call != 1 {
                    return Ok(());
                }
                if let Some(sender) = started_tx {
                    let _ = sender.send(());
                }
                if let Some(receiver) = stalled_rx {
                    // Non-cooperative: ignores the abort signal entirely.
                    let _ = receiver.await;
                }
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    // Upstream's refresh promise starts eagerly; drive the lazy future until
    // the provider's refresh callback signals it started.
    let mut pending = Box::pin(models.refresh(Some(ModelsRefreshOptions {
        signal: Some(controller.clone()),
        ..Default::default()
    })));
    tokio::select! {
        _ = started_rx => {}
        result = &mut pending => panic!("refresh completed early: {result:?}"),
    };
    controller.abort(None);

    let result = pending.await;
    assert!(result.aborted);
    assert_eq!(result.errors.len(), 0);

    // A late provider failure after the abort is not reported as an error.
    let _ = stalled_tx.send(Err(AiError::Other("late provider failure".to_string())));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(result.errors.len(), 0);
}

// "rejects late publication from a superseded non-cooperative provider"
#[tokio::test]
async fn rejects_late_publication_from_a_superseded_non_cooperative_provider() {
    let store = Arc::new(InMemoryModelsStore::new());
    let state = Arc::new(Mutex::new("initial".to_string()));
    let calls = Arc::new(Mutex::new(0u32));
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (first_finish_tx, first_finish_rx) = tokio::sync::oneshot::channel::<()>();
    let first_finish = Arc::new(Mutex::new(Some(first_finish_rx)));
    let first_started_slot = Arc::new(Mutex::new(Some(first_started_tx)));
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        models_store: Some(Arc::clone(&store) as Arc<dyn ModelsStore>),
        ..Default::default()
    });
    let refresh: RefreshModelsFn = {
        let state = Arc::clone(&state);
        let calls = Arc::clone(&calls);
        Arc::new(move |mut context: RefreshModelsContext| {
            let state = Arc::clone(&state);
            let calls = Arc::clone(&calls);
            let first_started_slot = Arc::clone(&first_started_slot);
            let first_finish = Arc::clone(&first_finish);
            Box::pin(async move {
                if !context.allow_network {
                    return Ok(());
                }
                let current = {
                    let mut guard = calls.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                if current == 1 {
                    // Take the gate only when the blocking call actually
                    // runs (upstream: persistent closure variables).
                    if let Some(sender) = first_started_slot.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                    let gate_receiver = first_finish.lock().unwrap().take();
                    if let Some(receiver) = gate_receiver {
                        let _ = receiver.await;
                    }
                }
                let value = format!("generation-{current}");
                let state_for_update = Arc::clone(&state);
                let value_for_state = value.clone();
                context
                    .publish
                    .call(ModelsPublication {
                        persist: Some(Some(ModelsStoreEntry {
                            models: vec![test_model("dynamic", &value)],
                            ..Default::default()
                        })),
                        update: Some(Box::new(move || {
                            *state_for_update.lock().unwrap() = value_for_state;
                        })),
                    })
                    .await;
                Ok(())
            })
        })
    };
    models.set_provider(test_provider(TestProvider {
        id: "dynamic".into(),
        refresh_models: Some(refresh),
        ..Default::default()
    }));

    // Upstream's refresh promise starts eagerly; drive the lazy future until
    // the first refresh signals it started.
    let mut first = Box::pin(models.refresh(Some(ModelsRefreshOptions {
        providers: Some(vec!["dynamic".to_string()]),
        ..Default::default()
    })));
    tokio::select! {
        _ = first_started_rx => {}
        result = &mut first => panic!("first refresh completed early: {result:?}"),
    };
    let second = models.refresh(Some(ModelsRefreshOptions {
        providers: Some(vec!["dynamic".to_string()]),
        ..Default::default()
    }));
    second.await;
    first.await;
    drop(first_finish_tx);
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    assert_eq!(*state.lock().unwrap(), "generation-2");
    let stored = store.read("dynamic", None).await.unwrap().unwrap();
    assert_eq!(stored.models[0].id, "generation-2");
}

// "passes caller signals to provider auth callbacks"
#[tokio::test]
async fn passes_caller_signals_to_provider_auth_callbacks() {
    let controller = AbortSignal::new();
    let received: Arc<Mutex<Vec<AbortSignal>>> = Arc::new(Mutex::new(Vec::new()));

    struct SignalAuth {
        received: Arc<Mutex<Vec<AbortSignal>>>,
    }
    #[async_trait]
    impl ApiKeyAuth for SignalAuth {
        fn name(&self) -> &str {
            "Signal auth"
        }
        fn has_check(&self) -> bool {
            true
        }
        async fn login(
            &self,
            interaction: &ProviderAuthInteraction,
        ) -> Result<Option<ApiKeyCredential>, AiError> {
            self.received
                .lock()
                .unwrap()
                .push(interaction.signal.clone());
            Ok(Some(ApiKeyCredential {
                key: Some("saved".to_string()),
                env: None,
            }))
        }
        async fn check(&self, input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthCheck>, AiError> {
            self.received
                .lock()
                .unwrap()
                .push(input.signal.cloned().unwrap());
            Ok(Some(AuthCheck {
                source: None,
                kind: "api_key".to_string(),
            }))
        }
        async fn resolve(
            &self,
            input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            self.received
                .lock()
                .unwrap()
                .push(input.signal.cloned().unwrap());
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("resolved".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }))
        }
    }

    let auth = Arc::new(SignalAuth {
        received: Arc::clone(&received),
    });
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(auth)),
        ..Default::default()
    }));

    let options = AuthOperationOptions {
        signal: Some(controller.clone()),
    };
    models.check_auth("p1", Some(&options)).await.unwrap();
    models
        .get_auth(
            AuthTarget::Provider("p1".to_string()),
            Some(&pillar_ai::auth_resolve::AuthResolutionOverrides {
                signal: Some(controller.clone()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    struct NoopInteraction {
        signal: Option<AbortSignal>,
    }
    #[async_trait]
    impl AuthInteraction for NoopInteraction {
        fn signal(&self) -> Option<&AbortSignal> {
            self.signal.as_ref()
        }
        async fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, AiError> {
            Ok("unused".to_string())
        }
        async fn notify(&self, _event: &AuthEvent) {}
    }
    models
        .login(
            "p1",
            &"api_key".to_string(),
            &NoopInteraction {
                signal: Some(controller.clone()),
            },
        )
        .await
        .unwrap();

    let received = received.lock().unwrap();
    assert_eq!(received.len(), 3);
    assert!(received.iter().all(|signal| signal.same_as(&controller)));
}

// "stops waiting for non-cooperative auth callbacks"
#[tokio::test]
async fn stops_waiting_for_non_cooperative_auth_callbacks() {
    struct BlockedPair {
        check_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        check_finish: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        resolve_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resolve_finish: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }
    #[async_trait]
    impl ApiKeyAuth for BlockedPair {
        fn name(&self) -> &str {
            "Blocked auth"
        }
        fn has_check(&self) -> bool {
            true
        }
        async fn check(&self, _input: &ApiKeyAuthInput<'_>) -> Result<Option<AuthCheck>, AiError> {
            if let Some(sender) = self.check_started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let receiver = self.check_finish.lock().unwrap().take().unwrap();
            let _ = receiver.await;
            Ok(Some(AuthCheck {
                source: None,
                kind: "api_key".to_string(),
            }))
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            if let Some(sender) = self.resolve_started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let receiver = self.resolve_finish.lock().unwrap().take().unwrap();
            let _ = receiver.await;
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("key".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }))
        }
    }

    let (check_started_tx, check_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (check_finish_tx, check_finish_rx) = tokio::sync::oneshot::channel::<()>();
    let (resolve_started_tx, resolve_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (resolve_finish_tx, resolve_finish_rx) = tokio::sync::oneshot::channel::<()>();
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(Arc::new(BlockedPair {
            check_started: Mutex::new(Some(check_started_tx)),
            check_finish: Mutex::new(Some(check_finish_rx)),
            resolve_started: Mutex::new(Some(resolve_started_tx)),
            resolve_finish: Mutex::new(Some(resolve_finish_rx)),
        }))),
        ..Default::default()
    }));

    let available_controller = AbortSignal::new();
    let available_options = AuthOperationOptions {
        signal: Some(available_controller.clone()),
    };
    let mut available = Box::pin(models.get_available(None, Some(&available_options)));
    tokio::select! {
        _ = check_started_rx => {}
        result = &mut available => panic!("available completed early: {result:?}"),
    };
    available_controller.abort(None);
    let error = available.await.unwrap_err();
    assert!(matches!(error, AiError::Aborted(_)), "{error}");

    let auth_controller = AbortSignal::new();
    let auth_overrides = pillar_ai::auth_resolve::AuthResolutionOverrides {
        signal: Some(auth_controller.clone()),
        ..Default::default()
    };
    let mut auth = Box::pin(models.get_auth(
        AuthTarget::Provider("p1".to_string()),
        Some(&auth_overrides),
    ));
    tokio::select! {
        _ = resolve_started_rx => {}
        result = &mut auth => panic!("auth completed early: {result:?}"),
    };
    auth_controller.abort(None);
    let error = auth.await.unwrap_err();
    assert!(matches!(error, AiError::Aborted(_)), "{error}");

    drop(check_finish_tx);
    drop(resolve_finish_tx);
}

// "cancels queued credential mutations without running them later"
#[tokio::test]
async fn cancels_queued_credential_mutations_without_running_them_later() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (first_finish_tx, first_finish_rx) = tokio::sync::oneshot::channel::<()>();
    let second_ran = Arc::new(Mutex::new(false));

    let first_credentials = Arc::clone(&credentials);
    let first_task = tokio::spawn(async move {
        first_credentials
            .modify(
                "p1",
                Box::new(move |_current| {
                    let first_finish_rx = first_finish_rx;
                    Box::pin(async move {
                        let _ = first_started_tx.send(());
                        let _ = first_finish_rx.await;
                        Ok(Some(api_key_credential("first")))
                    })
                }),
                None,
            )
            .await
            .unwrap();
    });
    first_started_rx.await.unwrap(); // first holds the per-provider lock

    let controller = AbortSignal::new();
    let second_ran_for_task = Arc::clone(&second_ran);
    let second_task = tokio::spawn({
        let credentials = Arc::clone(&credentials);
        let controller = controller.clone();
        async move {
            credentials
                .modify(
                    "p1",
                    Box::new(move |_current| {
                        let second_ran = Arc::clone(&second_ran_for_task);
                        Box::pin(async move {
                            *second_ran.lock().unwrap() = true;
                            Ok(Some(api_key_credential("second")))
                        })
                    }),
                    Some(&AuthOperationOptions {
                        signal: Some(controller),
                    }),
                )
                .await
        }
    });

    controller.abort(None);
    let second = second_task.await.unwrap();
    assert!(matches!(second, Err(AiError::Aborted(_))), "{second:?}");
    let _ = first_finish_tx.send(());
    first_task.await.unwrap();
    assert!(!*second_ran.lock().unwrap());
    assert_eq!(
        credentials.read("p1", None).await.unwrap(),
        Some(api_key_credential("first"))
    );
}

// "passes cancellation to OAuth refresh and preserves the previous credential"
#[tokio::test]
async fn passes_cancellation_to_oauth_refresh_and_preserves_the_previous_credential() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let previous = oauth_credential("old", "old-refresh", 0);
    set_credential(&credentials, "p1", previous.clone()).await;
    let (refresh_started_tx, refresh_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (refresh_finish_tx, refresh_finish_rx) = tokio::sync::oneshot::channel::<OAuthCredential>();
    let received_signal: Arc<Mutex<Option<AbortSignal>>> = Arc::new(Mutex::new(None));

    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    let received_for_refresh = Arc::clone(&received_signal);
    let started_slot = Mutex::new(Some(refresh_started_tx));
    let finish_slot = Mutex::new(Some(refresh_finish_rx));
    let oauth = TestOAuth::new(move |_credential, signal| {
        let received = Arc::clone(&received_for_refresh);
        let refresh_started_tx = started_slot.lock().unwrap().take();
        let refresh_finish_rx = finish_slot.lock().unwrap().take();
        let signal = signal.clone();
        Box::pin(async move {
            *received.lock().unwrap() = Some(signal);
            if let Some(sender) = refresh_started_tx {
                let _ = sender.send(());
            }
            if let Some(receiver) = refresh_finish_rx {
                let refreshed = receiver.await.unwrap();
                return Ok(refreshed);
            }
            Err(AiError::Other("refresh gate closed".to_string()))
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    let controller = AbortSignal::new();
    let auth_overrides = pillar_ai::auth_resolve::AuthResolutionOverrides {
        signal: Some(controller.clone()),
        ..Default::default()
    };
    let mut auth = Box::pin(models.get_auth(
        AuthTarget::Provider("p1".to_string()),
        Some(&auth_overrides),
    ));
    tokio::select! {
        _ = refresh_started_rx => {}
        result = &mut auth => panic!("auth completed early: {result:?}"),
    };
    controller.abort(None);

    let error = auth.await.unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("abort"),
        "{error}"
    );
    let signal = received_signal.lock().unwrap().clone().unwrap();
    assert!(signal.is_aborted());
    let _ = refresh_finish_tx.send(OAuthCredential {
        refresh: "old-refresh".to_string(),
        access: "new".to_string(),
        expires: now_ms() + 60_000,
        extra: BTreeMap::new(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(credentials.read("p1", None).await.unwrap(), Some(previous));
}

// "resolves auth: stored credential owns the provider, ambient only when nothing stored"
#[tokio::test]
async fn resolves_auth_stored_credential_owns_the_provider_ambient_only_when_nothing_stored() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(ProviderAuth {
            api_key: Some(EnvKeyAuth::ambient("env-key")),
            oauth: Some(TestOAuth::passthrough()),
        }),
        ..Default::default()
    }));
    let model = test_model("p1", "model-a");

    // model and provider-id overloads resolve the same provider-scoped auth
    let resolution = models
        .get_auth(AuthTarget::Model(Box::new(model.clone())), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("env-key"));
    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("env-key"));
    let resolution = models
        .get_auth(
            AuthTarget::Model(Box::new(model.clone())),
            Some(&pillar_ai::auth_resolve::AuthResolutionOverrides {
                api_key: Some("explicit-key".to_string()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("explicit-key"));

    // stored oauth credential (persisted via the single write path): beats ambient env
    set_credential(
        &credentials,
        "p1",
        oauth_credential("oauth-token", "r", now_ms() + 10 * 60_000),
    )
    .await;
    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("oauth-token"));
    assert_eq!(resolution.source.as_deref(), Some("OAuth"));

    // stored api-key credential resolves through apiKey auth, beats env
    set_credential(&credentials, "p1", api_key_credential("stored-key")).await;
    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("stored-key"));
    assert_eq!(resolution.source.as_deref(), Some("stored"));
}

// "checks provider auth without refreshing OAuth and filters available models"
#[tokio::test]
async fn checks_provider_auth_without_refreshing_oauth_and_filters_available_models() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let refreshes = Arc::new(Mutex::new(0u32));
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "ambient".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("env-key"))),
        ..Default::default()
    }));
    models.set_provider(test_provider(TestProvider {
        id: "missing".into(),
        auth: Some(auth_api_key(EnvKeyAuth::unconfigured())),
        ..Default::default()
    }));
    let counted = Arc::clone(&refreshes);
    let oauth = TestOAuth::new(move |credential, _signal| {
        let counted = Arc::clone(&counted);
        let credential = credential.clone();
        Box::pin(async move {
            *counted.lock().unwrap() += 1;
            Ok(credential)
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "oauth".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    set_credential(
        &credentials,
        "oauth",
        oauth_credential("expired", "refresh", 0),
    )
    .await;

    let check = models.check_auth("ambient", None).await.unwrap().unwrap();
    assert_eq!(check.source.as_deref(), Some("env"));
    assert_eq!(check.kind, "api_key");
    assert!(models.check_auth("missing", None).await.unwrap().is_none());
    let check = models.check_auth("oauth", None).await.unwrap().unwrap();
    assert_eq!(check.source.as_deref(), Some("OAuth"));
    assert_eq!(check.kind, "oauth");
    assert_eq!(*refreshes.lock().unwrap(), 0);
    assert_eq!(
        models
            .get_available(None, None)
            .await
            .unwrap()
            .iter()
            .map(|model| model.provider.clone())
            .collect::<Vec<_>>(),
        vec!["ambient", "oauth"]
    );
    assert_eq!(
        models
            .get_available(Some("ambient"), None)
            .await
            .unwrap()
            .iter()
            .map(|model| model.provider.clone())
            .collect::<Vec<_>>(),
        vec!["ambient"]
    );
}

// "runs provider login and logout through the credential store"
#[tokio::test]
async fn runs_provider_login_and_logout_through_the_credential_store() {
    struct LoginKeyAuth;
    #[async_trait]
    impl ApiKeyAuth for LoginKeyAuth {
        fn name(&self) -> &str {
            "Test API key"
        }
        async fn login(
            &self,
            _interaction: &ProviderAuthInteraction,
        ) -> Result<Option<ApiKeyCredential>, AiError> {
            Ok(Some(ApiKeyCredential {
                key: Some("logged-in".to_string()),
                env: None,
            }))
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            Ok(None)
        }
    }

    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(Arc::new(LoginKeyAuth))),
        ..Default::default()
    }));

    struct NoopInteraction;
    #[async_trait]
    impl AuthInteraction for NoopInteraction {
        fn signal(&self) -> Option<&AbortSignal> {
            None
        }
        async fn prompt(&self, _prompt: &AuthPrompt) -> Result<String, AiError> {
            Ok("unused".to_string())
        }
        async fn notify(&self, _event: &AuthEvent) {}
    }
    let credential = models
        .login("p1", &"api_key".to_string(), &NoopInteraction)
        .await
        .unwrap();
    assert_eq!(credential, api_key_credential("logged-in"));
    assert_eq!(
        credentials.read("p1", None).await.unwrap(),
        Some(credential.clone())
    );

    models.logout("p1", None).await.unwrap();
    assert_eq!(credentials.read("p1", None).await.unwrap(), None);
}

// "a stored credential without a matching handler blocks ambient fallback"
#[tokio::test]
async fn a_stored_credential_without_a_matching_handler_blocks_ambient_fallback() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    // provider has only apiKey auth, but an oauth credential is stored (stale config)
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("env-key"))),
        ..Default::default()
    }));
    set_credential(&credentials, "p1", oauth_credential("a", "r", 0)).await;

    assert!(
        models
            .get_auth(AuthTarget::Provider("p1".to_string()), None)
            .await
            .unwrap()
            .is_none()
    );
}

// "refreshes expired oauth credentials and persists the rotated credential"
#[tokio::test]
async fn refreshes_expired_oauth_credentials_and_persists_the_rotated_credential() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let oauth = TestOAuth::new(|credential, _signal| {
        let mut refreshed = credential.clone();
        refreshed.access = "new-token".to_string();
        refreshed.expires = now_ms() + 60 * 60_000;
        Box::pin(async move { Ok(refreshed) })
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    set_credential(&credentials, "p1", oauth_credential("old-token", "r", 0)).await;

    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    match credentials.read("p1", None).await.unwrap().unwrap() {
        Credential::OAuth(credential) => assert_eq!(credential.access, "new-token"),
        other => panic!("expected oauth credential, got {other:?}"),
    }
}

// "refreshes oauth credentials with less than five minutes remaining"
#[tokio::test]
async fn refreshes_oauth_credentials_with_less_than_five_minutes_remaining() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let refreshes = Arc::new(Mutex::new(0u32));
    let counted = Arc::clone(&refreshes);
    let oauth = TestOAuth::new(move |credential, _signal| {
        let counted = Arc::clone(&counted);
        let mut refreshed = credential.clone();
        refreshed.access = "new-token".to_string();
        refreshed.expires = now_ms() + 60 * 60_000;
        Box::pin(async move {
            *counted.lock().unwrap() += 1;
            Ok(refreshed)
        })
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    set_credential(
        &credentials,
        "p1",
        oauth_credential("old-token", "r", now_ms() + 60_000),
    )
    .await;

    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(*refreshes.lock().unwrap(), 1);
}

// "honors a caller's longer OAuth minimum validity"
#[tokio::test]
async fn honors_a_callers_longer_oauth_minimum_validity() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let refreshes = Arc::new(Mutex::new(0u32));
    let counted = Arc::clone(&refreshes);
    let oauth = TestOAuth::new(move |credential, _signal| {
        let counted = Arc::clone(&counted);
        let mut refreshed = credential.clone();
        refreshed.access = "new-token".to_string();
        refreshed.expires = now_ms() + 60 * 60_000;
        Box::pin(async move {
            *counted.lock().unwrap() += 1;
            Ok(refreshed)
        })
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    set_credential(
        &credentials,
        "p1",
        oauth_credential("old-token", "r", now_ms() + 10 * 60_000),
    )
    .await;

    let resolution = models
        .get_auth(
            AuthTarget::Provider("p1".to_string()),
            Some(&pillar_ai::auth_resolve::AuthResolutionOverrides {
                min_oauth_validity_ms: Some(30 * 60_000),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(*refreshes.lock().unwrap(), 1);
}

// "rejects with code oauth when refresh fails, preserving the stored credential"
#[tokio::test]
async fn rejects_with_code_oauth_when_refresh_fails_preserving_the_stored_credential() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let oauth = TestOAuth::new(|_credential, _signal| {
        Box::pin(async { Err(AiError::Other("invalid_grant".to_string())) })
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));
    set_credential(&credentials, "p1", oauth_credential("old", "r", 0)).await;

    let error = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with("oauth:"),
        "expected oauth code, got {error}"
    );
    // credential preserved for retry / re-login
    match credentials.read("p1", None).await.unwrap().unwrap() {
        Credential::OAuth(credential) => assert_eq!(credential.access, "old"),
        other => panic!("expected oauth credential, got {other:?}"),
    }
}

// "serializes concurrent OAuth refreshes through store.modify (no double refresh)"
#[tokio::test]
async fn serializes_concurrent_oauth_refreshes_through_store_modify_no_double_refresh() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    set_credential(&credentials, "p1", oauth_credential("old", "r1", 0)).await;

    let refreshes = Arc::new(Mutex::new(0u32));
    let counted = Arc::clone(&refreshes);
    let oauth = TestOAuth::new(move |_credential, _signal| {
        let counted = Arc::clone(&counted);
        Box::pin(async move {
            let access = {
                let mut guard = counted.lock().unwrap();
                *guard += 1;
                format!("new-{}", *guard)
            };
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(OAuthCredential {
                refresh: "r2".to_string(),
                access,
                expires: now_ms() + 60 * 60_000,
                extra: BTreeMap::new(),
            })
        })
    });
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));

    let (a, b) = tokio::join!(
        models.get_auth(AuthTarget::Provider("p1".to_string()), None),
        models.get_auth(AuthTarget::Provider("p1".to_string()), None),
    );
    assert_eq!(*refreshes.lock().unwrap(), 1);
    assert_eq!(a.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    assert_eq!(b.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
}

// "valid oauth tokens resolve without touching modify"
#[tokio::test]
async fn valid_oauth_tokens_resolve_without_touching_modify() {
    struct CountingStore {
        base: InMemoryCredentialStore,
        modifies: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl CredentialStore for CountingStore {
        async fn read(
            &self,
            provider_id: &str,
            options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            self.base.read(provider_id, options).await
        }
        async fn list(
            &self,
            options: Option<&AuthOperationOptions>,
        ) -> Result<Vec<CredentialInfo>, AiError> {
            self.base.list(options).await
        }
        async fn modify(
            &self,
            provider_id: &str,
            f: pillar_ai::auth_types::CredentialModifier<'_>,
            options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            *self.modifies.lock().unwrap() += 1;
            self.base.modify(provider_id, f, options).await
        }
        async fn delete(
            &self,
            provider_id: &str,
            options: Option<&AuthOperationOptions>,
        ) -> Result<(), AiError> {
            self.base.delete(provider_id, options).await
        }
    }

    let modifies = Arc::new(Mutex::new(0u32));
    let credentials = Arc::new(CountingStore {
        base: InMemoryCredentialStore::new(),
        modifies: Arc::clone(&modifies),
    });
    set_credential(
        &credentials.base,
        "p1",
        oauth_credential("valid", "r", now_ms() + 10 * 60_000),
    )
    .await;
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(TestOAuth::passthrough())),
        ..Default::default()
    }));

    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("valid"));
    assert_eq!(*modifies.lock().unwrap(), 0);
}

// "wraps credential store failures in ModelsError"
#[tokio::test]
async fn wraps_credential_store_failures_in_models_error() {
    // read failure
    struct ReadFailing;
    #[async_trait]
    impl CredentialStore for ReadFailing {
        async fn read(
            &self,
            _provider_id: &str,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            Err(AiError::Other("disk on fire".to_string()))
        }
        async fn list(
            &self,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Vec<CredentialInfo>, AiError> {
            Ok(Vec::new())
        }
        async fn modify(
            &self,
            _provider_id: &str,
            _f: pillar_ai::auth_types::CredentialModifier<'_>,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            Ok(None)
        }
        async fn delete(
            &self,
            _provider_id: &str,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<(), AiError> {
            Ok(())
        }
    }
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::new(ReadFailing)),
        ..Default::default()
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("env-key"))),
        ..Default::default()
    }));
    let error = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with("auth:"),
        "expected auth code, got {error}"
    );

    // modify failure during refresh
    struct ModifyFailing;
    #[async_trait]
    impl CredentialStore for ModifyFailing {
        async fn read(
            &self,
            _provider_id: &str,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            Ok(Some(oauth_credential("old", "r", 0)))
        }
        async fn list(
            &self,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Vec<CredentialInfo>, AiError> {
            Ok(vec![CredentialInfo {
                provider_id: "p1".to_string(),
                kind: "oauth".to_string(),
            }])
        }
        async fn modify(
            &self,
            _provider_id: &str,
            _f: pillar_ai::auth_types::CredentialModifier<'_>,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<Option<Credential>, AiError> {
            Err(AiError::Other("disk on fire".to_string()))
        }
        async fn delete(
            &self,
            _provider_id: &str,
            _options: Option<&AuthOperationOptions>,
        ) -> Result<(), AiError> {
            Ok(())
        }
    }
    let oauth_models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::new(ModifyFailing)),
        ..Default::default()
    });
    oauth_models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(TestOAuth::passthrough())),
        ..Default::default()
    }));
    let error = oauth_models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with("auth:"),
        "expected auth code, got {error}"
    );
}

// "keeps the underlying reason in wrapped oauth refresh errors"
#[tokio::test]
async fn keeps_the_underlying_reason_in_wrapped_oauth_refresh_errors() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    set_credential(&credentials, "p1", oauth_credential("old", "r", 0)).await;
    let models = create_models(pillar_ai::models::CreateModelsOptions {
        credentials: Some(Arc::clone(&credentials) as Arc<dyn CredentialStore>),
        ..Default::default()
    });
    let oauth = TestOAuth::new(|_credential, _signal| {
        Box::pin(async {
            Err(AiError::Other(
                "token refresh failed (400): invalid_grant".to_string(),
            ))
        })
    });
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_oauth(oauth)),
        ..Default::default()
    }));

    let error = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("OAuth refresh failed for p1: token refresh failed (400): invalid_grant"),
        "{error}"
    );
}

// "wraps api-key auth failures in ModelsError"
#[tokio::test]
async fn wraps_api_key_auth_failures_in_models_error() {
    struct Failing;
    #[async_trait]
    impl ApiKeyAuth for Failing {
        fn name(&self) -> &str {
            "Failing"
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            Err(AiError::Other("nope".to_string()))
        }
    }
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(Arc::new(Failing))),
        ..Default::default()
    }));
    let error = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with("auth:"),
        "expected auth code, got {error}"
    );
}

// "uses explicit request api key and env during provider auth resolution"
#[tokio::test]
async fn uses_explicit_request_api_key_and_env_during_provider_auth_resolution() {
    struct ScopedAuth;
    #[async_trait]
    impl ApiKeyAuth for ScopedAuth {
        fn name(&self) -> &str {
            "Scoped"
        }
        async fn resolve(
            &self,
            input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            let account = input.credential.and_then(|credential| {
                credential
                    .env
                    .as_ref()
                    .and_then(|env| env.get("ACCOUNT_ID").cloned())
            });
            let account = match account {
                Some(account) => Some(account),
                None => input.ctx.env("ACCOUNT_ID").await,
            };
            let key = input
                .credential
                .and_then(|credential| credential.key.clone());
            let (Some(key), Some(account)) = (key, account) else {
                return Ok(None);
            };
            let mut env = ProviderEnv::new();
            env.insert("ACCOUNT_ID".to_string(), account.clone());
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    base_url: Some(format!("https://example.test/{account}")),
                    ..Default::default()
                },
                env: Some(env),
                source: None,
            }))
        }
    }

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(Arc::new(ScopedAuth))),
        calls: Some(Arc::clone(&calls)),
        ..Default::default()
    }));
    let model = test_model("p1", "model-a");

    let mut env = ProviderEnv::new();
    env.insert("ACCOUNT_ID".to_string(), "acct".to_string());
    models
        .complete_simple(
            &model,
            &test_context(),
            Some(ModelsStreamOptions {
                api_key: Some("explicit-key".to_string()),
                env: Some(env.clone()),
                ..Default::default()
            }),
        )
        .await;

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model.base_url, "https://example.test/acct");
    assert_eq!(calls[0].options.api_key.as_deref(), Some("explicit-key"));
    assert_eq!(calls[0].options.env.as_ref(), Some(&env));
}

// "merges resolved auth into stream options; explicit options win per field"
#[tokio::test]
async fn merges_resolved_auth_into_stream_options_explicit_options_win_per_field() {
    struct ResolvingAuth;
    #[async_trait]
    impl ApiKeyAuth for ResolvingAuth {
        fn name(&self) -> &str {
            "Test"
        }
        async fn resolve(
            &self,
            _input: &ApiKeyAuthInput<'_>,
        ) -> Result<Option<AuthResult>, AiError> {
            Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some("resolved-key".to_string()),
                    headers: Some(headers_from(&[
                        ("Authorization", "Bearer resolved-key"),
                        ("x-a", "auth"),
                        ("x-b", "auth"),
                    ])),
                    base_url: Some("https://auth.test/v1".to_string()),
                },
                ..Default::default()
            }))
        }
    }

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(Arc::new(ResolvingAuth))),
        calls: Some(Arc::clone(&calls)),
        ..Default::default()
    }));
    let model = test_model("p1", "model-a");

    let result = models
        .complete_simple(
            &model,
            &test_context(),
            Some(ModelsStreamOptions {
                api_key: Some("explicit-key".to_string()),
                headers: Some(headers_from(&[
                    ("authorization", "Explicit token"),
                    ("x-b", "explicit"),
                ])),
                ..Default::default()
            }),
        )
        .await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    {
        let snapshot = calls.lock().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].options.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(
            snapshot[0].options.headers.as_ref(),
            Some(&headers_from(&[
                ("authorization", "Explicit token"),
                ("x-a", "auth"),
                ("x-b", "explicit"),
            ]))
        );
        assert_eq!(snapshot[0].model.base_url, "https://auth.test/v1");
    }

    // without explicit options, resolved auth applies
    let result = models.complete_simple(&model, &test_context(), None).await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    let snapshot = calls.lock().unwrap();
    assert_eq!(snapshot[1].options.api_key.as_deref(), Some("resolved-key"));
}

// "adds model headers only for model auth and transforms assembled headers once"
#[tokio::test]
async fn adds_model_headers_only_for_model_auth_and_transforms_assembled_headers_once() {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        auth: Some(auth_api_key(EnvKeyAuth::ambient("key"))),
        calls: Some(Arc::clone(&calls)),
        ..Default::default()
    }));
    let mut model = test_model("p1", "model-a");
    model.headers = Some(headers_from(&[("x-model", "model"), ("x-shared", "model")]));

    let resolution = models
        .get_auth(AuthTarget::Provider("p1".to_string()), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.auth.headers, None);
    let resolution = models
        .get_auth(AuthTarget::Model(Box::new(model.clone())), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolution.auth.headers,
        Some(headers_from(&[("x-model", "model"), ("x-shared", "model")]))
    );

    let transforms = Arc::new(Mutex::new(0u32));
    let transformed_input: Arc<Mutex<Option<ProviderHeaders>>> = Arc::new(Mutex::new(None));
    let transform: HeadersTransform = {
        let transforms = Arc::clone(&transforms);
        let transformed_input = Arc::clone(&transformed_input);
        Arc::new(move |headers: ProviderHeaders| {
            *transforms.lock().unwrap() += 1;
            *transformed_input.lock().unwrap() = Some(headers.clone());
            let mut headers = headers;
            headers.insert("x-transformed".to_string(), Some("yes".to_string()));
            Box::pin(async move { headers })
        })
    };
    models
        .complete_simple(
            &model,
            &test_context(),
            Some(ModelsStreamOptions {
                headers: Some(headers_from(&[
                    ("x-explicit", "explicit"),
                    ("X-Shared", "explicit"),
                ])),
                transform_headers: Some(transform),
                ..Default::default()
            }),
        )
        .await;

    assert_eq!(*transforms.lock().unwrap(), 1);
    assert_eq!(
        transformed_input.lock().unwrap().as_ref(),
        Some(&headers_from(&[
            ("x-model", "model"),
            ("x-explicit", "explicit"),
            ("X-Shared", "explicit"),
        ]))
    );
    let snapshot = calls.lock().unwrap();
    assert_eq!(
        snapshot[0].options.headers.as_ref(),
        Some(&headers_from(&[
            ("x-model", "model"),
            ("x-explicit", "explicit"),
            ("X-Shared", "explicit"),
            ("x-transformed", "yes"),
        ]))
    );
}

// "produces an error stream for unknown providers instead of throwing"
#[tokio::test]
async fn produces_an_error_stream_for_unknown_providers_instead_of_throwing() {
    let models = create_models(Default::default());
    let result = models
        .complete_simple(&test_model("ghost", "model-a"), &test_context(), None)
        .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("Unknown provider: ghost"),
        "{result:?}"
    );
}

// "streams through the provider"
#[tokio::test]
async fn streams_through_the_provider() {
    let models = create_models(Default::default());
    models.set_provider(test_provider(TestProvider {
        id: "p1".into(),
        ..Default::default()
    }));
    let model = test_model("p1", "model-a");

    let stream = models.stream_simple(&model, &test_context(), None);
    let mut events = Vec::new();
    {
        use futures::StreamExt;
        let mut iter = stream.iter();
        while let Some(event) = iter.next().await {
            events.push(event.kind().to_string());
        }
    }
    assert_eq!(events, vec!["start", "done"]);
    let message = stream.result().await;
    assert_eq!(message.stop_reason, StopReason::Stop);
}
