//! Port of packages/coding-agent/src/core/trust-manager.ts and
//! pi-manifest.ts (pi v0.84.3): the persisted project-trust store with
//! nearest-directory lookup, the trust option list, the
//! trust-requiring-resource detection, and package.json `pi` manifest
//! parsing.
//!
//! divergence: upstream uses `proper-lockfile` lock directories; the port
//! serializes via `fd-lock` on a `.lock` sibling file. Path canonicalization
//! uses a lexical normalization (no symlink resolution).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::settings_manager::CONFIG_DIR_NAME;

/// A trust decision for a path: true/false, or null to clear (upstream
/// `ProjectTrustDecision`).
pub type ProjectTrustDecision = Option<bool>;

/// A stored trust entry (upstream `ProjectTrustStoreEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustStoreEntry {
    pub path: String,
    pub decision: bool,
}

/// A pending trust update (upstream `ProjectTrustUpdate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustUpdate {
    pub path: String,
    pub decision: ProjectTrustDecision,
}

/// A trust choice shown to the user (upstream `ProjectTrustOption`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
    pub saved_path: Option<String>,
}

type TrustFile = BTreeMap<String, Option<bool>>;

const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 7] = [
    "settings.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// Lexically normalize a path: resolve `.`/`..` segments without touching
/// the filesystem (upstream canonicalizePath(resolvePath(...))).
pub fn normalize_cwd(cwd: &str) -> String {
    let expanded = if let Some(rest) = cwd.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| cwd.to_string())
    } else if cwd == "~" {
        std::env::var("HOME").unwrap_or_else(|_| cwd.to_string())
    } else {
        cwd.to_string()
    };
    let path = Path::new(&expanded);
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut normalized = PathBuf::new();
    for part in parts {
        normalized.push(part);
    }
    normalized.to_string_lossy().to_string()
}

fn find_nearest_trust_entry(data: &TrustFile, cwd: &str) -> Option<ProjectTrustStoreEntry> {
    let mut current_dir = normalize_cwd(cwd);
    loop {
        if let Some(Some(decision)) = data.get(&current_dir) {
            return Some(ProjectTrustStoreEntry {
                path: current_dir,
                decision: *decision,
            });
        }
        let parent_dir = Path::new(&current_dir).parent()?;
        if parent_dir.as_os_str().is_empty() {
            return None;
        }
        current_dir = parent_dir.to_string_lossy().to_string();
    }
}

/// The parent directory of a trust path, if any (upstream
/// `getProjectTrustParentPath`).
pub fn get_project_trust_parent_path(cwd: &str) -> Option<String> {
    let trust_path = normalize_cwd(cwd);
    let parent = Path::new(&trust_path).parent()?;
    if parent.as_os_str().is_empty() || parent.as_os_str() == Path::new(&trust_path).as_os_str() {
        return None;
    }
    Some(parent.to_string_lossy().to_string())
}

/// The trust choices offered for a cwd (upstream `getProjectTrustOptions`).
pub fn get_project_trust_options(cwd: &str, include_session_only: bool) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path.clone()),
    }];
    if let Some(parent_path) = get_project_trust_parent_path(cwd) {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({parent_path})"),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent_path.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: trust_path.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent_path),
        });
    }
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path),
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options
}

fn read_trust_file(path: &Path) -> Result<TrustFile, String> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read trust store {}: {e}", path.display()))?;
    let parsed: Value = serde_json::from_str(crate::core::auth_storage::strip_bom(&content))
        .map_err(|e| format!("Failed to read trust store {}: {e}", path.display()))?;
    if !parsed.is_object() {
        return Err(format!(
            "Invalid trust store {}: expected an object",
            path.display()
        ));
    }
    let mut data = TrustFile::new();
    for (key, value) in parsed.as_object().expect("checked") {
        match value {
            Value::Bool(decision) => {
                data.insert(key.clone(), Some(*decision));
            }
            Value::Null => {
                data.insert(key.clone(), None);
            }
            _ => {
                return Err(format!(
                    "Invalid trust store {}: value for {key:?} must be true, false, or null",
                    path.display()
                ));
            }
        }
    }
    Ok(data)
}

fn write_trust_file(path: &Path, data: &TrustFile) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("Failed to create trust dir: {e}"))?;
    }
    let mut content = String::from("{\n");
    let items: Vec<String> = data
        .iter()
        .map(|(key, value)| {
            let value_str = match value {
                Some(true) => "true",
                Some(false) => "false",
                None => "null",
            };
            format!("  \"{}\": {}", key.replace('"', "\\\""), value_str)
        })
        .collect();
    content.push_str(&items.join(",\n"));
    content.push_str("\n}\n");
    fs::write(path, content).map_err(|e| format!("Failed to write trust store: {e}"))
}

fn with_trust_file_lock<T>(
    path: &Path,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let lock_path = path.with_extension("lock");
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("Failed to create trust dir: {e}"))?;
    }
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open trust lock: {e}"))?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut guard = fd_lock::RwLock::new(&lock_file);
        let _handle = guard
            .try_write()
            .map_err(|_| "Failed to acquire trust store lock".to_string())?;
        f()
    }
    #[cfg(target_arch = "wasm32")]
    {
        // wasm32 has no advisory file locks; callers serialize in-process.
        let _ = &lock_file;
        f()
    }
}

/// Returns true when cwd has project-local resources gated by project trust:
/// trust-requiring entries under `cwd/.pi`, or `.agents/skills` in cwd or an
/// ancestor. The user's `~/.agents/skills` is always treated as a trusted
/// user resource and ignored (upstream
/// `hasTrustRequiringProjectResources`).
pub fn has_trust_requiring_project_resources(cwd: &str) -> bool {
    let home_dir = normalize_cwd(&std::env::var("HOME").unwrap_or_default());
    let user_agents_skills_dir = Path::new(&home_dir).join(".agents").join("skills");
    let mut current_dir = normalize_cwd(cwd);

    let config_dir = Path::new(&current_dir).join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        let agents_skills_dir = Path::new(&current_dir).join(".agents").join("skills");
        if agents_skills_dir != user_agents_skills_dir && agents_skills_dir.exists() {
            return true;
        }
        let Some(parent_dir) = Path::new(&current_dir).parent() else {
            return false;
        };
        if parent_dir.as_os_str().is_empty()
            || parent_dir.as_os_str() == Path::new(&current_dir).as_os_str()
        {
            return false;
        }
        current_dir = parent_dir.to_string_lossy().to_string();
    }
}

/// The persisted project-trust store (upstream `ProjectTrustStore`):
/// `trust.json` maps canonical paths to trust decisions; lookups walk up to
/// the nearest ancestor entry.
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            trust_path: agent_dir.join("trust.json"),
        }
    }

    /// The nearest trust decision for a cwd, or None (upstream `get`).
    pub fn get(&self, cwd: &str) -> Option<bool> {
        self.get_entry(cwd).map(|entry| entry.decision)
    }

    /// The nearest trust entry for a cwd (upstream `getEntry`).
    pub fn get_entry(&self, cwd: &str) -> Option<ProjectTrustStoreEntry> {
        with_trust_file_lock(&self.trust_path, || {
            let data = read_trust_file(&self.trust_path)?;
            Ok(find_nearest_trust_entry(&data, cwd))
        })
        .unwrap_or_default()
    }

    /// Set a single decision (upstream `set`).
    pub fn set(&self, cwd: &str, decision: ProjectTrustDecision) -> Result<(), String> {
        self.set_many(vec![ProjectTrustUpdate {
            path: cwd.to_string(),
            decision,
        }])
    }

    /// Apply multiple decisions under one lock (upstream `setMany`); a None
    /// decision deletes the entry.
    pub fn set_many(&self, decisions: Vec<ProjectTrustUpdate>) -> Result<(), String> {
        with_trust_file_lock(&self.trust_path, || {
            let mut data = read_trust_file(&self.trust_path)?;
            for update in decisions {
                let key = normalize_cwd(&update.path);
                if update.decision.is_none() {
                    data.remove(&key);
                } else {
                    data.insert(key, update.decision);
                }
            }
            write_trust_file(&self.trust_path, &data)
        })
    }
}

// ============================================================================
// pi-manifest.ts
// ============================================================================

/// A package.json `pi` manifest (upstream `PiManifest`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PiManifest {
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
}

const RESOURCE_FIELDS: [&str; 4] = ["extensions", "skills", "prompts", "themes"];

/// Read a package.json `pi` manifest (upstream `readPiManifest`): only
/// string-array resource fields are kept; anything else yields None.
pub fn read_pi_manifest(package_json_path: &Path) -> Option<PiManifest> {
    let content = fs::read_to_string(package_json_path).ok()?;
    let parsed: Value =
        serde_json::from_str(crate::core::auth_storage::strip_bom(&content)).ok()?;
    let pi = parsed.get("pi")?.as_object()?;

    let mut manifest = PiManifest::default();
    for field in RESOURCE_FIELDS {
        if let Some(Value::Array(entries)) = pi.get(field) {
            let all_strings = entries
                .iter()
                .all(|entry| matches!(entry, Value::String(_)));
            if all_strings {
                let list: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect();
                match field {
                    "extensions" => manifest.extensions = Some(list),
                    "skills" => manifest.skills = Some(list),
                    "prompts" => manifest.prompts = Some(list),
                    "themes" => manifest.themes = Some(list),
                    _ => {}
                }
            }
        }
    }
    Some(manifest)
}
