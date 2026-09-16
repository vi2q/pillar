//! The `pillar auth` command surface (`cli/auth-command.ts` +
//! `cli/auth-check.ts` + `cli/credential-print.ts`): parsing, the credential
//! prints, the readiness check, and the exit codes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pillar_ai::auth_types::CredentialStore;
use pillar_cli::auth::run_auth_command;
use pillar_coding_agent::cli::args::parse_args;
use pillar_coding_agent::cli::auth_check::{check_provider_auth, create_auth_check_model_runtime};
use pillar_coding_agent::cli::auth_command::{
    AuthCommandKind, get_auth_credential, parse_auth_command,
};
use pillar_coding_agent::cli::credential_print::resolve_credential_for_print;
use pillar_coding_agent::core::auth_storage::AuthStorage;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};

fn temp_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-auth-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn with_auth(name: &str, content: &str) -> PathBuf {
    let dir = temp_dir(name);
    std::fs::write(dir.join("auth.json"), content).unwrap();
    dir
}

fn runtime_for(agent_dir: &Path) -> ModelRuntime {
    ModelRuntime::new(CreateModelRuntimeOptions {
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: Some(agent_dir.join("models.json")),
        ..Default::default()
    })
    .unwrap()
}

/// Upstream `parseAuthCommand`: the kind, the per-kind options, and the
/// duration parse.
#[test]
fn auth_command_parsing() {
    let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let command = parse_auth_command(&args(&[
        "auth",
        "check",
        "--json",
        "--credentials",
        "--no-refresh",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(command.kind, AuthCommandKind::Check);
    assert!(command.json && command.credentials && command.no_refresh);
    assert!(command.args.is_empty());

    let command = parse_auth_command(&args(&[
        "auth",
        "print-bearer-token",
        "--provider",
        "openai-codex",
        "--min-expiry",
        "30m",
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(command.min_expiry_ms, Some(30 * 60_000));
    assert_eq!(command.args, vec!["--provider", "openai-codex"]);

    assert!(parse_auth_command(&args(&["auth", "check", "--min-expiry", "1h"])).is_err());
    assert!(parse_auth_command(&args(&["auth", "print-api-key", "--json"])).is_err());
    assert!(parse_auth_command(&args(&["auth", "print-api-key", "--min-expiry", "nope"])).is_err());
    assert!(
        parse_auth_command(&args(&["auth", "unknown"]))
            .unwrap_err()
            .to_string()
            .contains("Unknown auth command")
    );
    assert_eq!(parse_auth_command(&args(&["--model", "x"])).unwrap(), None);
}

/// `print-api-key` reads the stored key; OAuth providers answer the bearer
/// token and mismatch with an error.
#[tokio::test]
async fn credential_prints_read_the_configured_provider() {
    let dir = with_auth(
        "print",
        r#"{
            "openai": { "type": "api_key", "key": "sk-test" },
            "openai-codex": { "type": "oauth", "refresh": "r", "access": "Bearer-token", "expires": 99999999999999 }
        }"#,
    );
    let runtime = runtime_for(&dir);
    let args = parse_args(&["--provider".to_string(), "openai".to_string()]);
    assert_eq!(
        resolve_credential_for_print(&args, &runtime, AuthCommandKind::ApiKey, None)
            .await
            .unwrap(),
        "sk-test"
    );
    let error = resolve_credential_for_print(&args, &runtime, AuthCommandKind::BearerToken, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("OAuth"), "{error}");

    let args = parse_args(&["--provider".to_string(), "openai-codex".to_string()]);
    // No builtin provider carries an OAuth implementation (pi's OAuth flows
    // are not ported), so the stored oauth credential cannot be resolved:
    // both prints report the mismatch instead of a credential.
    let error = resolve_credential_for_print(&args, &runtime, AuthCommandKind::BearerToken, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("OAuth"), "{error}");
    let error = resolve_credential_for_print(&args, &runtime, AuthCommandKind::ApiKey, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("OAuth"), "{error}");
}

/// `getAuthCredential`: the API key wins, else the `Authorization` header's
/// `Bearer` token (any case).
#[test]
fn auth_credential_extraction() {
    use pillar_ai::auth_types::{AuthResult, ModelAuth};
    let auth = AuthResult {
        auth: ModelAuth {
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(get_auth_credential(Some(&auth)).as_deref(), Some("sk-test"));

    let auth = AuthResult {
        auth: ModelAuth {
            headers: Some(
                [(
                    "authorization".to_string(),
                    Some("Bearer token-1".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(get_auth_credential(Some(&auth)).as_deref(), Some("token-1"));
    assert_eq!(get_auth_credential(None), None);
}

/// `auth check`: a configured API key is ready; an unconfigured provider is
/// not; the exit codes follow the status.
#[tokio::test]
async fn auth_check_reports_readiness() {
    let dir = with_auth(
        "check",
        r#"{ "openai": { "type": "api_key", "key": "sk-test" } }"#,
    );
    let credentials: Arc<dyn CredentialStore> = Arc::new(AuthStorage::new(dir.join("auth.json")));
    let runtime = create_auth_check_model_runtime(credentials, &dir.join("models.json")).unwrap();

    let args = parse_args(&["--provider".to_string(), "openai".to_string()]);
    let ready = check_provider_auth(&args, &runtime, true).await.unwrap();
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.auth_type.as_deref(), Some("api_key"));
    assert_eq!(ready.exit_code(), 0);

    let args = parse_args(&["--provider".to_string(), "anthropic".to_string()]);
    let missing = check_provider_auth(&args, &runtime, true).await.unwrap();
    assert_eq!(missing.status, "not_ready");
    assert_eq!(
        missing.reason.as_deref(),
        Some("credentials_not_configured")
    );
    assert_eq!(missing.exit_code(), 1);

    let args = parse_args(&["--provider".to_string(), "nope".to_string()]);
    let unknown = check_provider_auth(&args, &runtime, true).await.unwrap();
    assert_eq!(unknown.reason.as_deref(), Some("provider_not_found"));
}

/// A provider that only exists in the agent directory's `models.json` is
/// visible to the check runtime (upstream's `ModelRuntime.create` reads it too;
/// the in-memory store only replaces the catalog cache).
#[tokio::test]
async fn auth_check_sees_configured_providers() {
    use pillar_coding_agent::core::auth_storage::ReadOnlyAuthStorage;

    let dir = temp_dir("configured");
    std::fs::write(
        dir.join("models.json"),
        serde_json::json!({
            "providers": {
                "custom-gateway": {
                    "name": "Custom Gateway",
                    "baseUrl": "https://example.test/v1",
                    "apiKey": "test-key",
                    "api": "openai-completions",
                    "models": [{ "id": "custom-model", "name": "Custom Model" }],
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let runtime = create_auth_check_model_runtime(
        Arc::new(ReadOnlyAuthStorage::new(dir.join("auth.json"))),
        &dir.join("models.json"),
    )
    .unwrap();
    let args = parse_args(&["--provider".to_string(), "custom-gateway".to_string()]);
    let result = check_provider_auth(&args, &runtime, true).await.unwrap();
    assert_ne!(
        result.reason.as_deref(),
        Some("provider_not_found"),
        "{result:?}"
    );
}

/// The CLI entry point: non-auth arguments keep going, the help exits 0, and
/// a malformed auth command exits non-zero.
#[tokio::test]
async fn auth_entry_point_codes() {
    let dir = temp_dir("entry");
    let agent_dir = dir.to_string_lossy().to_string();
    let args = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    assert_eq!(
        run_auth_command(&args(&["--model", "x"]), &agent_dir).await,
        None
    );
    assert_eq!(
        run_auth_command(&args(&["auth"]), &agent_dir).await,
        Some(0)
    );
    assert_eq!(
        run_auth_command(&args(&["auth", "--help"]), &agent_dir).await,
        Some(0)
    );
    assert_eq!(
        run_auth_command(&args(&["auth", "nope"]), &agent_dir).await,
        Some(1)
    );
    assert_eq!(
        run_auth_command(&args(&["auth", "print-api-key"]), &agent_dir).await,
        Some(1)
    );
}
