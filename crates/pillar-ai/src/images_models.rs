//! Port of packages/ai/src/images-models.ts (pi v0.84.3).
//!
//! Runtime collection of image-generation providers plus auth application
//! and generation convenience: the image-side counterpart of `Models`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth_resolve::{AuthResolutionOverrides, ProviderRef, resolve_provider_auth};
use crate::auth_types::{AuthContext, AuthResult, CredentialStore, ProviderAuth};
use crate::credential_store::InMemoryCredentialStore;
use crate::types::ProviderEnv;

use crate::api::openrouter_images::{
    AssistantImages, ImagesContent, ImagesContext, ImagesModel, ImagesOptions, ImagesStopReason,
};

/// An image-generation provider: the image-side counterpart of `Provider`.
/// Owns id/name metadata, auth, model listing, and generation behavior.
pub struct ImagesProvider {
    pub id: String,
    pub name: String,
    /// At least one of `api_key`/`oauth`. Same semantics as chat providers.
    pub auth: ProviderAuth,
    /// Current known models, sync. Must not panic; `ImagesModels` treats a
    /// panicking implementation as having no models.
    pub get_models: Box<dyn Fn() -> Vec<ImagesModel> + Send + Sync>,
    /// Dynamic providers only: refresh the model list. May fail; the list
    /// stays at its last-known state and a later call retries.
    pub refresh_models: Option<RefreshImagesModelsFn>,
    pub api: ImagesApiImpl,
}

pub type RefreshImagesModelsFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<ImagesModel>, AiError>> + Send + Sync>;

pub type ImagesGenerateFn = Arc<
    dyn Fn(ImagesModel, ImagesContext, Option<ImagesOptions>) -> BoxFuture<'static, AssistantImages>
        + Send
        + Sync,
>;

/// The uniform contract of an image-generation API implementation
/// (upstream `ProviderImages`).
#[derive(Clone, Default)]
pub struct ImagesApiImpl {
    pub generate_images: Option<ImagesGenerateFn>,
}

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
type AiError = crate::error::AiError;

impl ImagesProvider {
    /// Calls the provider's getModels; a panic is caught (upstream treats a
    /// throwing implementation as having no models).
    pub fn models(&self) -> Vec<ImagesModel> {
        let f = &self.get_models;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_default()
    }
}

/// Runtime collection of image-generation providers (upstream
/// `MutableImagesModels`).
pub struct ImagesModels {
    providers: std::sync::Mutex<BTreeMap<String, Arc<ImagesProvider>>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
}

/// Options for `create_images_models` (upstream reuses `CreateModelsOptions`).
#[derive(Default)]
pub struct CreateImagesModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub auth_context: Option<Arc<dyn AuthContext>>,
}

impl Default for ImagesModels {
    fn default() -> Self {
        Self::new(CreateImagesModelsOptions::default())
    }
}

impl ImagesModels {
    pub fn new(options: CreateImagesModelsOptions) -> Self {
        Self {
            providers: std::sync::Mutex::new(BTreeMap::new()),
            credentials: options
                .credentials
                .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            auth_context: options
                .auth_context
                .expect("auth_context required (upstream defaults to process env)"),
        }
    }

    /// Upsert/replace by provider.id. Provider ids are unique.
    pub fn set_provider(&self, provider: Arc<ImagesProvider>) {
        self.providers
            .lock()
            .expect("providers lock")
            .insert(provider.id.clone(), provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.providers.lock().expect("providers lock").remove(id);
    }

    pub fn clear_providers(&self) {
        self.providers.lock().expect("providers lock").clear();
    }

    pub fn get_providers(&self) -> Vec<Arc<ImagesProvider>> {
        self.providers
            .lock()
            .expect("providers lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<ImagesProvider>> {
        self.providers
            .lock()
            .expect("providers lock")
            .get(id)
            .cloned()
    }

    /// Sync read of last-known models from one provider or all providers.
    /// Best-effort: a provider whose `get_models` panics yields no models.
    pub fn get_models(&self, provider: Option<&str>) -> Vec<ImagesModel> {
        match provider {
            Some(provider) => self
                .get_provider(provider)
                .map(|entry| entry.models())
                .unwrap_or_default(),
            None => self
                .get_providers()
                .iter()
                .flat_map(|entry| entry.models())
                .collect(),
        }
    }

    /// Sync runtime model lookup against last-known lists.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<ImagesModel> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    /// Ask a dynamic provider to re-fetch its model list. Fails with
    /// `ModelsError` ("model_source") on the provider's fetch failure.
    pub async fn refresh_one(&self, provider: &str) -> Result<(), AiError> {
        let Some(entry) = self.get_provider(provider) else {
            return Ok(());
        };
        let Some(refresh) = &entry.refresh_models else {
            return Ok(());
        };
        match refresh().await {
            Ok(_refreshed) => Ok(()),
            Err(error) => Err(AiError::Other(format!(
                "model_source: Model refresh failed for {provider}: {error}"
            ))),
        }
    }

    /// Refresh all providers concurrently, best-effort (upstream
    /// `Promise.allSettled`).
    pub async fn refresh_all(&self) {
        let refreshes: Vec<_> = self
            .get_providers()
            .iter()
            .filter_map(|entry| entry.refresh_models.clone())
            .collect();
        futures::future::join_all(refreshes.into_iter().map(|refresh| refresh())).await;
    }

    /// Resolve request auth by provider id. Same contract as
    /// `Models.get_auth()`: `Ok(None)` when unknown/unconfigured.
    pub async fn get_auth(
        &self,
        provider_id: &str,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, AiError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        resolve_provider_auth(
            &ProviderRef {
                id: &provider.id,
                auth: &provider.auth,
            },
            self.credentials.as_ref(),
            self.auth_context.as_ref(),
            overrides,
        )
        .await
    }

    /// Generate images through the owning provider with auth resolved and
    /// merged (explicit options win per field). Never panics; failures are
    /// returned as an `AssistantImages` with `stop_reason: Error`.
    pub async fn generate_images(
        &self,
        model: ImagesModel,
        context: ImagesContext,
        options: Option<ImagesOptions>,
    ) -> AssistantImages {
        let result = self
            .generate_images_inner(model.clone(), context.clone(), options.clone())
            .await;
        match result {
            Ok(output) => output,
            Err(error) => AssistantImages {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                output: Vec::new(),
                response_id: None,
                usage: None,
                stop_reason: ImagesStopReason::Error,
                error_message: Some(error.to_string()),
                timestamp: now_ms(),
            },
        }
    }

    async fn generate_images_inner(
        &self,
        model: ImagesModel,
        context: ImagesContext,
        options: Option<ImagesOptions>,
    ) -> Result<AssistantImages, AiError> {
        let provider = self.get_provider(&model.provider).ok_or_else(|| {
            AiError::Other(format!("provider: Unknown provider: {}", model.provider))
        })?;

        let resolution = self
            .get_auth(
                &model.provider,
                Some(&AuthResolutionOverrides {
                    api_key: options.as_ref().and_then(|o| o.api_key.clone()),
                    env: options.as_ref().and_then(|o| o.env.clone()),
                    signal: options.as_ref().and_then(|o| o.signal.clone()),
                    ..Default::default()
                }),
            )
            .await?;
        let Some(resolution) = resolution else {
            // Unconfigured (resolve -> None) still dispatches; the provider
            // decides what to do.
            return Ok(dispatch(&provider, model, context, options).await);
        };
        let auth = &resolution.auth;

        let mut request_model = model.clone();
        if let Some(base_url) = &auth.base_url {
            request_model.base_url = base_url.clone();
        }

        // Explicit request options win per-field; headers/env merge per key.
        let mut merged_options = options.clone().unwrap_or_default();
        if merged_options.api_key.is_none() {
            merged_options.api_key = auth.api_key.clone();
        }
        if auth.headers.is_some() || merged_options.headers.is_some() {
            let mut headers = auth.headers.clone().unwrap_or_default();
            for (name, value) in merged_options.headers.iter().flat_map(|h| h.iter()) {
                headers.insert(name.clone(), value.clone());
            }
            merged_options.headers = Some(headers);
        }
        if resolution.env.is_some() || merged_options.env.is_some() {
            let mut env: ProviderEnv = resolution.env.clone().unwrap_or_default();
            for (name, value) in merged_options.env.iter().flat_map(|e| e.iter()) {
                env.insert(name.clone(), value.clone());
            }
            merged_options.env = Some(env);
        }

        Ok(dispatch(&provider, request_model, context, Some(merged_options)).await)
    }
}

async fn dispatch(
    provider: &ImagesProvider,
    model: ImagesModel,
    context: ImagesContext,
    options: Option<ImagesOptions>,
) -> AssistantImages {
    match &provider.api.generate_images {
        Some(generate) => generate(model, context, options).await,
        None => AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Error,
            error_message: Some(format!(
                "generateImages: Provider {} has no images API implementation",
                provider.id
            )),
            timestamp: now_ms(),
        },
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Options for `create_images_provider`.
pub struct CreateImagesProviderOptions {
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    pub auth: ProviderAuth,
    /// Initial model list (empty for purely dynamic providers).
    pub models: Vec<ImagesModel>,
    /// Dynamic providers: fetch the current list. Concurrent calls share one
    /// in-flight fetch.
    pub refresh_models: Option<RefreshImagesModelsFn>,
    pub api: ImagesApiImpl,
}

/// Builds an image-generation provider from parts (upstream
/// `createImagesProvider`): dynamic refreshes share one in-flight fetch.
pub fn create_images_provider(input: CreateImagesProviderOptions) -> Arc<ImagesProvider> {
    let models = Arc::new(std::sync::Mutex::new(input.models));
    let name = input.name.unwrap_or_else(|| input.id.clone());

    let refresh_models: Option<RefreshImagesModelsFn> = input.refresh_models.map(|fetch| {
        let models = Arc::clone(&models);
        // In-flight dedupe: a tokio Mutex-guarded slot holds the shared
        // future for the duration of the first caller's refresh; later
        // callers await the same slot (upstream shared-promise semantics).
        let state = Arc::new(tokio::sync::Mutex::<
            Option<tokio::sync::oneshot::Receiver<Result<Vec<ImagesModel>, String>>>,
        >::new(None));
        Arc::new(
            move || -> BoxFuture<'static, Result<Vec<ImagesModel>, AiError>> {
                let fetch = Arc::clone(&fetch);
                let models_slot = Arc::clone(&models);
                let state = Arc::clone(&state);
                Box::pin(async move {
                    let receiver = {
                        let mut guard = state.lock().await;
                        if let Some(receiver) = guard.take() {
                            receiver
                        } else {
                            let (sender, receiver) = tokio::sync::oneshot::channel();
                            *guard = Some(receiver);
                            let guard_slot = state.clone();
                            tokio::spawn(async move {
                                let result = match fetch().await {
                                    Ok(refreshed) => {
                                        *models_slot.lock().expect("models lock") =
                                            refreshed.clone();
                                        Ok(refreshed)
                                    }
                                    Err(error) => Err(error.to_string()),
                                };
                                // Reset the slot after completion so a later
                                // call starts a fresh refresh.
                                *guard_slot.lock().await = None;
                                let _ = sender.send(result);
                            });
                            guard.take().expect("just inserted receiver")
                        }
                    };
                    receiver
                        .await
                        .unwrap_or_else(|_| Err("refresh task dropped".to_string()))
                        .map_err(AiError::Other)
                })
            },
        ) as Arc<_>
    });

    Arc::new(ImagesProvider {
        id: input.id,
        name,
        auth: input.auth,
        get_models: Box::new(move || models.lock().expect("models lock").clone()),
        refresh_models,
        api: input.api,
    })
}

/// Re-export for parity tests exercising provider-free flows.
pub fn empty_images_context() -> ImagesContext {
    ImagesContext {
        input: vec![ImagesContent::Text {
            text: String::new(),
        }],
    }
}
