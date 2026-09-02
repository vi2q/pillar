//! Port of packages/coding-agent/src/core/resolve-config-value.ts (pi
//! v0.84.3).
//!
//! Resolve configuration values that may be shell commands (`!command`),
//! environment-variable templates (`$VAR` / `${VAR}`), or literals.
//! Used by auth storage and the model registry.
//!
//! divergence: command execution shells out via `std::process::Command` with
//! `sh -c` (upstream uses the configured shell on Windows and `execSync`
//! elsewhere — the Rust port targets POSIX hosts, so a single path covers
//! both upstream branches); the result cache is global with an explicit
//! clear, matching upstream process-lifetime semantics.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub type Env = BTreeMap<String, String>;

const ENV_VAR_NAME_RE_FIRST: fn(char) -> bool = |c| c.is_ascii_alphabetic() || c == '_';
const ENV_VAR_NAME_RE_REST: fn(char) -> bool = |c| c.is_ascii_alphanumeric() || c == '_';

/// Cache for shell command results (persists for process lifetime).
fn command_result_cache() -> &'static Mutex<BTreeMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Debug, Clone, PartialEq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

#[derive(Debug, Clone)]
enum ConfigValueReference {
    Command { config: String },
    Template { parts: Vec<TemplatePart> },
}

fn append_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(previous)) = parts.last_mut() {
        previous.push_str(value);
        return;
    }
    parts.push(TemplatePart::Literal(value.to_string()));
}

/// Upstream `parseConfigValueTemplate`.
fn parse_config_value_template(config: &str) -> Vec<TemplatePart> {
    let mut parts: Vec<TemplatePart> = Vec::new();
    let bytes = config.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        let Some(dollar_offset) = config[index..].find('$') else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        let dollar_index = index + dollar_offset;
        append_literal(&mut parts, &config[index..dollar_index]);

        let next_char = config
            .get(dollar_index + 1..)
            .and_then(|rest| rest.chars().next());
        match next_char {
            Some('$') | Some('!') => {
                append_literal(&mut parts, &next_char.unwrap().to_string());
                index = dollar_index + 2;
                continue;
            }
            Some('{') => {
                match config[dollar_index + 2..].find('}') {
                    Some(offset) => {
                        let end_index = dollar_index + 2 + offset;
                        let name = &config[dollar_index + 2..end_index];
                        if valid_env_var_name(name) {
                            parts.push(TemplatePart::Env(name.to_string()));
                        } else {
                            append_literal(&mut parts, &config[dollar_index..=end_index]);
                        }
                        index = end_index + 1;
                    }
                    None => {
                        append_literal(&mut parts, "$");
                        index = dollar_index + 1;
                    }
                }
                continue;
            }
            _ => {}
        }

        // Bare $NAME form: match [A-Za-z_][A-Za-z0-9_]*.
        let mut end = dollar_index + 1;
        while end < bytes.len() {
            let ch = config[end..].chars().next().unwrap();
            let is_first = end == dollar_index + 1;
            let ok = if is_first {
                ENV_VAR_NAME_RE_FIRST(ch)
            } else {
                ENV_VAR_NAME_RE_REST(ch)
            };
            if !ok {
                break;
            }
            end += ch.len_utf8();
        }
        if end > dollar_index + 1 {
            parts.push(TemplatePart::Env(config[dollar_index + 1..end].to_string()));
            index = end;
        } else {
            append_literal(&mut parts, "$");
            index = dollar_index + 1;
        }
    }

    parts
}

fn valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => {
            ENV_VAR_NAME_RE_FIRST(first)
                && chars.all(ENV_VAR_NAME_RE_REST)
                && chars.next().is_none()
        }
        None => false,
    }
}

/// Upstream `parseConfigValueReference`.
fn parse_config_value_reference(config: &str) -> ConfigValueReference {
    if let Some(command) = config.strip_prefix('!') {
        return ConfigValueReference::Command {
            config: format!("!{command}"),
        };
    }
    ConfigValueReference::Template {
        parts: parse_config_value_template(config),
    }
}

fn resolve_env_config_value(name: &str, env: Option<&Env>) -> Option<String> {
    env.and_then(|env| env.get(name))
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn get_template_env_var_names(parts: &[TemplatePart]) -> Vec<String> {
    let mut names = Vec::new();
    for part in parts {
        if let TemplatePart::Env(name) = part {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

fn resolve_template(parts: &[TemplatePart], env: Option<&Env>) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(value) => resolved.push_str(value),
            TemplatePart::Env(name) => {
                let env_value = resolve_env_config_value(name, env)?;
                resolved.push_str(&env_value);
            }
        }
    }
    Some(resolved)
}

/// The env var name when the config value is exactly one `$VAR` reference.
pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Template { parts } => match parts.as_slice() {
            [TemplatePart::Env(name)] => Some(name.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// All env var names referenced by a template value.
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Template { parts } => get_template_env_var_names(&parts),
        _ => Vec::new(),
    }
}

/// Referenced env var names that are not currently set.
pub fn get_missing_config_value_env_var_names(config: &str, env: Option<&Env>) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| resolve_env_config_value(name, env).is_none())
        .collect()
}

/// True when the value is a `!command` shell reference.
pub fn is_command_config_value(config: &str) -> bool {
    matches!(
        parse_config_value_reference(config),
        ConfigValueReference::Command { .. }
    )
}

/// True when every referenced env var is available.
pub fn is_config_value_configured(config: &str, env: Option<&Env>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

fn execute_command_uncached(command_config: &str) -> Option<String> {
    let command = command_config.strip_prefix('!')?;
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Resolve a config value (API key, header value, etc.) with caching:
/// - `!command` executes the rest as a shell command and uses stdout (cached)
/// - `$ENV_VAR` / `${ENV_VAR}` interpolate the named environment variable
/// - `$$` escapes a literal `$`, `$!` escapes a literal `!`
/// - otherwise the value is a literal
pub fn resolve_config_value(config: &str, env: Option<&Env>) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command { config } => {
            let mut cache = command_result_cache().lock().expect("config value cache");
            if let Some(cached) = cache.get(&config) {
                return cached.clone();
            }
            let result = execute_command_uncached(&config);
            cache.insert(config, result.clone());
            result
        }
        ConfigValueReference::Template { parts } => resolve_template(&parts, env),
    }
}

/// Resolve without consulting/populating the command cache.
pub fn resolve_config_value_uncached(config: &str, env: Option<&Env>) -> Option<String> {
    match parse_config_value_reference(config) {
        ConfigValueReference::Command { config } => execute_command_uncached(&config),
        ConfigValueReference::Template { parts } => resolve_template(&parts, env),
    }
}

/// Resolve or fail with a descriptive message (upstream
/// `resolveConfigValueOrThrow`).
pub fn resolve_config_value_or_throw(
    config: &str,
    description: &str,
    env: Option<&Env>,
) -> Result<String, String> {
    if let Some(resolved) = resolve_config_value_uncached(config, env) {
        return Ok(resolved);
    }
    match parse_config_value_reference(config) {
        ConfigValueReference::Command { config } => Err(format!(
            "Failed to resolve {description} from shell command: {}",
            config.strip_prefix('!').unwrap_or(&config)
        )),
        ConfigValueReference::Template { .. } => {
            let missing = get_missing_config_value_env_var_names(config, env);
            match missing.len() {
                1 => Err(format!(
                    "Failed to resolve {description} from environment variable: {}",
                    missing[0]
                )),
                count if count > 1 => Err(format!(
                    "Failed to resolve {description} from environment variables: {}",
                    missing.join(", ")
                )),
                _ => Err(format!("Failed to resolve {description}")),
            }
        }
    }
}

/// Resolve all header values using the same resolution logic as API keys.
pub fn resolve_headers(
    headers: Option<&BTreeMap<String, String>>,
    env: Option<&Env>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    let mut resolved = BTreeMap::new();
    for (key, value) in headers {
        if let Some(resolved_value) = resolve_config_value(value, env) {
            resolved.insert(key.clone(), resolved_value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

/// Resolve all header values, failing on the first unresolvable one.
pub fn resolve_headers_or_throw(
    headers: Option<&BTreeMap<String, String>>,
    description: &str,
    env: Option<&Env>,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let Some(headers) = headers else {
        return Ok(None);
    };
    let mut resolved = BTreeMap::new();
    for (key, value) in headers {
        let resolved_value =
            resolve_config_value_or_throw(value, &format!("{description} header \"{key}\""), env)?;
        resolved.insert(key.clone(), resolved_value);
    }
    Ok((!resolved.is_empty()).then_some(resolved))
}

/// Clear the config value command cache (upstream: exported for testing).
pub fn clear_config_value_cache() {
    command_result_cache()
        .lock()
        .expect("config value cache")
        .clear();
}
