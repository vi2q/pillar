//! Port of packages/coding-agent/src/migrations.ts (pi v0.84.3): the one-time
//! migrations that run on startup.
//!
//! divergences:
//! - the agent directory is passed in instead of read from the environment, so
//!   the CLI's `PILLAR_CODING_AGENT_DIR` handling stays in one place.
//! - `showDeprecationWarnings` waits for a line instead of a raw-mode
//!   keypress (the port has no public raw-mode helper outside the TUI).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::keybindings::{app_definitions, migrate_keybindings_config};
use crate::core::settings_manager::CONFIG_DIR_NAME;
use crate::utils::text::strip_bom;

const MIGRATION_GUIDE_URL: &str =
    "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
const EXTENSIONS_DOC_URL: &str =
    "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/extensions.md";

/// What [`run_migrations`] did (upstream the returned object).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationResult {
    pub migrated_auth_providers: Vec<String>,
    pub deprecation_warnings: Vec<String>,
}

/// Every startup migration, in upstream order.
pub fn run_migrations(cwd: &str, agent_dir: &Path) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json(agent_dir);
    migrate_sessions_from_agent_root(agent_dir);
    migrate_tools_to_bin(agent_dir);
    migrate_keybindings_config_file(agent_dir);
    let deprecation_warnings = migrate_extension_system(cwd, agent_dir);
    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

/// Legacy `oauth.json` and `settings.json` `apiKeys` to `auth.json`; answers
/// the providers that were migrated. A present `auth.json` skips everything.
pub fn migrate_auth_to_auth_json(agent_dir: &Path) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    let oauth_path = agent_dir.join("oauth.json");
    let settings_path = agent_dir.join("settings.json");
    if auth_path.exists() {
        return Vec::new();
    }

    let mut migrated = serde_json::Map::new();
    let mut providers = Vec::new();
    if oauth_path.exists() {
        if let Ok(content) = fs::read_to_string(&oauth_path) {
            if let Ok(Value::Object(oauth)) = serde_json::from_str::<Value>(strip_bom(&content)) {
                for (provider, credential) in oauth {
                    let mut entry = credential.as_object().cloned().unwrap_or_default();
                    entry.insert("type".to_string(), Value::String("oauth".to_string()));
                    migrated.insert(provider.clone(), Value::Object(entry));
                    providers.push(provider);
                }
                let _ = fs::rename(&oauth_path, oauth_path.with_extension("json.migrated"));
            }
        }
    }
    if settings_path.exists() {
        if let Ok(content) = fs::read_to_string(&settings_path) {
            if let Ok(Value::Object(mut settings)) = serde_json::from_str::<Value>(strip_bom(&content)) {
                if let Some(Value::Object(api_keys)) = settings.get("apiKeys").cloned() {
                    for (provider, key) in api_keys {
                        if migrated.contains_key(&provider) {
                            continue;
                        }
                        if let Value::String(key) = key {
                            migrated.insert(
                                provider.clone(),
                                serde_json::json!({ "type": "api_key", "key": key }),
                            );
                            providers.push(provider);
                        }
                    }
                    settings.remove("apiKeys");
                    let _ = fs::write(
                        &settings_path,
                        format!("{}\n", serde_json::to_string_pretty(&settings).unwrap_or_default()),
                    );
                }
            }
        }
    }
    if !migrated.is_empty() {
        let _ = fs::create_dir_all(agent_dir);
        let _ = write_private(
            &auth_path,
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&Value::Object(migrated)).unwrap_or_default()
            ),
        );
    }
    providers
}

/// Write a credential file with the owner-only mode upstream uses.
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(content.as_bytes())
}

/// Sessions the v0.30.0 bug wrote to `<agent>/` instead of
/// `<agent>/sessions/<encoded-cwd>/`, moved by their header's `cwd`.
pub fn migrate_sessions_from_agent_root(agent_dir: &Path) {
    let Ok(entries) = fs::read_dir(agent_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(header) = content
            .lines()
            .next()
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
        else {
            continue;
        };
        if header.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let Some(cwd) = header.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let safe_path = format!(
            "--{}--",
            cwd.trim_start_matches(['/', '\\'])
                .replace(['/', '\\', ':'], "-")
        );
        let correct_dir = agent_dir.join("sessions").join(safe_path);
        if fs::create_dir_all(&correct_dir).is_err() {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let new_path = correct_dir.join(file_name);
        if new_path.exists() {
            continue;
        }
        let _ = fs::rename(&path, &new_path);
    }
}

/// `commands/` is `prompts/` now (regular directories and symlinks alike).
fn migrate_commands_to_prompts(base_dir: &Path, label: &str) -> bool {
    let commands_dir = base_dir.join("commands");
    let prompts_dir = base_dir.join("prompts");
    if commands_dir.exists() && !prompts_dir.exists() {
        match fs::rename(&commands_dir, &prompts_dir) {
            Ok(()) => {
                println!("Migrated {label} commands/ → prompts/");
                return true;
            }
            Err(error) => println!(
                "Warning: Could not migrate {label} commands/ to prompts/: {error}"
            ),
        }
    }
    false
}

fn migrate_keybindings_config_file(agent_dir: &Path) {
    let config_path = agent_dir.join("keybindings.json");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return;
    };
    let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(strip_bom(&content)) else {
        return;
    };
    let raw: BTreeMap<String, Value> = raw.into_iter().collect();
    let definitions = app_definitions();
    let order: Vec<&'static str> = definitions.keys().copied().collect();
    let (config, migrated) = migrate_keybindings_config(&raw, &order);
    if !migrated {
        return;
    }
    let serialized = serde_json::to_string_pretty(&config).unwrap_or_default();
    let _ = fs::write(&config_path, format!("{serialized}\n"));
}

/// The managed `fd` / `rg` binaries moved from `tools/` to `bin/`.
fn migrate_tools_to_bin(agent_dir: &Path) {
    let tools_dir = agent_dir.join("tools");
    let bin_dir = agent_dir.join("bin");
    if !tools_dir.exists() {
        return;
    }
    let mut moved_any = false;
    for binary in ["fd", "rg", "fd.exe", "rg.exe"] {
        let old_path = tools_dir.join(binary);
        let new_path = bin_dir.join(binary);
        if !old_path.exists() {
            continue;
        }
        if new_path.exists() {
            let _ = fs::remove_file(&old_path);
            continue;
        }
        if fs::create_dir_all(&bin_dir).is_ok() && fs::rename(&old_path, &new_path).is_ok() {
            moved_any = true;
        }
    }
    if moved_any {
        println!("Migrated managed binaries tools/ → bin/");
    }
}

/// Deprecated `hooks/` and `tools/` directories (the latter only when it holds
/// anything beyond the auto-extracted binaries).
fn check_deprecated_extension_dirs(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if base_dir.join("hooks").exists() {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions."
        ));
    }
    if let Ok(entries) = fs::read_dir(base_dir.join("tools")) {
        let custom = entries.flatten().any(|entry| {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            name != "fd" && name != "rg" && name != "fd.exe" && name != "rg.exe" && !name.starts_with('.')
        });
        if custom {
            warnings.push(format!(
                "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
            ));
        }
    }
    warnings
}

/// The extension-system migrations (`commands/` → `prompts/`) plus the
/// deprecation warnings.
fn migrate_extension_system(cwd: &str, agent_dir: &Path) -> Vec<String> {
    let project_dir = PathBuf::from(cwd).join(CONFIG_DIR_NAME);
    migrate_commands_to_prompts(agent_dir, "Global");
    migrate_commands_to_prompts(&project_dir, "Project");
    let mut warnings = check_deprecated_extension_dirs(agent_dir, "Global");
    warnings.extend(check_deprecated_extension_dirs(&project_dir, "Project"));
    warnings
}

/// Print the deprecation warnings and wait for the user (upstream waits for a
/// raw-mode keypress).
pub fn show_deprecation_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    for warning in warnings {
        println!("Warning: {warning}");
    }
    println!("\nMove your extensions to the extensions/ directory.");
    println!("Migration guide: {MIGRATION_GUIDE_URL}");
    println!("Documentation: {EXTENSIONS_DOC_URL}");
    println!("\nPress Enter to continue...");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}
