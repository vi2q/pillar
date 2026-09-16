//! The `pillar auth …` commands (upstream `cli/auth-command.ts` +
//! `main.ts`'s `runAuthCommand`).
//!
//! Upstream parses these before the normal option parser, because the kind
//! (`check` / `print-api-key` / `print-bearer-token`) selects which options are
//! legal; the port does the same from the CLI entry point.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use pillar_coding_agent::cli::args::{Args, parse_args};
use pillar_coding_agent::cli::auth_check::{
    AuthCheckReason, AuthCheckResult, AuthCheckStatus, check_provider_auth,
    create_auth_check_model_runtime, get_provider_credential,
};
use pillar_coding_agent::cli::auth_command::{
    AuthCommand, AuthCommandKind, auth_command_help, is_auth_command_help, parse_auth_command,
    validate_auth_command_args,
};
use pillar_coding_agent::cli::credential_print::resolve_credential_for_print;
use pillar_coding_agent::core::auth_storage::{AuthStorage, ReadOnlyAuthStorage};
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_ai::auth_types::CredentialStore;

/// Run one `pillar auth …` invocation. `None` when the arguments are not an
/// auth command, so the caller continues with the normal parsing.
pub async fn run_auth_command(args: &[String], agent_dir: &str) -> Option<i32> {
    if is_auth_command_help(args) {
        println!("{}", auth_command_help());
        return Some(0);
    }
    let command = match parse_auth_command(args) {
        Ok(Some(command)) => command,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("Error: {error}");
            return Some(1);
        }
    };
    let parsed = parse_args(&command.args);
    if let Some(option) = parsed.unknown_flags.keys().next() {
        eprintln!(
            "Unknown option --{option} for \"{}\".",
            command.kind.name()
        );
        eprintln!("Use \"{}\" or \"{}\".", pillar_coding_agent::cli::args::APP_NAME, command.kind.usage());
        return Some(1);
    }
    Some(match run(&command, &parsed, agent_dir).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            if command.kind == AuthCommandKind::Check {
                2
            } else {
                1
            }
        }
    })
}

async fn run(
    command: &AuthCommand,
    parsed: &Args,
    agent_dir: &str,
) -> Result<i32, String> {
    if !parsed.diagnostics.is_empty() {
        return Err(parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if command.kind != AuthCommandKind::Check {
        let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
            auth_path: Some(Path::new(agent_dir).join("auth.json")),
            models_path: Some(Path::new(agent_dir).join("models.json")),
            ..Default::default()
        })
        .map_err(|error| format!("Failed to create the model runtime: {error}"))?;
        let credential = resolve_credential_for_print(
            parsed,
            &runtime,
            command.kind,
            command.min_expiry_ms,
        )
        .await
        .map_err(|error| error.to_string())?;
        println!("{credential}");
        return Ok(0);
    }

    let requested = validate_auth_command_args(parsed, command.kind)
        .map_err(|error| error.to_string())?;
    let auth_path = Path::new(agent_dir).join("auth.json");
    let credentials: Arc<dyn CredentialStore> = if command.no_refresh {
        Arc::new(ReadOnlyAuthStorage::new(&auth_path))
    } else {
        Arc::new(AuthStorage::new(&auth_path))
    };
    let runtime = create_auth_check_model_runtime(Arc::clone(&credentials))?;
    let result = match check_provider_auth(parsed, &runtime, !command.no_refresh).await {
        Ok(result) => result,
        Err(_) => AuthCheckResult {
            status: AuthCheckStatus::Invalid.as_str().to_string(),
            provider: requested
                .0
                .clone()
                .or_else(|| requested.1.clone())
                .unwrap_or_default(),
            reason: Some(AuthCheckReason::InvalidState.as_str().to_string()),
            auth_type: None,
        },
    };
    let mut result = result;
    let mut credential: Option<String> = None;
    if command.credentials && result.status == AuthCheckStatus::Ready.as_str() {
        credential = get_provider_credential(
            &result.provider,
            &runtime,
            credentials.as_ref(),
            !command.no_refresh,
        )
        .await
        .map_err(|error| error.to_string())?;
        if credential.is_none() {
            result.status = AuthCheckStatus::NotReady.as_str().to_string();
            result.reason = Some(AuthCheckReason::CredentialNotAvailable.as_str().to_string());
            result.auth_type = None;
        }
    }
    let output = if command.json {
        let mut value = serde_json::to_value(&result).map_err(|error| error.to_string())?;
        if let Some(credential) = &credential {
            value["credentials"] = serde_json::Value::String(credential.clone());
        }
        value.to_string()
    } else {
        credential.clone().unwrap_or_else(|| result.status.clone())
    };
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{output}");
    Ok(result.exit_code())
}
