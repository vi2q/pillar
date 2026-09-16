//! Port of packages/coding-agent/src/cli/credential-print.ts (pi v0.84.3):
//! `pillar auth print-api-key` / `print-bearer-token`.

use std::collections::BTreeMap;

use pillar_ai::auth_types::CredentialInfo;
use pillar_ai::models::AuthTarget;

use crate::cli::auth_check::print_auth_overrides;
use crate::cli::auth_command::{
    AuthCommandError, AuthCommandKind, get_auth_credential, validate_auth_command_args,
};
use crate::cli::args::Args;
use crate::core::model_resolver::{AuthProviders, ResolveCliModelOptions, resolve_cli_model};
use crate::core::model_runtime::ModelRuntime;

/// The bearer-token validity the print requires unless `--min-expiry` overrides
/// it (upstream `DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS`).
pub const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: u64 = 30 * 60_000;

/// Upstream `resolveCredentialForPrint`: the one configured credential the
/// command prints. `getAuth` refreshes and persists nearly expired OAuth
/// credentials through the normal request-auth path.
pub async fn resolve_credential_for_print(
    args: &Args,
    model_runtime: &ModelRuntime,
    kind: AuthCommandKind,
    min_expiry_ms: Option<u64>,
) -> Result<String, AuthCommandError> {
    let (cli_provider, cli_model) = validate_auth_command_args(args, kind)?;
    let credential_types: BTreeMap<String, String> = model_runtime
        .list_credentials()
        .await
        .map_err(|error| AuthCommandError(error.to_string()))?
        .into_iter()
        .map(|CredentialInfo { provider_id, kind }| (provider_id, kind))
        .collect();

    let mut providers: Vec<(String, Option<pillar_ai::types::Model>)> = Vec::new();
    match (&cli_provider, &cli_model) {
        (Some(provider_id), model) => {
            let Some(provider) = model_runtime.get_provider(provider_id) else {
                return Err(AuthCommandError(format!(
                    "Unknown provider \"{provider_id}\". Use --list-models to see available providers."
                )));
            };
            match model {
                Some(_) => {
                    let model = resolve_model(Some(provider.id.clone()), cli_model.clone(), model_runtime)?;
                    providers.push((provider.id.clone(), Some(model)));
                }
                None => providers.push((provider.id.clone(), None)),
            }
        }
        (None, Some(model_id)) => {
            for provider in model_runtime.get_providers() {
                if !credential_types.contains_key(&provider.id) {
                    continue;
                }
                if let Some(model) = resolve_model_loose(
                    Some(provider.id.clone()),
                    Some(model_id.clone()),
                    model_runtime,
                ) {
                    providers.push((provider.id.clone(), Some(model)));
                }
            }
            if providers.is_empty() {
                return Err(AuthCommandError(format!(
                    "Model \"{model_id}\" not found. Use --list-models to see available models."
                )));
            }
        }
        (None, None) => {
            return Err(AuthCommandError(
                "Credential printing requires --provider <provider> or --model <model>".to_string(),
            ));
        }
    }

    let mut credentials: Vec<(String, String)> = Vec::new();
    for (provider_id, model) in &providers {
        let Some(kind_of) = credential_types.get(provider_id) else {
            continue;
        };
        if kind == AuthCommandKind::ApiKey && kind_of == "oauth" {
            continue;
        }
        if kind == AuthCommandKind::BearerToken && kind_of != "oauth" {
            continue;
        }
        let overrides = (kind == AuthCommandKind::BearerToken).then(|| {
            print_auth_overrides(Some(
                min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS),
            ))
        });
        let target = match model {
            Some(model) => AuthTarget::Model(Box::new(model.clone())),
            None => AuthTarget::Provider(provider_id.clone()),
        };
        let auth = model_runtime
            .get_auth(target, overrides.as_ref())
            .await
            .map_err(|error| AuthCommandError(error.to_string()))?;
        if let Some(value) = get_auth_credential(auth.as_ref()) {
            credentials.push((provider_id.clone(), value));
        }
    }

    if credentials.len() == 1 {
        return Ok(credentials[0].1.clone());
    }
    if credentials.is_empty() {
        let provider_id = providers.first().map(|(id, _)| id.clone());
        let configured = provider_id.as_ref().and_then(|id| credential_types.get(id));
        if cli_provider.is_some() && kind == AuthCommandKind::ApiKey && configured == Some(&"oauth".to_string())
        {
            return Err(AuthCommandError(format!(
                "Provider \"{}\" is configured with OAuth, not an API key",
                provider_id.unwrap_or_default()
            )));
        }
        if cli_provider.is_some()
            && kind == AuthCommandKind::BearerToken
            && configured != Some(&"oauth".to_string())
        {
            return Err(AuthCommandError(format!(
                "Provider \"{}\" is not configured with an OAuth bearer token",
                provider_id.unwrap_or_default()
            )));
        }
        return Err(AuthCommandError(format!(
            "No usable {} is configured",
            if kind == AuthCommandKind::ApiKey {
                "API key"
            } else {
                "OAuth bearer token"
            }
        )));
    }
    Err(AuthCommandError(format!(
        "Multiple configured providers matched ({}). Specify --provider.",
        credentials
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// The `resolveCliModel` call both branches share.
fn resolve_model(
    cli_provider: Option<String>,
    cli_model: Option<String>,
    model_runtime: &ModelRuntime,
) -> Result<pillar_ai::types::Model, AuthCommandError> {
    let resolved = resolve_cli_model_for(cli_provider, cli_model, model_runtime);
    if let Some(error) = resolved.error {
        return Err(AuthCommandError(error));
    }
    resolved
        .model
        .ok_or_else(|| AuthCommandError("Unable to resolve the requested provider/model".to_string()))
}

/// The model-only scan skips providers that fail to resolve and the
/// "Using custom model id" fallback, so the scan picks the provider the model
/// actually belongs to (upstream the warning filter).
fn resolve_model_loose(
    cli_provider: Option<String>,
    cli_model: Option<String>,
    model_runtime: &ModelRuntime,
) -> Option<pillar_ai::types::Model> {
    let resolved = resolve_cli_model_for(cli_provider, cli_model, model_runtime);
    if resolved.error.is_some()
        || resolved
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("Using custom model id"))
    {
        return None;
    }
    resolved.model
}

fn resolve_cli_model_for(
    cli_provider: Option<String>,
    cli_model: Option<String>,
    model_runtime: &ModelRuntime,
) -> crate::core::model_resolver::ResolveCliModelResult {
    let models = model_runtime.get_models(None);
    let auth = AuthProviders(
        model_runtime
            .get_snapshot()
            .configured_providers
            .into_iter()
            .collect(),
    );
    resolve_cli_model(ResolveCliModelOptions {
        cli_provider,
        cli_model,
        cli_thinking: None,
        models: &models,
        auth: &auth,
    })
}
