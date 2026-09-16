//! Port of packages/coding-agent/src/cli/auth-check.ts (pi v0.84.3):
//! `pillar auth check` and the credential read the check exposes.

use std::sync::Arc;

use pillar_ai::auth_types::{Credential, CredentialStore};
use pillar_ai::models::AuthTarget;

use crate::cli::auth_command::{
    AuthCommandError, AuthCommandKind, get_auth_credential, validate_auth_command_args,
};
use crate::cli::args::Args;
use crate::core::auth_storage::InMemoryCodingAgentModelsStore;
use crate::core::model_resolver::{AuthProviders, ResolveCliModelOptions, resolve_cli_model};
use crate::core::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelRuntimeAuthOverrides,
};

/// Upstream `AuthCheckStatus` / `AuthCheckReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheckStatus {
    Ready,
    NotReady,
    Invalid,
}

impl AuthCheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheckReason {
    ProviderNotFound,
    CredentialsNotConfigured,
    CredentialNotAvailable,
    InvalidState,
}

impl AuthCheckReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderNotFound => "provider_not_found",
            Self::CredentialsNotConfigured => "credentials_not_configured",
            Self::CredentialNotAvailable => "credential_not_available",
            Self::InvalidState => "invalid_state",
        }
    }
}

/// Upstream `AuthCheckResult`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCheckResult {
    pub status: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(rename = "authType", skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
}

impl AuthCheckResult {
    fn new(status: AuthCheckStatus, provider: String) -> Self {
        Self {
            status: status.as_str().to_string(),
            provider,
            reason: None,
            auth_type: None,
        }
    }

    fn with_reason(mut self, reason: AuthCheckReason) -> Self {
        self.reason = Some(reason.as_str().to_string());
        self
    }

    pub fn exit_code(&self) -> i32 {
        match self.status.as_str() {
            "ready" => 0,
            "not_ready" => 1,
            _ => 2,
        }
    }
}

/// Upstream `checkProviderAuth`.
pub async fn check_provider_auth(
    args: &Args,
    model_runtime: &ModelRuntime,
    refresh: bool,
) -> Result<AuthCheckResult, AuthCommandError> {
    let (cli_provider, cli_model) = validate_auth_command_args(args, AuthCommandKind::Check)?;
    let provider = resolve_provider(cli_provider, cli_model, model_runtime)?;
    if model_runtime.get_error().is_some() {
        return Ok(AuthCheckResult::new(AuthCheckStatus::Invalid, provider)
            .with_reason(AuthCheckReason::InvalidState));
    }
    if model_runtime.get_provider(&provider).is_none() {
        return Ok(AuthCheckResult::new(AuthCheckStatus::NotReady, provider)
            .with_reason(AuthCheckReason::ProviderNotFound));
    }
    let Ok(auth) = model_runtime.check_auth(&provider, None).await else {
        return Ok(AuthCheckResult::new(AuthCheckStatus::Invalid, provider)
            .with_reason(AuthCheckReason::InvalidState));
    };
    let Some(auth) = auth else {
        return Ok(AuthCheckResult::new(AuthCheckStatus::NotReady, provider)
            .with_reason(AuthCheckReason::CredentialsNotConfigured));
    };
    if refresh {
        let resolved = model_runtime
            .get_auth(AuthTarget::Provider(provider.clone()), None)
            .await;
        if !matches!(resolved, Ok(Some(_))) {
            return Ok(AuthCheckResult::new(AuthCheckStatus::NotReady, provider)
                .with_reason(AuthCheckReason::CredentialsNotConfigured));
        }
    }
    let mut result = AuthCheckResult::new(AuthCheckStatus::Ready, provider);
    result.auth_type = Some(auth.kind);
    Ok(result)
}

/// Map `--model` to its provider (upstream the `resolveCliModel` branch).
fn resolve_provider(
    cli_provider: Option<String>,
    cli_model: Option<String>,
    model_runtime: &ModelRuntime,
) -> Result<String, AuthCommandError> {
    if cli_model.is_none() {
        return cli_provider.ok_or_else(|| {
            AuthCommandError("Unable to resolve an auth provider".to_string())
        });
    }
    let models = model_runtime.get_models(None);
    let auth = AuthProviders(
        model_runtime
            .get_snapshot()
            .configured_providers
            .into_iter()
            .collect(),
    );
    let resolved = resolve_cli_model(ResolveCliModelOptions {
        cli_provider,
        cli_model: cli_model.clone(),
        cli_thinking: None,
        models: &models,
        auth: &auth,
    });
    if let Some(error) = resolved.error {
        return Err(AuthCommandError(error));
    }
    resolved
        .model
        .map(|model| model.provider)
        .ok_or_else(|| {
            AuthCommandError(format!(
                "Unable to resolve model \"{}\"",
                cli_model.unwrap_or_default()
            ))
        })
}

/// Upstream `getProviderCredential`: the stored credential (unless a refresh
/// was asked for), else the resolved request auth.
pub async fn get_provider_credential(
    provider_id: &str,
    model_runtime: &ModelRuntime,
    credentials: &dyn CredentialStore,
    refresh: bool,
) -> Result<Option<String>, String> {
    let credential = credentials.read(provider_id, None).await.map_err(|e| e.to_string())?;
    if !refresh {
        if let Some(Credential::OAuth(oauth)) = credential {
            return Ok(Some(oauth.access));
        }
    }
    let auth = model_runtime
        .get_auth(AuthTarget::Provider(provider_id.to_string()), None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(get_auth_credential(auth.as_ref()))
}

/// Upstream `createAuthCheckModelRuntime`: an offline runtime over the given
/// credential store and an in-memory catalog.
pub fn create_auth_check_model_runtime(
    credentials: Arc<dyn CredentialStore>,
) -> Result<ModelRuntime, String> {
    ModelRuntime::new(CreateModelRuntimeOptions {
        credentials: Some(credentials),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .map_err(|error| format!("Failed to create the model runtime: {error}"))
}

/// The auth overrides a credential print uses (upstream the `minOAuthValidityMs`
/// option).
pub fn print_auth_overrides(min_oauth_validity_ms: Option<u64>) -> ModelRuntimeAuthOverrides {
    ModelRuntimeAuthOverrides {
        min_oauth_validity_ms,
        ..Default::default()
    }
}
