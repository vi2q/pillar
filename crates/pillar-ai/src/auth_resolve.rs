//! Port of packages/ai/src/auth/resolve.ts (pi v0.84.3) — auth resolution
//! shared by the `Models` and `ImagesModels` collections. A stored credential
//! owns the provider: ambient/env is consulted only when nothing is stored.
//! No silent env fallback after a failed refresh or for a credential type
//! without a matching handler.

use std::sync::Arc;

use async_trait::async_trait;

use crate::abort::{AbortSignal, operation_signal};
use crate::auth_types::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthResult, Credential, CredentialStore, OAuthAuth,
    OAuthCredential, ProviderAuth,
};
use crate::error::AiError;

pub type ModelsErrorCode = String; // "model_source" | "model_validation" | "provider" | "stream" | "auth" | "oauth"

pub const MODEL_ERROR_CODES: [&str; 6] = [
    "model_source",
    "model_validation",
    "provider",
    "stream",
    "auth",
    "oauth",
];

/// Upstream `ModelsError`: an error with a machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn to_error(self) -> AiError {
        AiError::Other(format!("{}: {}", self.code, self.message))
    }
}

/// Overrides for auth resolution.
#[derive(Debug, Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<crate::auth_types::ProviderEnv>,
    /// Require this much remaining OAuth-token validity (ms); defaults to 5 minutes.
    pub min_oauth_validity_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// Auth resolution shared by the `Models` and `ImagesModels` collections.
/// A stored credential owns the provider: ambient/env is consulted only when
/// nothing is stored. No silent env fallback after a failed refresh or for a
/// credential type without a matching handler.
pub async fn resolve_provider_auth(
    provider: &ProviderRef<'_>,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
) -> Result<Option<AuthResult>, AiError> {
    let signal = operation_signal(overrides.and_then(|o| o.signal.as_ref()));
    signal
        .race(resolve_provider_auth_with_signal(
            provider,
            credentials,
            auth_context,
            overrides,
            &signal,
        ))
        .await
}

/// Minimal provider view needed by resolution (id + auth).
pub struct ProviderRef<'a> {
    pub id: &'a str,
    pub auth: &'a ProviderAuth,
}

async fn resolve_provider_auth_with_signal(
    provider: &ProviderRef<'_>,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
    signal: &AbortSignal,
) -> Result<Option<AuthResult>, AiError> {
    signal
        .throw_if_aborted()
        .map_err(|e| AiError::Other(e.to_string()))?;
    let request_auth_context: &dyn AuthContext = match overrides.and_then(|o| o.env.as_ref()) {
        Some(env) => &OverlayEnvAuthContext {
            base: auth_context,
            env: env.clone(),
        },
        None => auth_context,
    };

    if let (Some(api_key), Some(auth_method)) = (
        overrides.and_then(|o| o.api_key.clone()),
        provider.auth.api_key.as_ref(),
    ) {
        return resolve_api_key(
            request_auth_context,
            auth_method.as_ref(),
            provider.id,
            Some(&ApiKeyCredential {
                key: Some(api_key),
                env: overrides.and_then(|o| o.env.clone()),
            }),
            signal,
        )
        .await;
    }

    let stored = read_credential(credentials, provider.id, signal).await?;
    if let Some(stored) = stored {
        match (
            &stored,
            provider.auth.oauth.as_ref(),
            provider.auth.api_key.as_ref(),
        ) {
            (Credential::OAuth(oauth_cred), Some(oauth), _) => {
                return resolve_stored_oauth(
                    credentials,
                    provider.id,
                    Arc::clone(oauth),
                    oauth_cred,
                    signal,
                    overrides.and_then(|o| o.min_oauth_validity_ms),
                )
                .await;
            }
            (Credential::ApiKey(cred), _, Some(api_key)) => {
                let mut cred = cred.clone();
                if let Some(env) = overrides.and_then(|o| o.env.clone()) {
                    cred.env = Some(match cred.env.take() {
                        Some(existing) => {
                            let mut merged = existing;
                            merged.extend(env);
                            merged
                        }
                        None => env,
                    });
                }
                return resolve_api_key(
                    request_auth_context,
                    api_key.as_ref(),
                    provider.id,
                    Some(&cred),
                    signal,
                )
                .await;
            }
            _ => return Ok(None),
        }
    }

    // Ambient (env vars, AWS profiles, ADC files).
    match provider.auth.api_key.as_ref() {
        Some(api_key) => {
            resolve_api_key(
                request_auth_context,
                api_key.as_ref(),
                provider.id,
                None,
                signal,
            )
            .await
        }
        None => Ok(None),
    }
}

/// AuthContext with provider-env overrides layered over ambient env lookups.
struct OverlayEnvAuthContext<'a> {
    base: &'a dyn AuthContext,
    env: crate::auth_types::ProviderEnv,
}

#[async_trait]
impl AuthContext for OverlayEnvAuthContext<'_> {
    async fn env(&self, name: &str) -> Option<String> {
        if let Some(value) = self.env.get(name)
            && !value.is_empty()
        {
            return Some(value.clone());
        }
        self.base.env(name).await
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.base.file_exists(path).await
    }
}

const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;
const DEFAULT_OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

/// OAuth resolution with double-checked locking: tokens with less than five
/// minutes remaining lock, re-check expiry under the lock, refresh once
/// globally, and persist the rotated credential before release.
async fn resolve_stored_oauth(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    oauth: Arc<dyn OAuthAuth>,
    stored: &OAuthCredential,
    signal: &AbortSignal,
    min_oauth_validity_ms: Option<u64>,
) -> Result<Option<AuthResult>, AiError> {
    let minimum_validity_ms =
        DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_oauth_validity_ms.unwrap_or(0));
    let expires_soon =
        |credential: &OAuthCredential| now_ms() + minimum_validity_ms >= credential.expires;
    let mut credential = stored.clone();

    if expires_soon(&credential) {
        // Optimistic check said expired; the authoritative check runs under the lock.
        let refresh_signal = {
            let inner = AbortSignal::timeout(std::time::Duration::from_millis(
                DEFAULT_OAUTH_REFRESH_TIMEOUT_MS,
            ));
            AbortSignal::any(&[signal.clone(), inner])
        };
        let oauth_for_modify: Arc<dyn OAuthAuth> = Arc::clone(&oauth);
        let refresh_signal_for_modify = refresh_signal.clone();
        let signal_for_modify = signal.clone();
        let provider_id_owned = provider_id.to_string();
        let provider_id_for_err = provider_id_owned.clone();
        let post = credentials
            .modify(
                provider_id,
                Box::new(move |current| {
                    Box::pin(async move {
                        let current = match current {
                            Some(Credential::OAuth(c))
                                if expires_soon_static(&c, minimum_validity_ms) =>
                            {
                                c
                            }
                            _ => return Ok(None), // logged out meanwhile or already refreshed
                        };
                        let refreshed = oauth_for_modify
                            .refresh(&current, &refresh_signal_for_modify)
                            .await
                            .map_err(|error| {
                                AiError::Other(format!(
                                    "oauth: OAuth refresh failed for {provider_id_for_err}: {error}"
                                ))
                            })?;
                        Ok(Some(Credential::OAuth(refreshed)))
                    })
                }),
                Some(&crate::auth_types::AuthOperationOptions {
                    signal: Some(signal_for_modify),
                }),
            )
            .await
            .map_err(|error| match error {
                AiError::Other(ref msg) if msg.starts_with("oauth:") => error,
                other => AiError::Other(format!(
                    "auth: Credential store modify failed for {provider_id_owned}: {other}"
                )),
            })?;
        let post = match post {
            Some(Credential::OAuth(c)) => c,
            _ => return Ok(None), // logged out meanwhile
        };
        credential = post;
        // The normal five-minute window triggers a refresh but does not impose a
        // provider contract. Explicit callers (such as bearer-token export) do
        // require the requested minimum after the refresh.
        if min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(AiError::Other(format!(
                "oauth: OAuth refresh returned a token that expires too soon for {provider_id}"
            )));
        }
    }

    match oauth.to_auth(&credential).await {
        Ok(auth) => Ok(Some(AuthResult {
            auth,
            env: None,
            source: Some("OAuth".to_string()),
        })),
        Err(error) => Err(AiError::Other(format!(
            "oauth: OAuth auth derivation failed for {provider_id}: {error}"
        ))),
    }
}

fn expires_soon_static(credential: &OAuthCredential, minimum_validity_ms: u64) -> bool {
    now_ms() + minimum_validity_ms >= credential.expires
}

async fn resolve_api_key(
    auth_context: &dyn AuthContext,
    api_key: &dyn ApiKeyAuth,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
    signal: &AbortSignal,
) -> Result<Option<AuthResult>, AiError> {
    let input = crate::auth_types::ApiKeyAuthInput {
        ctx: auth_context,
        credential,
        signal: Some(signal),
    };
    match api_key.resolve(&input).await {
        Ok(result) => Ok(result),
        Err(error) => Err(AiError::Other(format!(
            "auth: API key auth failed for provider {provider_id}: {error}"
        ))),
    }
}

async fn read_credential(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    signal: &AbortSignal,
) -> Result<Option<Credential>, AiError> {
    credentials
        .read(
            provider_id,
            Some(&crate::auth_types::AuthOperationOptions {
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
