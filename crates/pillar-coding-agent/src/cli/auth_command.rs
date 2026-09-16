//! Port of packages/coding-agent/src/cli/auth-command.ts (pi v0.84.3): the
//! `pillar auth <command>` parsing and its shared credential extraction.

use pillar_ai::auth_types::AuthResult;

use crate::cli::args::{Args, APP_NAME};

/// Which `auth` subcommand ran (upstream `AuthCommandKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCommandKind {
    Check,
    ApiKey,
    BearerToken,
}

impl AuthCommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::ApiKey => "api_key",
            Self::BearerToken => "bearer_token",
        }
    }

    /// Upstream `getAuthCommandName`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Check => "auth check",
            Self::ApiKey => "auth print-api-key",
            Self::BearerToken => "auth print-bearer-token",
        }
    }

    /// Upstream `getAuthCommandUsage`.
    pub fn usage(self) -> String {
        match self {
            Self::Check => format!(
                "{APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
            ),
            Self::ApiKey => {
                format!("{APP_NAME} auth print-api-key --provider <provider> [--model <model>]")
            }
            Self::BearerToken => format!(
                "{APP_NAME} auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            ),
        }
    }
}

/// A parsed `pillar auth …` invocation (upstream `AuthCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCommand {
    pub kind: AuthCommandKind,
    /// The arguments the normal parser reads (`--provider`, `--model`, …).
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<u64>,
}

/// Upstream `AuthCommandError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCommandError(pub String);

impl std::fmt::Display for AuthCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthCommandError {}

/// Whether the invocation asks for the auth help instead of running (upstream
/// `isAuthCommandHelp`).
pub fn is_auth_command_help(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("auth")
        && (args.get(1).is_none()
            || args.get(1).map(String::as_str) == Some("help")
            || args.iter().any(|arg| arg == "--help" || arg == "-h"))
}

/// Upstream `printAuthCommandHelp`.
pub fn auth_command_help() -> String {
    format!(
        "Usage:\n  {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]\n  {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]\n  {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]\n\nAuth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output."
    )
}

/// Upstream `parseAuthCommand`: `None` when the invocation is not an auth
/// command at all.
pub fn parse_auth_command(args: &[String]) -> Result<Option<AuthCommand>, AuthCommandError> {
    if args.first().map(String::as_str) != Some("auth") {
        return Ok(None);
    }
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        _ => {
            let unknown = args.get(1).cloned().unwrap_or_default();
            return Err(AuthCommandError(format!(
                "Unknown auth command \"{unknown}\". Use \"{APP_NAME} auth print-api-key\", \"{APP_NAME} auth print-bearer-token\", or \"{APP_NAME} auth check\"."
            )));
        }
    };

    let mut command_args: Vec<String> = Vec::new();
    let (mut json, mut credentials, mut no_refresh) = (false, false, false);
    let mut min_expiry_ms: Option<u64> = None;
    let mut index = 2;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--min-expiry" {
            if kind != AuthCommandKind::BearerToken {
                return Err(AuthCommandError(
                    "--min-expiry is only supported by print-bearer-token".to_string(),
                ));
            }
            index += 1;
            let value = args.get(index).cloned().unwrap_or_default();
            min_expiry_ms = Some(parse_duration(&value)?);
            index += 1;
            continue;
        }
        if arg == "--json" || arg == "--credentials" || arg == "--no-refresh" {
            if kind != AuthCommandKind::Check {
                return Err(AuthCommandError(format!(
                    "{arg} is only supported by auth check"
                )));
            }
            match arg {
                "--json" => json = true,
                "--credentials" => credentials = true,
                _ => no_refresh = true,
            }
            index += 1;
            continue;
        }
        command_args.push(arg.to_string());
        index += 1;
    }
    Ok(Some(AuthCommand {
        kind,
        args: command_args,
        json,
        credentials,
        no_refresh,
        min_expiry_ms,
    }))
}

/// Upstream the `--min-expiry` duration (`30m`, `1h`, `500ms`, `10s`).
fn parse_duration(value: &str) -> Result<u64, AuthCommandError> {
    let error = || AuthCommandError("--min-expiry must use a duration such as 30m or 1h".to_string());
    let (amount, multiplier) = if let Some(amount) = value.strip_suffix("ms") {
        (amount, 1)
    } else if let Some(amount) = value.strip_suffix('s') {
        (amount, 1_000)
    } else if let Some(amount) = value.strip_suffix('m') {
        (amount, 60_000)
    } else if let Some(amount) = value.strip_suffix('h') {
        (amount, 3_600_000)
    } else {
        return Err(error());
    };
    let amount: u64 = amount.parse().map_err(|_| error())?;
    Ok(amount * multiplier)
}

/// Upstream `validateAuthCommandArgs`: the provider / model the command acts
/// on, rejecting everything else.
pub fn validate_auth_command_args(
    args: &Args,
    kind: AuthCommandKind,
) -> Result<(Option<String>, Option<String>), AuthCommandError> {
    let provider = args
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let model = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(option) = args.unknown_flags.keys().next() {
        return Err(AuthCommandError(format!(
            "Unknown option --{option} for \"{}\".",
            kind.name()
        )));
    }
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err(AuthCommandError(
            "Auth commands only accept --provider and --model".to_string(),
        ));
    }
    if provider.is_none() && model.is_none() {
        return Err(AuthCommandError(match kind {
            AuthCommandKind::Check => {
                "Auth checks require --provider <provider> or --model <model>".to_string()
            }
            _ => "Credential printing requires --provider <provider> or --model <model>".to_string(),
        }));
    }
    Ok((provider, model))
}

/// Upstream `getAuthCredential`: the API key, or the `Bearer` token of the
/// `Authorization` header.
pub fn get_auth_credential(auth: Option<&AuthResult>) -> Option<String> {
    let auth = auth?;
    if let Some(key) = &auth.auth.api_key {
        return Some(key.clone());
    }
    let authorization = auth.auth.headers.as_ref()?.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("authorization")
            .then_some(value.clone())
            .flatten()
    });
    let bearer = authorization?;
    let (scheme, token) = bearer.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.to_string())
}
