//! Port of packages/ai/src/models.ts (pi v0.84.3) — the `Models` runtime
//! collection: a provider registry with generation-checked refresh,
//! transactional publication chains, auth application, and stream
//! convenience, plus the `createProvider` factory and the pure model
//! helpers (calculateCost, thinking levels, hasApi, modelsAreEqual).
//!
//! divergence: upstream `Provider` is an object literal with optional
//! methods; the Rust port is a struct of `Option<Arc<dyn …>>` fields with
//! the same observable contract. TS overloads (`getAuth(string | Model)`)
//! map to an enum. `structuredClone` maps to `Clone`; the publication
//! promise chain maps to a per-provider tokio mutex. Upstream's
//! `raceWithAbortSignal(publish…)` rejection maps to `publish` returning
//! `false` — the enclosing refresh operation races the same signal, so the
//! abort still surfaces there. `fetchDeferred`/`cancelDeferred` arrive with
//! the provider API modules.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::abort::{AbortSignal, operation_signal};
use crate::auth_context::DefaultAuthContext;
use crate::auth_resolve::{AuthResolutionOverrides, ProviderRef, resolve_provider_auth};
use crate::auth_types::{
    ApiKeyAuthInput, AuthCheck, AuthContext, AuthInteraction, AuthOperationOptions, AuthResult,
    AuthType, Credential, CredentialModifier, CredentialStore, ProviderAuth,
    ProviderAuthInteraction,
};
use crate::event_stream::{AssistantMessageEventStream, assistant_message_event_stream};
use crate::models_store::{ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, Model, ModelCostRates, ModelThinkingLevel,
    ProviderEnv, ProviderHeaders, StopReason, Usage, UsageCost,
};

// --- Publication / refresh contracts ------------------------------------

/// Provider-selected publication: what to persist and what to update
/// synchronously once the persistence mutation lands.
pub struct ModelsPublication {
    /// Persisted catalog. `None` leaves storage unchanged; `Some(None)`
    /// deletes the stored entry (upstream `persist: null`).
    pub persist: Option<Option<ModelsStoreEntry>>,
    /// Synchronous in-memory catalog update, run only after the selected
    /// persistence mutation.
    pub update: Option<Box<dyn FnOnce() + Send>>,
}

/// Context handed to a provider's `refresh_models` phase (consumed by
/// value; the phase owns its publish closure and signal clones).
pub struct RefreshModelsContext {
    /// Effective configured credential. OAuth credentials are refreshed
    /// before network access.
    pub credential: Option<Credential>,
    /// Immutable provider-scoped catalog snapshot captured before this
    /// refresh phase.
    pub stored: Option<ModelsStoreEntry>,
    /// Generation-checked publication. Returns false when the publication
    /// was superseded or aborted.
    pub publish: PublishFn,
    /// False during offline/cache-only initialization.
    pub allow_network: bool,
    /// Bypass provider freshness checks; present only when network is allowed.
    pub force: Option<bool>,
    /// Always present, even when the public refresh caller omits its signal.
    pub signal: AbortSignal,
}

/// The `publish` callback: queued onto the provider's publication chain. The
/// future is `'static`: the closure owns clones of the shared models state
/// (publication locks, generation map, models store) and the phase signal.
pub struct PublishFn(Box<dyn FnMut(ModelsPublication) -> BoxFuture<'static, bool> + Send>);

impl PublishFn {
    pub fn call(&mut self, publication: ModelsPublication) -> BoxFuture<'static, bool> {
        (self.0)(publication)
    }
}

/// Options for `Models::refresh`.
#[derive(Debug, Clone, Default)]
pub struct ModelsRefreshOptions {
    pub allow_network: Option<bool>,
    /// Restrict refresh to these provider IDs. Unknown and static providers
    /// are ignored.
    pub providers: Option<Vec<String>>,
    /// Bypass provider freshness checks and fetch immediately when network
    /// access is allowed.
    pub force: Option<bool>,
    pub signal: Option<AbortSignal>,
}

/// Result of `Models::refresh`.
#[derive(Debug, Clone, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    /// Per-provider failure messages (upstream: `Map<string, Error>`).
    pub errors: BTreeMap<String, String>,
}

// --- Provider ------------------------------------------------------------

/// Resolved request options handed to a stream implementation (upstream
/// `ProviderRequestOptions` subset). Per-API extras land with the provider
/// API modules.
#[derive(Clone, Default)]
pub struct StreamRequestOptions {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    pub headers: Option<ProviderHeaders>,
    pub signal: Option<AbortSignal>,
    /// Custom HTTP transport (upstream `ProviderRequestOptions.fetch`).
    pub fetch: Option<crate::transport::SharedFetchFn>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// Stream implementation bundle for one API (upstream `ProviderStreams`).
/// Only `stream`/`stream_simple` exist until the provider API modules are
/// ported; deferred methods stay optional hooks.
pub struct ProviderStreams {
    pub stream: StreamFn,
    pub stream_simple: StreamFn,
}

pub type StreamFn = Arc<
    dyn Fn(&Model, &Context, &StreamRequestOptions) -> AssistantMessageEventStream + Send + Sync,
>;

/// Single implementation, or map keyed by `model.api` for mixed-API
/// providers (upstream `ProviderStreams | Partial<Record<TApi, ProviderStreams>>`).
#[derive(Clone, Default)]
pub enum ProviderApi {
    #[default]
    None,
    Single(Arc<ProviderStreams>),
    Map(BTreeMap<String, Arc<ProviderStreams>>),
}

/// The concrete runtime unit: id/name/base metadata, auth methods, model
/// listing, and stream behavior.
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    /// At least one of `api_key`/`oauth`. Every provider has auth semantics —
    /// even ambient/keyless ones provide `api_key` auth whose `resolve`
    /// reports whether the provider is configured.
    pub auth: ProviderAuth,
    /// Current known models, sync. Must not panic; `Models` treats a panicking
    /// implementation as having no models.
    pub get_models: Box<dyn Fn() -> Vec<Model> + Send + Sync>,
    /// Dynamic providers only: restore `context.stored` and optionally fetch
    /// a newer list using the effective credential.
    pub refresh_models: Option<RefreshModelsFn>,
    /// Optional credential-specific availability filter.
    pub filter_models: Option<FilterModelsFn>,
    /// Stream dispatch: a single bundle, or a map keyed by `model.api`.
    pub api: ProviderApi,
}

pub type RefreshModelsFn =
    Arc<dyn Fn(RefreshModelsContext) -> BoxFuture<'static, Result<(), AiError>> + Send + Sync>;

pub type FilterModelsFn = Arc<dyn Fn(&[Model], Option<&Credential>) -> Vec<Model> + Send + Sync>;

pub type FetchModelsFn = Arc<
    dyn for<'a> Fn(&'a RefreshModelsContext) -> BoxFuture<'a, Result<Vec<Model>, AiError>>
        + Send
        + Sync,
>;

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub type AiError = crate::error::AiError;

impl Provider {
    /// Calls the provider's getModels; a panic is caught (upstream treats a
    /// throwing implementation as having no models).
    pub fn models(&self) -> Vec<Model> {
        let f = &self.get_models;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_default()
    }

    fn api_for(&self, model: &Model) -> Option<Arc<ProviderStreams>> {
        match &self.api {
            ProviderApi::Single(streams) => Some(Arc::clone(streams)),
            ProviderApi::Map(map) => map.get(&model.api).cloned(),
            ProviderApi::None => None,
        }
    }

    fn dispatch(
        &self,
        model: &Model,
        run: impl FnOnce(&ProviderStreams) -> AssistantMessageEventStream,
    ) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(streams) => run(&streams),
            None => error_stream(
                model,
                AiError::Other(format!(
                    "stream: Provider {} has no API implementation for \"{}\"",
                    self.id, model.api
                )),
            ),
        }
    }

    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamRequestOptions,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| (streams.stream)(model, context, options))
    }

    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamRequestOptions,
    ) -> AssistantMessageEventStream {
        self.dispatch(model, |streams| {
            (streams.stream_simple)(model, context, options)
        })
    }
}

/// A stream that terminates immediately with an error event (upstream
/// `lazyStream` whose setup throws).
/// Public alias for `error_stream` (used by builtin provider dispatch).
pub fn error_stream_for(model: &Model, error: AiError) -> AssistantMessageEventStream {
    error_stream(model, error)
}

fn error_stream(model: &Model, error: AiError) -> AssistantMessageEventStream {
    lazy_stream(model.clone(), Box::pin(async move { Err(error) }))
}

// --- Request transforms --------------------------------------------------

/// Options for the stream entry points (upstream `ModelsSimpleStreamOptions`
/// minus the per-API extras the provider modules will add).
#[derive(Clone, Default)]
pub struct ModelsStreamOptions {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    pub headers: Option<ProviderHeaders>,
    pub signal: Option<AbortSignal>,
    /// Transform fully assembled model/auth/request headers before provider
    /// dispatch. Consumed by `apply_auth`.
    pub transform_headers: Option<HeadersTransform>,
}

pub type HeadersTransform =
    Arc<dyn Fn(ProviderHeaders) -> BoxFuture<'static, ProviderHeaders> + Send + Sync>;

// --- Auth-check enum (getAuth's string | Model overload) ------------------

/// What to resolve auth for: a provider id or a model (whose provider plus
/// static model headers apply).
#[derive(Debug, Clone)]
pub enum AuthTarget {
    Provider(String),
    Model(Box<Model>),
}

// --- The collection -------------------------------------------------------

/// Shared state behind `Arc`: everything an in-flight refresh, publication,
/// or stream setup needs without borrowing the `Models` handle. All fields
/// and internals are private; reachable only through `Models` (via `Deref`).
pub struct ModelsState {
    providers: std::sync::Mutex<Vec<(String, Arc<Provider>)>>,
    credentials: Arc<dyn CredentialStore>,
    models_store: Arc<dyn ModelsStore>,
    auth_context: Arc<dyn AuthContext>,
    /// Generation counter per provider; bumped to supersede in-flight refreshes.
    refresh_generations: std::sync::Mutex<HashMap<String, u64>>,
    /// Live refresh signals per provider; superseded by generation bump.
    refresh_signals: std::sync::Mutex<HashMap<String, AbortSignal>>,
    /// Per-provider serialization of publication chains (upstream keeps a
    /// promise chain; here a tokio mutex per provider id).
    publication_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ModelsState {
    /// Bumps the generation and aborts any in-flight refresh signal.
    fn supersede_provider_refresh(&self, provider_id: &str) -> u64 {
        let generation = {
            let mut generations = self
                .refresh_generations
                .lock()
                .expect("models generations lock");
            let entry = generations.entry(provider_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        let previous = self
            .refresh_signals
            .lock()
            .expect("models signals lock")
            .remove(provider_id);
        if let Some(signal) = previous {
            signal.abort(None);
        }
        generation
    }

    fn begin_provider_refresh(&self, provider_id: &str) -> (u64, AbortSignal) {
        let generation = self.supersede_provider_refresh(provider_id);
        let controller = AbortSignal::new();
        self.refresh_signals
            .lock()
            .expect("models signals lock")
            .insert(provider_id.to_string(), controller.clone());
        (generation, controller)
    }

    fn publication_lock(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .publication_locks
            .lock()
            .expect("models publication locks");
        Arc::clone(locks.entry(provider_id.to_string()).or_default())
    }

    fn current_generation(&self, provider_id: &str) -> Option<u64> {
        self.refresh_generations
            .lock()
            .expect("models generations lock")
            .get(provider_id)
            .copied()
    }

    /// Generation-checked publication: serialized per provider, persistence
    /// first, then the synchronous update — unless superseded or aborted.
    async fn publish_provider_models(
        self: &Arc<Self>,
        provider_id: &str,
        generation: u64,
        signal: &AbortSignal,
        publication: ModelsPublication,
    ) -> bool {
        let lock = self.publication_lock(provider_id);
        let _guard = lock.lock().await;
        if signal.is_aborted() || self.current_generation(provider_id) != Some(generation) {
            return false;
        }
        let options = ModelsStoreOperationOptions {
            signal: Some(signal.clone()),
        };
        match publication.persist {
            Some(None) => {
                let _ = self.models_store.delete(provider_id, Some(&options)).await;
            }
            Some(Some(entry)) => {
                let _ = self
                    .models_store
                    .write(provider_id, &entry, Some(&options))
                    .await;
            }
            None => {}
        }
        if signal.is_aborted() || self.current_generation(provider_id) != Some(generation) {
            return false;
        }
        if let Some(update) = publication.update {
            update();
        }
        true
    }

    fn publish_fn(
        state: &Arc<ModelsState>,
        provider_id: &str,
        generation: u64,
        signal: &AbortSignal,
    ) -> PublishFn {
        let state = Arc::clone(state);
        let provider_id = provider_id.to_string();
        let signal = signal.clone();
        PublishFn(Box::new(move |publication: ModelsPublication| {
            let state = Arc::clone(&state);
            let provider_id = provider_id.clone();
            let signal = signal.clone();
            Box::pin(async move {
                state
                    .publish_provider_models(&provider_id, generation, &signal, publication)
                    .await
            })
        }))
    }

    async fn run_provider_refresh_phase(
        state: &Arc<ModelsState>,
        provider: &Provider,
        credential: Option<Credential>,
        allow_network: bool,
        force: Option<bool>,
        generation: u64,
        signal: &AbortSignal,
    ) -> Result<(), AiError> {
        let stored = state
            .models_store
            .read(
                &provider.id,
                Some(&ModelsStoreOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await?;

        let refresh = match &provider.refresh_models {
            Some(refresh) => refresh,
            None => return Ok(()),
        };

        let context = RefreshModelsContext {
            credential,
            stored,
            publish: Self::publish_fn(state, &provider.id, generation, signal),
            allow_network,
            force: if allow_network { force } else { None },
            signal: signal.clone(),
        };
        refresh(context).await
    }

    async fn run_provider_operation(
        state: &Arc<ModelsState>,
        provider: &Provider,
        allow_network: bool,
        force: Option<bool>,
        generation: u64,
        signal: &AbortSignal,
    ) -> Result<(), AiError> {
        // Read the stored credential, deferring failures until after the
        // cache-restore phase (upstream: credentialError thrown after phase 1).
        let (stored_credential, credential_error) =
            match state.read_credential(&provider.id, signal).await {
                Ok(credential) => (credential, None),
                Err(error) => (None, Some(error)),
            };

        // Restore cached provider state before auth resolution or network access.
        Self::run_provider_refresh_phase(
            state,
            provider,
            stored_credential.clone(),
            false,
            None,
            generation,
            signal,
        )
        .await?;
        if let Some(error) = credential_error {
            return Err(error);
        }
        if !allow_network || signal.is_aborted() {
            return Ok(());
        }

        let credential = state
            .resolve_refresh_credential(provider, stored_credential, signal)
            .await?;
        let Some(credential) = credential else {
            return Ok(());
        };
        Self::run_provider_refresh_phase(
            state,
            provider,
            Some(credential),
            true,
            force,
            generation,
            signal,
        )
        .await
    }

    async fn read_credential(
        &self,
        provider_id: &str,
        signal: &AbortSignal,
    ) -> Result<Option<Credential>, AiError> {
        self.credentials
            .read(
                provider_id,
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|error| {
                AiError::Other(format!(
                    "auth: Credential store read failed for {provider_id}: {error}"
                ))
            })
    }

    async fn resolve_refresh_credential(
        &self,
        provider: &Provider,
        stored: Option<Credential>,
        signal: &AbortSignal,
    ) -> Result<Option<Credential>, AiError> {
        if let Some(Credential::OAuth(stored)) = &stored {
            let Some(oauth) = &provider.auth.oauth else {
                return Ok(None);
            };
            if now_ms() < stored.expires {
                return Ok(Some(Credential::OAuth(stored.clone())));
            }
            if signal.is_aborted() {
                return Ok(None);
            }
            let oauth = Arc::clone(oauth);
            let signal_for_modify = signal.clone();
            let signal_for_options = signal.clone();
            let provider_id = provider.id.clone();
            let modifier: CredentialModifier<'static> = Box::new(move |current| {
                let oauth = Arc::clone(&oauth);
                let signal = signal_for_modify.clone();
                let provider_id = provider_id.clone();
                Box::pin(async move {
                    let Some(Credential::OAuth(current)) = current else {
                        return Ok(None); // logged out meanwhile
                    };
                    if now_ms() < current.expires {
                        return Ok(None); // already refreshed by someone else
                    }
                    let refreshed = oauth.refresh(&current, &signal).await.map_err(|error| {
                        AiError::Other(format!(
                            "oauth: OAuth refresh failed for {provider_id}: {error}"
                        ))
                    })?;
                    Ok(Some(Credential::OAuth(refreshed)))
                })
            });
            let post = self
                .credentials
                .modify(
                    &provider.id,
                    modifier,
                    Some(&AuthOperationOptions {
                        signal: Some(signal_for_options),
                    }),
                )
                .await?;
            return Ok(match post {
                Some(credential @ Credential::OAuth(_)) => Some(credential),
                _ => None,
            });
        }

        let Some(api_key) = &provider.auth.api_key else {
            return Ok(None);
        };
        let credential = match &stored {
            Some(Credential::ApiKey(cred)) => Some(cred.clone()),
            _ => None,
        };
        let input = ApiKeyAuthInput {
            ctx: self.auth_context.as_ref(),
            credential: credential.as_ref(),
            signal: Some(signal),
        };
        let result = api_key.resolve(&input).await.map_err(|error| {
            AiError::Other(format!(
                "auth: API key auth failed for provider {}: {error}",
                provider.id
            ))
        })?;
        let Some(result) = result else {
            return Ok(None);
        };
        Ok(Some(Credential::ApiKey(
            crate::auth_types::ApiKeyCredential {
                key: result.auth.api_key,
                env: result.env,
            },
        )))
    }

    async fn check_provider_auth(
        &self,
        provider: &Provider,
        credential: Option<&Credential>,
        signal: &AbortSignal,
    ) -> Result<Option<AuthCheck>, AiError> {
        if let Some(Credential::OAuth(_)) = credential {
            return Ok(provider.auth.oauth.as_ref().map(|_| AuthCheck {
                source: Some("OAuth".to_string()),
                kind: "oauth".to_string(),
            }));
        }
        let Some(api_key) = &provider.auth.api_key else {
            return Ok(None);
        };
        if api_key.has_check() {
            let input = ApiKeyAuthInput {
                ctx: self.auth_context.as_ref(),
                credential: match credential {
                    Some(Credential::ApiKey(cred)) => Some(cred),
                    _ => None,
                },
                signal: Some(signal),
            };
            return api_key.check(&input).await.map_err(|error| {
                AiError::Other(format!(
                    "auth: API key auth check failed for provider {}: {error}",
                    provider.id
                ))
            });
        }

        // No explicit check: availability is decided by resolving auth.
        let resolution = resolve_provider_auth(
            &ProviderRef {
                id: &provider.id,
                auth: &provider.auth,
            },
            self.credentials.as_ref(),
            self.auth_context.as_ref(),
            Some(&AuthResolutionOverrides {
                signal: Some(signal.clone()),
                ..Default::default()
            }),
        )
        .await?;
        Ok(resolution.map(|result| AuthCheck {
            source: result.source,
            kind: "api_key".to_string(),
        }))
    }

    fn get_provider(&self, id: &str) -> Option<Arc<Provider>> {
        self.providers
            .lock()
            .expect("models providers lock")
            .iter()
            .find(|(existing, _)| existing == id)
            .map(|(_, provider)| Arc::clone(provider))
    }

    fn get_providers(&self) -> Vec<Arc<Provider>> {
        self.providers
            .lock()
            .expect("models providers lock")
            .iter()
            .map(|(_, provider)| Arc::clone(provider))
            .collect()
    }

    fn require_provider(&self, model: &Model) -> Result<Arc<Provider>, AiError> {
        self.get_provider(&model.provider).ok_or_else(|| {
            AiError::Other(format!("provider: Unknown provider: {}", model.provider))
        })
    }

    /// Internal auth resolution shared by the public `get_auth` and the
    /// request paths.
    async fn get_auth_internal(
        &self,
        target: AuthTarget,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, AiError> {
        let (provider_id, model_headers) = match &target {
            AuthTarget::Provider(id) => (id.clone(), None),
            AuthTarget::Model(model) => (model.provider.clone(), model.headers.clone()),
        };
        let Some(provider) = self.get_provider(&provider_id) else {
            return Ok(None);
        };
        let Some(result) = resolve_provider_auth(
            &ProviderRef {
                id: &provider.id,
                auth: &provider.auth,
            },
            self.credentials.as_ref(),
            self.auth_context.as_ref(),
            overrides,
        )
        .await?
        else {
            return Ok(None);
        };
        let Some(model_headers) = model_headers else {
            return Ok(Some(result));
        };
        let mut result = result;
        result.auth.headers = merge_headers(result.auth.headers, Some(model_headers));
        Ok(Some(result))
    }

    /// Resolve auth and merge it into request options. Explicit options win
    /// per-field; the Models-only transform runs last.
    async fn apply_auth(
        &self,
        model: &Model,
        options: Option<&ModelsStreamOptions>,
    ) -> Result<(Model, StreamRequestOptions), AiError> {
        self.require_provider(model)?;
        let overrides = AuthResolutionOverrides {
            api_key: options.and_then(|o| o.api_key.clone()),
            env: options.and_then(|o| o.env.clone()),
            min_oauth_validity_ms: None,
            signal: options.and_then(|o| o.signal.clone()),
        };
        let resolution = self
            .get_auth_internal(AuthTarget::Model(Box::new(model.clone())), Some(&overrides))
            .await?
            .ok_or_else(|| {
                AiError::Other(format!(
                    "auth: Provider is not configured: {}",
                    model.provider
                ))
            })?;
        let auth = resolution.auth;

        // Explicit request options win per-field; the Models-only transform runs last.
        let api_key = options
            .and_then(|o| o.api_key.clone())
            .or(auth.api_key.clone());
        let mut headers = merge_headers(auth.headers, options.and_then(|o| o.headers.clone()));
        if let Some(transform) = options.and_then(|o| o.transform_headers.as_ref()) {
            headers = Some(transform(headers.unwrap_or_default()).await);
        }
        let env = match (&resolution.env, options.and_then(|o| o.env.clone())) {
            (None, None) => None,
            (resolution_env, options_env) => {
                let mut merged = resolution_env.clone().unwrap_or_default();
                merged.extend(options_env.unwrap_or_default());
                Some(merged)
            }
        };
        let mut request_model = model.clone();
        if let Some(base_url) = &auth.base_url {
            request_model.base_url = base_url.clone();
        }
        let request_options = StreamRequestOptions {
            api_key,
            env,
            headers,
            signal: options.and_then(|o| o.signal.clone()),
            ..Default::default()
        };
        Ok((request_model, request_options))
    }
}

/// Runtime collection of providers plus auth application and stream
/// convenience. Providers own stream behavior; `Models` resolves auth and
/// delegates each request to the provider that owns the model. Cheap to
/// clone; clones share state.
#[derive(Clone)]
pub struct Models {
    state: Arc<ModelsState>,
}

impl std::ops::Deref for Models {
    type Target = ModelsState;

    fn deref(&self) -> &ModelsState {
        &self.state
    }
}

/// Options for `create_models`.
#[derive(Default)]
pub struct CreateModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub models_store: Option<Arc<dyn ModelsStore>>,
    pub auth_context: Option<Arc<dyn AuthContext>>,
}

/// Port of upstream `createModels`.
pub fn create_models(options: CreateModelsOptions) -> Models {
    Models::new(options)
}

impl Models {
    pub fn new(options: CreateModelsOptions) -> Self {
        Self {
            state: Arc::new(ModelsState {
                providers: std::sync::Mutex::new(Vec::new()),
                credentials: options.credentials.unwrap_or_else(|| {
                    Arc::new(crate::credential_store::InMemoryCredentialStore::new())
                }),
                models_store: options
                    .models_store
                    .unwrap_or_else(|| Arc::new(crate::models_store::InMemoryModelsStore::new())),
                auth_context: options
                    .auth_context
                    .unwrap_or_else(|| Arc::new(DefaultAuthContext)),
                refresh_generations: std::sync::Mutex::new(HashMap::new()),
                refresh_signals: std::sync::Mutex::new(HashMap::new()),
                publication_locks: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    // --- Registry --------------------------------------------------------

    /// Upsert/replace by provider.id. Supersedes any in-flight refresh.
    pub fn set_provider(&self, provider: Arc<Provider>) {
        self.supersede_provider_refresh(&provider.id);
        let mut providers = self.providers.lock().expect("models providers lock");
        match providers.iter_mut().find(|(id, _)| *id == provider.id) {
            Some(slot) => slot.1 = provider,
            None => providers.push((provider.id.clone(), provider)),
        }
    }

    pub fn delete_provider(&self, id: &str) {
        self.supersede_provider_refresh(id);
        self.providers
            .lock()
            .expect("models providers lock")
            .retain(|(existing, _)| existing != id);
    }

    pub fn clear_providers(&self) {
        // Collect ids under the locks, then supersede without holding any
        // guard: `supersede_provider_refresh` re-locks the same mutexes.
        let ids: Vec<String> = {
            let providers = self.providers.lock().expect("models providers lock");
            let signals = self.refresh_signals.lock().expect("models signals lock");
            providers
                .iter()
                .map(|(id, _)| id.clone())
                .chain(signals.keys().cloned())
                .collect()
        };
        for id in &ids {
            self.supersede_provider_refresh(id);
        }
        self.providers
            .lock()
            .expect("models providers lock")
            .clear();
    }

    pub fn get_providers(&self) -> Vec<Arc<Provider>> {
        self.state.get_providers()
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<Provider>> {
        self.state.get_provider(id)
    }

    /// Sync read of last-known models from one provider or all providers.
    /// Best-effort: a provider whose getModels panics yields no models.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        match provider {
            Some(id) => match self.get_provider(id) {
                Some(entry) => entry.models(),
                None => Vec::new(),
            },
            None => {
                let mut models = Vec::new();
                for entry in self.get_providers() {
                    models.extend(entry.models());
                }
                models
            }
        }
    }

    /// Sync runtime model lookup against last-known lists.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|m| m.id == id)
    }

    // --- Refresh ---------------------------------------------------------

    /// Refresh selected configured dynamic providers concurrently (all when
    /// `providers` is omitted). Provider errors and cancellation are returned
    /// without failing the call; static, unknown, and unconfigured providers
    /// are skipped.
    pub async fn refresh(&self, options: Option<ModelsRefreshOptions>) -> ModelsRefreshResult {
        let options = options.unwrap_or_default();
        let allow_network = options.allow_network.unwrap_or(true);
        let caller_signal = operation_signal(options.signal.as_ref());
        let errors: Arc<std::sync::Mutex<BTreeMap<String, String>>> = Arc::default();
        if caller_signal.is_aborted() {
            return ModelsRefreshResult {
                aborted: true,
                errors: BTreeMap::new(),
            };
        }
        let selected = options
            .providers
            .map(|list| list.into_iter().collect::<std::collections::HashSet<_>>());
        let refreshable: Vec<Arc<Provider>> = self
            .get_providers()
            .into_iter()
            .filter(|provider| provider.refresh_models.is_some())
            .filter(|provider| {
                selected
                    .as_ref()
                    .map(|set| set.contains(&provider.id))
                    .unwrap_or(true)
            })
            .collect();

        let mut handles = Vec::new();
        for provider in refreshable {
            let (generation, controller) = self.begin_provider_refresh(&provider.id);
            let signal = AbortSignal::any(&[caller_signal.clone(), controller.clone()]);
            let state = Arc::clone(&self.state);
            let errors = Arc::clone(&errors);
            let provider_id = provider.id.clone();
            handles.push(tokio::spawn(async move {
                let operation = ModelsState::run_provider_operation(
                    &state,
                    &provider,
                    allow_network,
                    options.force,
                    generation,
                    &signal,
                );
                match signal.race(operation).await {
                    Ok(()) => {}
                    Err(error) => {
                        if !signal.is_aborted() {
                            errors
                                .lock()
                                .expect("models refresh errors lock")
                                .insert(provider_id.clone(), error.to_string());
                        }
                    }
                }
                // finally: drop the live controller only if still current.
                let mut signals = state.refresh_signals.lock().expect("models signals lock");
                if signals
                    .get(&provider_id)
                    .map(|live| live.same_as(&controller))
                    .unwrap_or(false)
                {
                    signals.remove(&provider_id);
                }
            }));
        }

        // Race the whole batch against the caller signal; per-provider tasks
        // keep running in the background but are superseded via generation.
        let all = futures::future::join_all(handles);
        let batch = async move {
            all.await;
            Ok::<(), AiError>(())
        };
        // Only errors when the caller signal aborted — ignored (upstream:
        // `catch` rethrows only non-abort errors, which cannot happen here).
        let _ = caller_signal.race(batch).await;

        let collected = errors.lock().expect("models refresh errors lock").clone();
        ModelsRefreshResult {
            aborted: caller_signal.is_aborted(),
            errors: collected,
        }
    }

    // --- Auth surface ----------------------------------------------------

    /// Check whether a provider has complete auth configuration without
    /// refreshing OAuth. Unknown provider resolves `Ok(None)`.
    pub async fn check_auth(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<AuthCheck>, AiError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let check = async {
            signal.throw_if_aborted()?;
            let Some(provider) = self.get_provider(provider_id) else {
                return Ok(None);
            };
            let credential = self.read_credential(provider_id, &signal).await?;
            self.check_provider_auth(&provider, credential.as_ref(), &signal)
                .await
        };
        signal.race(check).await
    }

    /// Return models whose providers have complete auth configuration.
    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<Model>, AiError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let available = async {
            signal.throw_if_aborted()?;
            let providers: Vec<Arc<Provider>> = match provider_id {
                Some(id) => self.get_provider(id).into_iter().collect(),
                None => self.get_providers(),
            };
            let mut out = Vec::new();
            for provider in providers {
                let credential = self.read_credential(&provider.id, &signal).await?;
                let Some(auth) = self
                    .check_provider_auth(&provider, credential.as_ref(), &signal)
                    .await?
                else {
                    continue;
                };
                let _ = auth;
                let models = provider.models();
                let models = match &provider.filter_models {
                    Some(filter) => filter(&models, credential.as_ref()),
                    None => models,
                };
                out.extend(models);
            }
            Ok(out)
        };
        signal.race(available).await
    }

    /// Resolve provider-scoped auth by provider id or model. Resolves `None`
    /// when the provider is unknown or unconfigured.
    pub async fn get_auth(
        &self,
        target: AuthTarget,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, AiError> {
        self.get_auth_internal(target, overrides).await
    }

    /// Run a provider-owned login flow and persist its returned credential.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: &AuthType,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, AiError> {
        let signal = operation_signal(interaction.signal());
        signal.throw_if_aborted()?;
        let provider = self
            .get_provider(provider_id)
            .ok_or_else(|| AiError::Other(format!("provider: Unknown provider: {provider_id}")))?;
        let pinteraction = ProviderAuthInteraction {
            interaction,
            signal: &signal,
        };
        let credential: Credential = if auth_type == "oauth" {
            let Some(oauth) = &provider.auth.oauth else {
                return Err(AiError::Other(format!(
                    "auth: {} does not support {auth_type} login",
                    provider.name
                )));
            };
            Credential::OAuth(oauth.login(&pinteraction).await?)
        } else {
            let Some(api_key) = &provider.auth.api_key else {
                return Err(AiError::Other(format!(
                    "auth: {} does not support {auth_type} login",
                    provider.name
                )));
            };
            let Some(credential) = api_key.login(&pinteraction).await? else {
                return Err(AiError::Other(format!(
                    "auth: {} does not support {auth_type} login",
                    provider.name
                )));
            };
            Credential::ApiKey(credential)
        };

        // Persist through the store's only write path. An abort before the
        // mutation starts rejects the login; the store enforces the same
        // boundary (upstream: race started/mutation against abort, then await
        // the mutation).
        let store = Arc::clone(&self.credentials);
        let started = Arc::new(tokio::sync::Notify::new());
        let started_for_fn = Arc::clone(&started);
        let credential_for_fn = credential.clone();
        let modifier: CredentialModifier<'static> = Box::new(move |_current| {
            let started = Arc::clone(&started_for_fn);
            let credential = credential_for_fn.clone();
            Box::pin(async move {
                started.notify_one();
                Ok(Some(credential))
            })
        });
        let mutation_options = AuthOperationOptions {
            signal: Some(signal.clone()),
        };
        let mut mutation = Box::pin(store.modify(provider_id, modifier, Some(&mutation_options)));
        let finish = |result: Result<Option<Credential>, AiError>| -> Result<(), AiError> {
            match result {
                Ok(_) => Ok(()),
                Err(error) => {
                    // Abort rejection wins over a store failure (upstream:
                    // `signal.throwIfAborted()` in the catch).
                    signal.throw_if_aborted()?;
                    Err(AiError::Other(format!(
                        "auth: Credential store modify failed for {provider_id}: {error}"
                    )))
                }
            }
        };
        tokio::select! {
            _ = started.notified() => {}
            reason = signal.aborted_or_pending() => {
                return Err(AiError::Aborted(reason.to_string()));
            }
            result = &mut mutation => {
                finish(result)?;
                return Ok(credential);
            }
        }
        finish(mutation.await)?;
        Ok(credential)
    }

    /// Remove the stored credential for a provider.
    pub async fn logout(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), AiError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        signal.throw_if_aborted()?;
        let delete_options = AuthOperationOptions {
            signal: Some(signal.clone()),
        };
        let delete = self.credentials.delete(provider_id, Some(&delete_options));
        signal.race(delete).await.map_err(|error| {
            // Abort rejection wins over a store failure (upstream: the catch
            // rethrows the abort first).
            match signal.throw_if_aborted() {
                Err(abort) => abort,
                Ok(()) => AiError::Other(format!(
                    "auth: Credential store delete failed for {provider_id}: {error}"
                )),
            }
        })
    }

    // --- Request paths ---------------------------------------------------

    /// Stream through the provider that owns the model. Unknown provider or
    /// unconfigured auth produces an error stream, not a thrown error.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessageEventStream {
        let state = Arc::clone(&self.state);
        let model = model.clone();
        let context = context.clone();
        lazy_stream(
            model.clone(),
            Box::pin(async move {
                let provider = state.require_provider(&model)?;
                let (request_model, request_options) =
                    state.apply_auth(&model, options.as_ref()).await?;
                Ok(provider.stream(&request_model, &context, &request_options))
            }),
        )
    }

    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessage {
        self.stream(model, context, options).result().await
    }

    /// Stream through the provider's `streamSimple` implementation.
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessageEventStream {
        let state = Arc::clone(&self.state);
        let model = model.clone();
        let context = context.clone();
        lazy_stream(
            model.clone(),
            Box::pin(async move {
                let provider = state.require_provider(&model)?;
                let (request_model, request_options) =
                    state.apply_auth(&model, options.as_ref()).await?;
                Ok(provider.stream_simple(&request_model, &context, &request_options))
            }),
        )
    }

    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options).result().await
    }
}

// --- lazyStream ----------------------------------------------------------

/// Port of upstream `lazyStream`: returns a stream synchronously while
/// running async setup (auth resolution, provider dispatch) behind it. Setup
/// failures terminate the stream with an error event.
fn lazy_stream(
    model: Model,
    setup: BoxFuture<'static, Result<AssistantMessageEventStream, AiError>>,
) -> AssistantMessageEventStream {
    let outer = assistant_message_event_stream();
    let sink = outer.clone_stream();
    tokio::spawn(async move {
        match setup.await {
            Ok(inner) => forward_stream(&sink, &inner).await,
            Err(error) => {
                let message = setup_error_message(&model, &error.to_string());
                sink.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: message.clone(),
                });
                sink.end(Some(message));
            }
        }
    });
    outer
}

async fn forward_stream(
    target: &AssistantMessageEventStream,
    source: &AssistantMessageEventStream,
) {
    use futures::StreamExt;
    let mut iter = source.iter();
    while let Some(event) = iter.next().await {
        target.push(event);
    }
    let final_message = source.result().await;
    target.end(Some(final_message));
}

fn setup_error_message(model: &Model, error: &str) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some(error.to_string()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

// --- createProvider ------------------------------------------------------

/// Options for `create_provider`.
#[derive(Default)]
pub struct CreateProviderOptions {
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    /// Required — every provider has auth semantics, even ambient/keyless ones.
    pub auth: ProviderAuth,
    /// Static baseline model list (empty for purely dynamic providers).
    pub models: Vec<Model>,
    /// Fetch a dynamic model overlay. createProvider restores and publishes
    /// it transactionally.
    pub fetch_models: Option<FetchModelsFn>,
    pub filter_models: Option<FilterModelsFn>,
    /// Single implementation, or map keyed by `model.api` for mixed-API
    /// providers. A model whose api has no entry produces a stream error.
    pub api: ProviderApi,
}

/// Builds a provider from parts. Built-in provider factories and models.json
/// custom providers both go through this.
pub fn create_provider(input: CreateProviderOptions) -> Arc<Provider> {
    let id = input.id.clone();
    let baseline_models = input.models;
    let dynamic_models: Arc<std::sync::Mutex<Vec<Model>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let current_models = {
        let dynamic = Arc::clone(&dynamic_models);
        let baseline = baseline_models.clone();
        Box::new(move || -> Vec<Model> {
            let dynamic = dynamic.lock().expect("dynamic models lock").clone();
            let mut merged = baseline.clone();
            for model in dynamic {
                match merged.iter_mut().find(|entry| entry.id == model.id) {
                    Some(slot) => *slot = model,
                    None => merged.push(model),
                }
            }
            merged
        }) as Box<dyn Fn() -> Vec<Model> + Send + Sync>
    };

    let refresh_models: Option<RefreshModelsFn> = input.fetch_models.map(|fetch| {
        let dynamic = Arc::clone(&dynamic_models);
        let id = id.clone();
        let refresh: RefreshModelsFn = Arc::new(move |mut context: RefreshModelsContext| {
            let dynamic = Arc::clone(&dynamic);
            let id = id.clone();
            let fetch = Arc::clone(&fetch);
            Box::pin(async move {
                if let Some(stored) = context.stored.clone() {
                    let restored: Vec<Model> = stored
                        .models
                        .into_iter()
                        .filter(|model| model.provider == id)
                        .collect();
                    let dynamic_for_restore = Arc::clone(&dynamic);
                    let published = context
                        .publish
                        .call(ModelsPublication {
                            persist: None,
                            update: Some(Box::new(move || {
                                *dynamic_for_restore.lock().expect("dynamic models lock") =
                                    restored;
                            })),
                        })
                        .await;
                    if !published {
                        return Ok(());
                    }
                }
                if !context.allow_network || context.signal.is_aborted() {
                    return Ok(());
                }
                let refreshed = fetch(&context).await?;
                if context.signal.is_aborted() {
                    return Ok(());
                }
                let dynamic_for_update = Arc::clone(&dynamic);
                let refreshed_for_update = refreshed.clone();
                context
                    .publish
                    .call(ModelsPublication {
                        persist: Some(Some(ModelsStoreEntry {
                            models: refreshed,
                            last_modified: None,
                            checked_at: Some(now_ms()),
                            etag: None,
                        })),
                        update: Some(Box::new(move || {
                            *dynamic_for_update.lock().expect("dynamic models lock") =
                                refreshed_for_update;
                        })),
                    })
                    .await;
                Ok(())
            })
        });
        refresh
    });

    Arc::new(Provider {
        id,
        name: input.name.unwrap_or_else(|| input.id.clone()),
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        get_models: current_models,
        refresh_models,
        filter_models: input.filter_models,
        api: input.api,
    })
}

// --- Pure helpers ---------------------------------------------------------

/// Runtime-checked narrowing for dynamically looked-up models (upstream
/// `hasApi`): in Rust this is an equality check on `model.api`.
pub fn has_api(model: &Model, api: &str) -> bool {
    model.api == api
}

/// Port of upstream `calculateCost`: tiered pricing — the highest matching
/// threshold applies to the full request — plus Anthropic's 2x base input
/// rate for 1h cache writes. Mutates `usage.cost` and returns it.
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> UsageCost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates: ModelCostRates = model.cost.rates;
    let mut matched_threshold: i64 = -1;
    for tier in model.cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above
            && (tier.input_tokens_above as i64) > matched_threshold
        {
            rates = tier.rates;
            matched_threshold = tier.input_tokens_above as i64;
        }
    }

    // Anthropic charges 2x base input for 1h cache writes.
    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write - long_write;
    usage.cost.input = (rates.input / 1_000_000.0) * usage.input as f64;
    usage.cost.output = (rates.output / 1_000_000.0) * usage.output as f64;
    usage.cost.cache_read = (rates.cache_read / 1_000_000.0) * usage.cache_read as f64;
    usage.cost.cache_write = (rates.cache_write * short_write as f64
        + rates.input * 2.0 * long_write as f64)
        / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level));
            match mapped {
                Some(None) => false, // explicitly unsupported
                Some(Some(_)) => true,
                None => !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max),
            }
        })
        .collect()
}

pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested_index = match EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) {
        Some(index) => index,
        None => {
            return available
                .first()
                .copied()
                .unwrap_or(ModelThinkingLevel::Off);
        }
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

/// Check if two models are equal by comparing both their id and provider.
/// `None` inputs are never equal (upstream returns false for null).
pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}

// --- Shared plumbing ------------------------------------------------------

fn merge_headers(
    base: Option<ProviderHeaders>,
    override_headers: Option<ProviderHeaders>,
) -> Option<ProviderHeaders> {
    if base.is_none() && override_headers.is_none() {
        return None;
    }
    let mut merged = base.unwrap_or_default();
    for (name, value) in override_headers.unwrap_or_default() {
        let lower = name.to_lowercase();
        merged.retain(|existing, _| existing.to_lowercase() != lower);
        merged.insert(name, value);
    }
    Some(merged)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
