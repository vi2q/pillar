//! Port of packages/coding-agent/src/core/model-runtime.ts (pi v0.84.3):
//! the configured pi-ai `Models` collection used by coding-agent and SDK
//! consumers — builtin/providers.json/extension composition, credential
//! orchestration, availability snapshots, and request preparation.
//!
//! divergence: the remote pi.dev catalog overlay (remote-catalog-provider)
//! and Radius gateway configuration require network fetch plumbing; the port
//! keeps providers static (generated catalog only) and omits the network
//! refresh path. Availability snapshots, credential synchronization, and
//! request preparation mirror the upstream contract.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pillar_ai::abort::AbortSignal;
use pillar_ai::auth_types::{
    AuthCheck, AuthInteraction, AuthOperationOptions, AuthResult, Credential, CredentialInfo,
    CredentialStore,
};
use pillar_ai::error::AiError;
use pillar_ai::models::{
    AuthTarget, CreateModelsOptions, Models, ModelsRefreshOptions, ModelsRefreshResult,
    ModelsStreamOptions, Provider, StreamRequestOptions, create_models, lazy_stream,
    merge_headers_public as merge_headers,
};
use pillar_ai::types::{AssistantMessage, Context, Model, ProviderEnv, ProviderHeaders};

use crate::core::auth_storage::AuthStorage;
use crate::core::auth_storage::{FileModelsStore, InMemoryCodingAgentModelsStore};
use crate::core::model_config::ModelConfig;
use crate::core::provider_composer::{
    AuthStatus, AuthStatusSource, ComposeError, ProviderConfigInput, compose_model_provider,
    configured_request_auth_status, resolve_compatibility_request_config,
    resolve_configured_model_headers,
};
use crate::core::runtime_credentials::RuntimeCredentials;

/// Where a configured auth source comes from (upstream string union).
pub const AUTH_SOURCE_RUNTIME: &str = "runtime";
pub const AUTH_SOURCE_STORED: &str = "stored";

/// Credential operation kinds for synchronization errors (upstream
/// `CredentialSynchronizationOperation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSynchronizationOperation {
    Login,
    Logout,
    SetRuntimeApiKey,
    RemoveRuntimeApiKey,
}

impl CredentialSynchronizationOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Logout => "logout",
            Self::SetRuntimeApiKey => "setRuntimeApiKey",
            Self::RemoveRuntimeApiKey => "removeRuntimeApiKey",
        }
    }
}

/// Credentials changed successfully, but the local model/auth snapshot could
/// not be synchronized (upstream `CredentialSynchronizationError`).
#[derive(Debug, Clone)]
pub struct CredentialSynchronizationError {
    pub provider_id: String,
    pub operation: CredentialSynchronizationOperation,
    pub credential: Option<Credential>,
    pub message: String,
}

impl std::fmt::Display for CredentialSynchronizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credential {} committed for {}, but local synchronization failed: {}",
            self.operation.as_str(),
            self.provider_id,
            self.message
        )
    }
}

impl std::error::Error for CredentialSynchronizationError {}

/// Immutable availability/auth snapshot (upstream `ModelRuntimeSnapshot`).
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeSnapshot {
    pub all: Vec<Model>,
    pub available: Vec<Model>,
    pub configured_providers: BTreeSet<String>,
    pub stored_providers: BTreeSet<String>,
    pub auth: BTreeMap<String, Option<AuthCheck>>,
}

/// Auth resolution overrides (upstream `ModelRuntimeAuthOverrides`).
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeAuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// Require this much remaining OAuth-token validity; defaults to five minutes.
    pub min_oauth_validity_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// Options for creating a `ModelRuntime` (upstream
/// `CreateModelRuntimeOptions`; network/refresh options omitted per the
/// static-catalog divergence).
#[derive(Default)]
pub struct CreateModelRuntimeOptions {
    /// Credential storage. Defaults to the file at `auth_path`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub auth_path: Option<std::path::PathBuf>,
    pub models_path: Option<std::path::PathBuf>,
    pub models_store: Option<Arc<dyn pillar_ai::models_store::ModelsStore>>,
    pub models_store_path: Option<std::path::PathBuf>,
}

/// Configured pi-ai Models collection (upstream `ModelRuntime`).
pub struct ModelRuntime {
    models: Models,
    credentials: Arc<RuntimeCredentials>,
    default_builtins: Vec<Arc<Provider>>,
    native_extension_providers: Vec<Arc<Provider>>,
    extension_providers: BTreeMap<String, ProviderConfigInput>,
    composition_errors: BTreeMap<String, String>,
    models_path: Option<std::path::PathBuf>,
    config: ModelConfig,
    snapshot: std::sync::RwLock<ModelRuntimeSnapshot>,
}

impl ModelRuntime {
    /// Build a runtime with the builtin provider catalog.
    pub fn new(options: CreateModelRuntimeOptions) -> Result<Self, ComposeError> {
        let credentials = Arc::new(RuntimeCredentials::new(
            options.credentials.clone().unwrap_or_else(|| {
                Arc::new(AuthStorage::new(
                    options
                        .auth_path
                        .clone()
                        .unwrap_or_else(|| std::path::PathBuf::from("auth.json")),
                ))
            }),
        ));
        let models_path = options.models_path.clone();
        let config = ModelConfig::load(models_path.as_deref());
        let models_store: Arc<dyn pillar_ai::models_store::ModelsStore> = match options.models_store
        {
            Some(store) => store,
            None => match &models_path {
                Some(path) => Arc::new(FileModelsStore::new(
                    options
                        .models_store_path
                        .clone()
                        .unwrap_or_else(|| path.with_file_name("models-store.json")),
                )),
                None => Arc::new(InMemoryCodingAgentModelsStore::new()),
            },
        };
        let providers = pillar_ai::providers_all::builtin_providers();
        let mut runtime = Self {
            models: create_models(CreateModelsOptions {
                credentials: Some(credentials.clone()),
                models_store: Some(models_store),
                auth_context: None,
            }),
            credentials,
            default_builtins: providers,
            native_extension_providers: Vec::new(),
            extension_providers: BTreeMap::new(),
            composition_errors: BTreeMap::new(),
            models_path,
            config,
            snapshot: std::sync::RwLock::new(ModelRuntimeSnapshot::default()),
        };
        runtime.rebuild_providers()?;
        Ok(runtime)
    }

    fn builtin(&self, provider_id: &str) -> Option<Arc<Provider>> {
        self.default_builtins
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
    }

    fn provider_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for provider in &self.default_builtins {
            ids.insert(provider.id.clone());
        }
        for provider in &self.native_extension_providers {
            ids.insert(provider.id.clone());
        }
        ids.extend(self.config.get_provider_ids());
        ids.extend(self.extension_providers.keys().cloned());
        ids
    }

    fn recompose_provider(&mut self, provider_id: &str) {
        let base = self
            .native_extension_providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .or_else(|| self.builtin(provider_id));
        let extension = self.extension_providers.get(provider_id).cloned();
        let has_config = self.config.get_provider(provider_id).is_some();
        if base.is_none() && !has_config && extension.is_none() {
            self.models.delete_provider(provider_id);
            self.composition_errors.remove(provider_id);
            return;
        }
        if let (Some(base), false, None) = (&base, has_config, &extension) {
            // No overlays: use the builtin untouched so its auth/login/stream
            // behavior is exact.
            self.models.set_provider(Arc::clone(base));
            self.composition_errors.remove(provider_id);
            return;
        }
        match compose_model_provider(
            provider_id,
            base.as_deref(),
            &self.config,
            extension.as_ref(),
        ) {
            Ok(provider) => {
                self.models.set_provider(Arc::new(provider));
                self.composition_errors.remove(provider_id);
            }
            Err(error) => {
                self.composition_errors
                    .insert(provider_id.to_string(), error.0.clone());
                if let Some(base) = base {
                    self.models.set_provider(base);
                } else {
                    self.models.delete_provider(provider_id);
                }
            }
        }
    }

    fn rebuild_providers(&mut self) -> Result<(), ComposeError> {
        self.models.clear_providers();
        self.composition_errors.clear();
        for provider_id in self.provider_ids() {
            self.recompose_provider(&provider_id);
        }
        self.update_model_snapshot();
        Ok(())
    }

    fn update_model_snapshot(&self) {
        let all = self.models.get_models(None);
        let mut snapshot = self.snapshot.write().unwrap();
        let available = all
            .iter()
            .filter(|model| snapshot.configured_providers.contains(&model.provider))
            .cloned()
            .collect();
        snapshot.all = all;
        snapshot.available = available;
    }

    /// Re-derive the availability snapshot from auth checks (upstream
    /// `runAvailabilityRefresh`).
    pub async fn refresh_availability(&self, signal: Option<&AbortSignal>) -> Result<(), AiError> {
        let providers = self.models.get_providers();
        let mut configured_providers = BTreeSet::new();
        let mut auth: BTreeMap<String, Option<AuthCheck>> = BTreeMap::new();
        for provider in &providers {
            let check = self
                .models
                .check_auth(
                    &provider.id,
                    Some(&AuthOperationOptions {
                        signal: signal.cloned(),
                    }),
                )
                .await?;
            auth.insert(provider.id.clone(), check.clone());
            if check.is_some() {
                configured_providers.insert(provider.id.clone());
            }
        }
        let stored_providers: BTreeSet<String> = self
            .credentials
            .list(None)
            .await?
            .into_iter()
            .map(|entry| entry.provider_id)
            .collect();
        let all = self.models.get_models(None);
        let available = all
            .iter()
            .filter(|model| configured_providers.contains(&model.provider))
            .cloned()
            .collect();
        *self.snapshot.write().unwrap() = ModelRuntimeSnapshot {
            all,
            available,
            configured_providers,
            stored_providers,
            auth,
        };
        Ok(())
    }

    // --- passthrough accessors -------------------------------------------------

    pub fn get_providers(&self) -> Vec<Arc<Provider>> {
        self.models.get_providers()
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<Arc<Provider>> {
        self.models.get_provider(provider_id)
    }

    pub fn get_models(&self, provider_id: Option<&str>) -> Vec<Model> {
        self.models.get_models(provider_id)
    }

    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.models.get_model(provider_id, model_id)
    }

    pub async fn check_auth(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<AuthCheck>, AiError> {
        self.models.check_auth(provider_id, options).await
    }

    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<Model>, AiError> {
        self.models.get_available(provider_id, options).await
    }

    pub fn get_available_snapshot(&self) -> Vec<Model> {
        self.snapshot.read().unwrap().available.clone()
    }

    pub fn get_snapshot(&self) -> ModelRuntimeSnapshot {
        self.snapshot.read().unwrap().clone()
    }

    /// Aggregated config/composition/availability errors (upstream
    /// `getError`).
    pub fn get_error(&self) -> Option<String> {
        let mut errors: Vec<String> = Vec::new();
        if let Some(config_error) = self.config.get_error() {
            errors.push(config_error.to_string());
        }
        for (provider_id, error) in &self.composition_errors {
            errors.push(format!("Provider \"{provider_id}\": {error}"));
        }
        if !errors.is_empty() {
            Some(errors.join("\n\n"))
        } else {
            None
        }
    }

    pub fn get_registered_provider_config(
        &self,
        provider_id: &str,
    ) -> Option<&ProviderConfigInput> {
        self.extension_providers.get(provider_id)
    }

    pub fn get_registered_provider_ids(&self) -> Vec<String> {
        let mut ids: BTreeSet<String> = self.extension_providers.keys().cloned().collect();
        ids.extend(
            self.native_extension_providers
                .iter()
                .map(|provider| provider.id.clone()),
        );
        ids.into_iter().collect()
    }

    pub fn get_composition_error(&self, provider_id: &str) -> Option<&String> {
        self.composition_errors.get(provider_id)
    }

    /// Compatibility request config for a model (upstream
    /// `getCompatibilityRequestConfig`).
    pub fn get_compatibility_request_config(
        &self,
        model: &Model,
    ) -> Result<crate::core::provider_composer::CompatibilityRequestConfig, ComposeError> {
        resolve_compatibility_request_config(
            model,
            self.config.get_provider(&model.provider),
            self.extension_providers.get(&model.provider),
        )
    }

    pub fn is_using_oauth(&self, provider_id: &str) -> bool {
        self.snapshot
            .read()
            .unwrap()
            .auth
            .get(provider_id)
            .and_then(|check| check.as_ref())
            .is_some_and(|check| check.kind == "oauth")
    }

    pub fn is_using_subscription(&self, provider_id: &str) -> bool {
        if !self.is_using_oauth(provider_id) {
            return false;
        }
        self.get_provider(provider_id)
            .and_then(|provider| provider.auth.oauth.clone())
            .and_then(|oauth| oauth.is_subscription())
            .unwrap_or(false)
    }

    pub fn has_configured_auth(&self, provider_id: &str) -> bool {
        self.snapshot
            .read()
            .unwrap()
            .configured_providers
            .contains(provider_id)
    }

    /// Resolve provider-scoped auth for a provider id or model, merging the
    /// configured per-model headers (upstream `getAuth`).
    pub async fn get_auth(
        &self,
        target: AuthTarget,
        overrides: Option<&ModelRuntimeAuthOverrides>,
    ) -> Result<Option<AuthResult>, AiError> {
        let pill_overrides = overrides.map(|o| pillar_ai::auth_resolve::AuthResolutionOverrides {
            api_key: o.api_key.clone(),
            env: o.env.clone(),
            min_oauth_validity_ms: o.min_oauth_validity_ms,
            signal: o.signal.clone(),
        });
        let model_ref = match &target {
            AuthTarget::Model(model) => Some((**model).clone()),
            _ => None,
        };
        let resolution = self
            .models
            .get_auth(target, pill_overrides.as_ref())
            .await?;
        let Some(resolution) = resolution else {
            return Ok(None);
        };
        // Model-target: merge configured model headers.
        if let Some(model) = &model_ref {
            let explicit_env: ProviderEnv = resolution
                .env
                .clone()
                .or_else(|| overrides.and_then(|o| o.env.clone()))
                .unwrap_or_default();
            let configured = resolve_configured_model_headers(
                model,
                self.config.get_provider(&model.provider),
                self.extension_providers.get(&model.provider),
                Some(&explicit_env),
            )
            .map_err(|e| AiError::Other(e.0))?;
            let configured: Option<ProviderHeaders> =
                configured.map(|headers| headers.into_iter().map(|(k, v)| (k, Some(v))).collect());
            let mut result = resolution;
            result.auth.headers = merge_headers(result.auth.headers, configured);
            return Ok(Some(result));
        }
        Ok(Some(resolution))
    }

    // --- credentials -----------------------------------------------------------------

    async fn synchronize_credential_state(
        &mut self,
        provider_id: &str,
        operation: CredentialSynchronizationOperation,
        credential: Option<Credential>,
    ) -> Result<(), CredentialSynchronizationError> {
        let result: Result<(), String> = (|| {
            self.recompose_provider(provider_id);
            if let Some(error) = self.composition_errors.get(provider_id) {
                return Err(error.clone());
            }
            Ok(())
        })();
        let mut result = result.map_err(|message| CredentialSynchronizationError {
            provider_id: provider_id.to_string(),
            operation,
            credential: credential.clone(),
            message,
        });
        if result.is_ok() {
            let refresh = self
                .models
                .refresh(Some(ModelsRefreshOptions {
                    allow_network: Some(false),
                    providers: Some(vec![provider_id.to_string()]),
                    ..Default::default()
                }))
                .await;
            if let Some(error) = refresh.errors.get(provider_id) {
                result = Err(CredentialSynchronizationError {
                    provider_id: provider_id.to_string(),
                    operation,
                    credential: credential.clone(),
                    message: error.clone(),
                });
            }
        }
        if result.is_ok() {
            self.update_model_snapshot();
        }
        result
    }

    pub async fn set_runtime_api_key(
        &mut self,
        provider_id: &str,
        api_key: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        self.credentials.set_runtime_api_key(provider_id, api_key);
        self.synchronize_credential_state(
            provider_id,
            CredentialSynchronizationOperation::SetRuntimeApiKey,
            Some(Credential::ApiKey(
                pillar_ai::auth_types::ApiKeyCredential {
                    key: Some(api_key.to_string()),
                    env: None,
                },
            )),
        )
        .await
    }

    pub async fn remove_runtime_api_key(
        &mut self,
        provider_id: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        self.credentials.remove_runtime_api_key(provider_id);
        self.synchronize_credential_state(
            provider_id,
            CredentialSynchronizationOperation::RemoveRuntimeApiKey,
            None,
        )
        .await
    }

    pub async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, AiError> {
        self.credentials.list(None).await
    }

    pub fn get_provider_auth_status(&self, provider_id: &str) -> AuthStatus {
        if self.credentials.has_runtime_api_key(provider_id) {
            return AuthStatus {
                configured: true,
                source: Some(AuthStatusSource::Runtime),
                label: None,
            };
        }
        if self
            .snapshot
            .read()
            .unwrap()
            .stored_providers
            .contains(provider_id)
        {
            return AuthStatus {
                configured: true,
                source: Some(AuthStatusSource::Stored),
                label: None,
            };
        }
        if let Some(configured) = configured_request_auth_status(
            self.config.get_provider(provider_id),
            self.extension_providers.get(provider_id),
        ) {
            return configured;
        }
        let check = self
            .snapshot
            .read()
            .unwrap()
            .auth
            .get(provider_id)
            .and_then(|check| check.as_ref())
            .cloned();
        match check {
            Some(check) => AuthStatus {
                configured: true,
                source: Some(AuthStatusSource::Environment),
                label: check.source,
            },
            None => AuthStatus {
                configured: false,
                source: None,
                label: None,
            },
        }
    }

    pub async fn login(
        &mut self,
        provider_id: &str,
        auth_type: &str,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, CredentialSynchronizationError> {
        let credential = self
            .models
            .login(provider_id, &auth_type.to_string(), interaction)
            .await
            .map_err(|e| CredentialSynchronizationError {
                provider_id: provider_id.to_string(),
                operation: CredentialSynchronizationOperation::Login,
                credential: None,
                message: e.to_string(),
            })?;
        self.synchronize_credential_state(
            provider_id,
            CredentialSynchronizationOperation::Login,
            Some(credential.clone()),
        )
        .await?;
        Ok(credential)
    }

    pub async fn logout(
        &mut self,
        provider_id: &str,
    ) -> Result<(), CredentialSynchronizationError> {
        self.models.logout(provider_id, None).await.map_err(|e| {
            CredentialSynchronizationError {
                provider_id: provider_id.to_string(),
                operation: CredentialSynchronizationOperation::Logout,
                credential: None,
                message: e.to_string(),
            }
        })?;
        self.synchronize_credential_state(
            provider_id,
            CredentialSynchronizationOperation::Logout,
            None,
        )
        .await
    }

    /// Reload models.json, recompose providers, and refresh catalogs (the
    /// port's refresh is offline-only per the static-catalog divergence).
    pub async fn refresh(
        &mut self,
        options: Option<ModelsRefreshOptions>,
    ) -> Result<ModelsRefreshResult, ComposeError> {
        self.config = ModelConfig::load(self.models_path.as_deref());
        self.rebuild_providers()?;
        let mut options = options.unwrap_or_default();
        options.allow_network.get_or_insert(false);
        let result = self.models.refresh(Some(options)).await;
        self.update_model_snapshot();
        Ok(result)
    }

    // --- extension registration --------------------------------------------------------

    /// Register a fully-constructed native extension provider.
    pub fn register_native_provider(&mut self, provider: Provider) -> Result<(), ComposeError> {
        self.register_native_provider_arc(Arc::new(provider))
    }

    /// Arc-taking variant of `register_native_provider`.
    pub fn register_native_provider_arc(
        &mut self,
        provider: Arc<Provider>,
    ) -> Result<(), ComposeError> {
        if provider.id.trim().is_empty() {
            return Err(ComposeError("Provider id must not be empty.".to_string()));
        }
        self.extension_providers.remove(&provider.id);
        self.native_extension_providers
            .retain(|p| p.id != provider.id);
        self.native_extension_providers.push(provider);
        let id = self.native_extension_providers.last().unwrap().id.clone();
        self.recompose_provider(&id);
        self.update_model_snapshot();
        Ok(())
    }

    /// Register a data-only extension provider; re-registration merges
    /// defined values over the previous registration (upstream
    /// `registerProvider` legacy-registry contract).
    pub fn register_provider(
        &mut self,
        provider_id: &str,
        config: ProviderConfigInput,
    ) -> Result<(), ComposeError> {
        // Validate the incoming registration on its own: a broken
        // re-registration must fail without touching the stored config.
        crate::core::provider_composer::validate_extension_provider(
            provider_id,
            &self
                .builtin(provider_id)
                .map(|p| p.models())
                .unwrap_or_default(),
            self.config.get_provider(provider_id),
            &config,
        )?;
        self.native_extension_providers
            .retain(|provider| provider.id != provider_id);
        let previous = self.extension_providers.get(provider_id).cloned();
        let effective = merge_defined(previous, config);
        self.extension_providers
            .insert(provider_id.to_string(), effective);
        self.recompose_provider(provider_id);
        self.update_model_snapshot();
        Ok(())
    }

    pub fn unregister_provider(&mut self, provider_id: &str) {
        self.extension_providers.remove(provider_id);
        self.native_extension_providers
            .retain(|provider| provider.id != provider_id);
        self.recompose_provider(provider_id);
        self.update_model_snapshot();
    }

    // --- streaming -----------------------------------------------------------------------

    /// Prepare a request: resolve auth and merge configured headers (upstream
    /// `prepareRequest`).
    async fn prepare_request(
        &self,
        model: &Model,
        options: Option<&ModelsStreamOptions>,
    ) -> Result<(Arc<Provider>, Model, StreamRequestOptions), AiError> {
        let provider = self.models.get_provider(&model.provider).ok_or_else(|| {
            AiError::Other(format!("provider: Unknown provider: {}", model.provider))
        })?;
        let resolution = self
            .get_auth(
                AuthTarget::Model(Box::new(model.clone())),
                Some(&ModelRuntimeAuthOverrides {
                    api_key: options.and_then(|o| o.api_key.clone()),
                    env: options.and_then(|o| o.env.clone()),
                    min_oauth_validity_ms: None,
                    signal: options.and_then(|o| o.signal.clone()),
                }),
            )
            .await?
            .ok_or_else(|| {
                AiError::Other(format!(
                    "auth: Provider is not configured: {}",
                    model.provider
                ))
            })?;

        let mut headers = merge_headers(
            resolution.auth.headers,
            options.and_then(|o| o.headers.clone()),
        );
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
        if let Some(base_url) = &resolution.auth.base_url {
            request_model.base_url = base_url.clone();
        }
        let request_options = StreamRequestOptions {
            api_key: options
                .and_then(|o| o.api_key.clone())
                .or(resolution.auth.api_key.clone()),
            env,
            headers,
            signal: options.and_then(|o| o.signal.clone()),
            ..Default::default()
        };
        Ok((provider, request_model, request_options))
    }

    pub fn stream(
        self: Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> pillar_ai::event_stream::AssistantMessageEventStream {
        let owned_model = model.clone();
        let owned_context = context.clone();
        let this = self;
        lazy_stream(
            owned_model.clone(),
            Box::pin(async move {
                let (provider, request_model, request_options) =
                    this.prepare_request(&owned_model, options.as_ref()).await?;
                Ok(provider.stream(&request_model, &owned_context, &request_options))
            }),
        )
    }

    pub async fn complete(
        self: Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessage {
        self.stream(model, context, options).result().await
    }

    pub fn stream_simple(
        self: Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> pillar_ai::event_stream::AssistantMessageEventStream {
        let owned_model = model.clone();
        let owned_context = context.clone();
        let this = self;
        lazy_stream(
            owned_model.clone(),
            Box::pin(async move {
                let (provider, request_model, request_options) =
                    this.prepare_request(&owned_model, options.as_ref()).await?;
                Ok(provider.stream_simple(&request_model, &owned_context, &request_options))
            }),
        )
    }

    pub async fn complete_simple(
        self: Arc<Self>,
        model: &Model,
        context: &Context,
        options: Option<ModelsStreamOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options).result().await
    }
}

/// Merge a re-registration's defined values over the previous config,
/// preserving undefined ones (upstream legacy registry contract).
fn merge_defined(
    previous: Option<ProviderConfigInput>,
    next: ProviderConfigInput,
) -> ProviderConfigInput {
    let mut merged = previous.unwrap_or_default();
    if next.name.is_some() {
        merged.name = next.name;
    }
    if next.base_url.is_some() {
        merged.base_url = next.base_url;
    }
    if next.api_key.is_some() {
        merged.api_key = next.api_key;
    }
    if next.api.is_some() {
        merged.api = next.api;
    }
    if next.auth_header.is_some() {
        merged.auth_header = next.auth_header;
    }
    if next.headers.is_some() {
        merged.headers = next.headers;
    }
    if next.models.is_some() {
        merged.models = next.models;
    }
    merged
}
