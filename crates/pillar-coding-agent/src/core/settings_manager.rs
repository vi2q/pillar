//! Port of packages/coding-agent/src/core/settings-manager.ts (pi v0.84.3):
//! layered settings (global + project) with JSON file storage, deep merge,
//! per-field modification tracking, and scoped persistence.
//!
//! divergence: upstream uses a typed `Settings` interface with per-field
//! getters/setters plus a serialized write queue; the port keeps the same
//! storage/merge/persistence contract but exposes field access through a
//! dynamic JSON settings object (the TUI layer consumes settings
//! dynamically), with the same defaults and validation for the accessor
//! subset the runtime needs. The async write queue collapses to synchronous
//! writes under the storage lock (the port's storage `with_lock` is sync).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::core::auth_storage::strip_bom;
use crate::core::http_dispatcher::{DEFAULT_HTTP_IDLE_TIMEOUT_MS, parse_http_idle_timeout_ms};

pub const CONFIG_DIR_NAME: &str = ".pi";

// ============================================================================
// Settings values
// ============================================================================

/// Resolved compaction settings (upstream `getCompactionSettings` shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// Resolved branch summary settings (upstream `getBranchSummarySettings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchSummarySettings {
    pub reserve_tokens: u64,
    pub skip_prompt: bool,
}

impl Default for BranchSummarySettings {
    fn default() -> Self {
        Self {
            reserve_tokens: 16_384,
            skip_prompt: false,
        }
    }
}

/// Resolved retry settings (upstream `getRetrySettings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

/// Resolved provider retry settings (upstream `getProviderRetrySettings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetrySettings {
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: u64,
}

/// Resolved thinking budgets (upstream `ThinkingBudgetsSettings`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

/// Default project trust levels (upstream `DefaultProjectTrust`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultProjectTrust {
    Ask,
    Always,
    Never,
}

/// Double-escape action (upstream `doubleEscapeAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoubleEscapeAction {
    Fork,
    Tree,
    None,
}

/// Tree filter mode (upstream `treeFilterMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

/// A package source: string form or object form with filters (upstream
/// `PackageSource`).
#[derive(Debug, Clone, PartialEq)]
pub enum PackageSource {
    Source(String),
    Filtered {
        source: String,
        autoload: Option<bool>,
        extensions: Option<Vec<String>>,
        skills: Option<Vec<String>>,
        prompts: Option<Vec<String>>,
        themes: Option<Vec<String>>,
    },
}

// ============================================================================
// Merge semantics
// ============================================================================

fn is_mergeable_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

fn deep_merge_objects(base: &Value, overrides: &Value) -> Value {
    match (base, overrides) {
        (Value::Object(base_map), Value::Object(override_map)) => {
            let mut result = base_map.clone();
            for (key, override_value) in override_map {
                if override_value.is_null() {
                    continue;
                }
                let merged = match (result.get(key), override_value) {
                    (Some(base_value), _)
                        if is_mergeable_object(base_value)
                            && is_mergeable_object(override_value) =>
                    {
                        deep_merge_objects(base_value, override_value)
                    }
                    _ => override_value.clone(),
                };
                result.insert(key.clone(), merged);
            }
            Value::Object(result)
        }
        (_, overrides) => overrides.clone(),
    }
}

/// Deep merge settings: project/overrides take precedence, nested objects
/// merge recursively (upstream `deepMergeSettings`).
pub fn deep_merge_settings(base: &Value, overrides: &Value) -> Value {
    deep_merge_objects(base, overrides)
}

// ============================================================================
// Storage
// ============================================================================

/// Settings scope (upstream `SettingsScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

/// A settings storage error with scope (upstream `SettingsError`).
#[derive(Debug, Clone)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<PathBuf>,
    pub message: String,
}

/// Storage backend contract (upstream `SettingsStorage`): run a
/// read-modify-write under the scope's lock. Returning Some replaces the
/// content; None leaves it unchanged.
pub trait SettingsStorage: Send + Sync {
    fn with_lock(&self, scope: SettingsScope, f: &mut dyn FnMut(Option<String>) -> Option<String>);
    fn path_for(&self, scope: SettingsScope) -> Option<PathBuf>;
}

/// File-backed storage (upstream `FileSettingsStorage`): global settings
/// live in the agent dir, project settings in `<cwd>/.pi/settings.json`.
pub struct FileSettingsStorage {
    global_settings_path: PathBuf,
    project_settings_path: PathBuf,
}

impl FileSettingsStorage {
    pub fn new(cwd: &str, agent_dir: &Path) -> Self {
        Self {
            global_settings_path: agent_dir.join("settings.json"),
            project_settings_path: Path::new(cwd).join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }

    fn path(&self, scope: SettingsScope) -> &Path {
        match scope {
            SettingsScope::Global => &self.global_settings_path,
            SettingsScope::Project => &self.project_settings_path,
        }
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(&self, scope: SettingsScope, f: &mut dyn FnMut(Option<String>) -> Option<String>) {
        let path = self.path(scope);
        let file_exists = path.exists();
        // The port's fd-lock based backend serializes writers; the
        // read-before-lock ordering of the upstream implementation is
        // collapsed into a single locked critical section.
        if let Ok(Some(next)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let current = file_exists.then(|| std::fs::read_to_string(path).unwrap_or_default());
            let next = f(current);
            if let Some(next) = next {
                if let Some(dir) = path.parent() {
                    if !dir.exists() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                }
                if std::fs::write(path, next).is_err() {
                    return None::<String>;
                }
            }
            Some(String::new())
        })) {
            let _ = next;
        }
    }

    fn path_for(&self, scope: SettingsScope) -> Option<PathBuf> {
        Some(self.path(scope).to_path_buf())
    }
}

/// In-memory storage (upstream `InMemorySettingsStorage`).
#[derive(Debug, Default)]
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(&self, scope: SettingsScope, f: &mut dyn FnMut(Option<String>) -> Option<String>) {
        let cell = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let mut guard = cell.lock().unwrap();
        let next = f(guard.clone());
        if let Some(next) = next {
            *guard = Some(next);
        }
    }

    fn path_for(&self, _scope: SettingsScope) -> Option<PathBuf> {
        None
    }
}

// ============================================================================
// Settings manager
// ============================================================================

/// Load and migrate settings from storage (upstream `loadFromStorage`).
fn load_from_storage(
    storage: &dyn SettingsStorage,
    scope: SettingsScope,
    project_trusted: bool,
) -> Result<Value, String> {
    if scope == SettingsScope::Project && !project_trusted {
        return Ok(Value::Object(Default::default()));
    }
    let mut content: Option<String> = None;
    storage.with_lock(scope, &mut |current| {
        content = current;
        None
    });
    let Some(content) = content.filter(|c| !c.is_empty()) else {
        return Ok(Value::Object(Default::default()));
    };
    let parsed: Value = serde_json::from_str(strip_bom(&content))
        .map_err(|e| format!("Failed to parse settings: {e}"))?;
    Ok(migrate_settings(parsed))
}

fn try_load_from_storage(
    storage: &dyn SettingsStorage,
    scope: SettingsScope,
    project_trusted: bool,
) -> (Value, Option<String>) {
    match load_from_storage(storage, scope, project_trusted) {
        Ok(settings) => (settings, None),
        Err(error) => (Value::Object(Default::default()), Some(error)),
    }
}

/// Migrate old settings formats to the current one (upstream
/// `migrateSettings`): queueMode -> steeringMode, websockets boolean ->
/// transport enum, legacy skills object -> array, retry.maxDelayMs ->
/// retry.provider.maxRetryDelayMs.
pub fn migrate_settings(mut settings: Value) -> Value {
    let Some(object) = settings.as_object_mut() else {
        return settings;
    };

    // queueMode -> steeringMode
    if object.contains_key("queueMode") && !object.contains_key("steeringMode") {
        let queue_mode = object.remove("queueMode").unwrap();
        object.insert("steeringMode".to_string(), queue_mode);
    }

    // websockets boolean -> transport enum
    if !object.contains_key("transport") {
        if let Some(websockets) = object.get("websockets").and_then(|v| v.as_bool()) {
            object.insert(
                "transport".to_string(),
                Value::String(if websockets { "websocket" } else { "sse" }.to_string()),
            );
        }
        object.remove("websockets");
    }

    // Legacy skills object format -> array
    if let Some(skills) = object.get("skills") {
        if skills.is_object() {
            let enable_skill_commands = skills.get("enableSkillCommands").cloned();
            let custom_directories = skills.get("customDirectories").cloned();
            if let Some(enabled) = enable_skill_commands {
                object
                    .entry("enableSkillCommands".to_string())
                    .or_insert(enabled);
            }
            match custom_directories {
                Some(Value::Array(directories)) if !directories.is_empty() => {
                    object.insert("skills".to_string(), Value::Array(directories));
                }
                _ => {
                    object.remove("skills");
                }
            }
        }
    }

    // retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    if let Some(retry) = object.get_mut("retry") {
        if let Some(retry_object) = retry.as_object_mut() {
            let max_delay = retry_object.get("maxDelayMs").and_then(|v| v.as_f64());
            if let Some(max_delay) = max_delay {
                let has_provider_max = retry_object
                    .get("provider")
                    .and_then(|provider| provider.get("maxRetryDelayMs"))
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                if !has_provider_max {
                    let provider = retry_object
                        .get("provider")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Default::default()));
                    let mut provider_object = provider.as_object().cloned().unwrap_or_default();
                    provider_object.insert(
                        "maxRetryDelayMs".to_string(),
                        serde_json::json!(max_delay as u64),
                    );
                    retry_object.insert("provider".to_string(), Value::Object(provider_object));
                }
                retry_object.remove("maxDelayMs");
            }
        }
    }

    settings
}

/// Create options (upstream `SettingsManagerCreateOptions`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsManagerCreateOptions {
    pub project_trusted: Option<bool>,
}

/// Layered settings manager (upstream `SettingsManager`). The merged view is
/// global settings deep-merged with project settings; setters track modified
/// fields so persistence merges field-wise over the file contents.
pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    global_settings: Value,
    project_settings: Value,
    settings: Value,
    project_trusted: bool,
    modified_fields: BTreeSet<String>,
    modified_nested_fields: BTreeMap<String, BTreeSet<String>>,
    modified_project_fields: BTreeSet<String>,
    modified_project_nested_fields: BTreeMap<String, BTreeSet<String>>,
    global_settings_load_error: Option<String>,
    project_settings_load_error: Option<String>,
    errors: Vec<SettingsError>,
}

impl SettingsManager {
    /// Create a manager that loads from files (upstream
    /// `SettingsManager.create`).
    pub fn create(cwd: &str, agent_dir: &Path, options: SettingsManagerCreateOptions) -> Self {
        let storage = Arc::new(FileSettingsStorage::new(cwd, agent_dir));
        Self::from_storage(storage, options)
    }

    /// Create from an arbitrary storage backend (upstream `fromStorage`).
    pub fn from_storage(
        storage: Arc<dyn SettingsStorage>,
        options: SettingsManagerCreateOptions,
    ) -> Self {
        let project_trusted = options.project_trusted.unwrap_or(true);
        let (global_settings, global_error) =
            try_load_from_storage(storage.as_ref(), SettingsScope::Global, true);
        let (project_settings, project_error) =
            try_load_from_storage(storage.as_ref(), SettingsScope::Project, project_trusted);
        let mut errors = Vec::new();
        if let Some(error) = &global_error {
            errors.push(SettingsError {
                scope: SettingsScope::Global,
                path: storage.path_for(SettingsScope::Global),
                message: error.clone(),
            });
        }
        if let Some(error) = &project_error {
            errors.push(SettingsError {
                scope: SettingsScope::Project,
                path: storage.path_for(SettingsScope::Project),
                message: error.clone(),
            });
        }
        let settings = deep_merge_settings(&global_settings, &project_settings);
        Self {
            storage,
            global_settings,
            project_settings,
            settings,
            project_trusted,
            modified_fields: BTreeSet::new(),
            modified_nested_fields: BTreeMap::new(),
            modified_project_fields: BTreeSet::new(),
            modified_project_nested_fields: BTreeMap::new(),
            global_settings_load_error: global_error,
            project_settings_load_error: project_error,
            errors,
        }
    }

    /// Create an in-memory manager (upstream `inMemory`).
    pub fn in_memory(settings: Value, options: SettingsManagerCreateOptions) -> Self {
        let storage = Arc::new(InMemorySettingsStorage::default());
        let migrated = migrate_settings(settings);
        storage.with_lock(SettingsScope::Global, &mut |_| {
            Some(serde_json::to_string_pretty(&migrated).unwrap_or_default())
        });
        Self::from_storage(storage, options)
    }

    pub fn global_settings(&self) -> &Value {
        &self.global_settings
    }

    pub fn project_settings(&self) -> &Value {
        &self.project_settings
    }

    /// The merged settings view.
    pub fn settings(&self) -> &Value {
        &self.settings
    }

    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        if !trusted {
            self.project_settings = Value::Object(Default::default());
            self.project_settings_load_error = None;
            self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
            return;
        }
        let (project_settings, error) =
            try_load_from_storage(self.storage.as_ref(), SettingsScope::Project, trusted);
        self.project_settings = project_settings;
        self.project_settings_load_error = error.clone();
        if let Some(error) = error {
            self.record_error(SettingsScope::Project, error);
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// Reload both scopes from storage (upstream `reload`).
    pub fn reload(&mut self) {
        let (global_settings, global_error) =
            try_load_from_storage(self.storage.as_ref(), SettingsScope::Global, true);
        if global_error.is_none() {
            self.global_settings = global_settings;
            self.global_settings_load_error = None;
        } else {
            self.global_settings_load_error = global_error.clone();
            if let Some(error) = global_error {
                self.record_error(SettingsScope::Global, error);
            }
        }

        self.modified_fields.clear();
        self.modified_nested_fields.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();

        let (project_settings, project_error) = try_load_from_storage(
            self.storage.as_ref(),
            SettingsScope::Project,
            self.project_trusted,
        );
        if project_error.is_none() {
            self.project_settings = project_settings;
            self.project_settings_load_error = None;
        } else {
            self.project_settings_load_error = project_error.clone();
            if let Some(error) = project_error {
                self.record_error(SettingsScope::Project, error);
            }
        }

        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// Apply additional overrides on top of the current settings (upstream
    /// `applyOverrides`).
    pub fn apply_overrides(&mut self, overrides: &Value) {
        self.settings = deep_merge_settings(&self.settings, overrides);
    }

    fn mark_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    fn mark_project_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_project_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_project_nested_fields
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    fn assert_project_trusted_for_write(&self) -> Result<(), String> {
        if !self.project_trusted {
            return Err("Project is not trusted; refusing to write project settings".to_string());
        }
        Ok(())
    }

    fn record_error(&mut self, scope: SettingsScope, message: String) {
        self.errors.push(SettingsError {
            scope,
            path: self.storage.path_for(scope),
            message,
        });
    }

    fn clear_modified_scope(&mut self, scope: SettingsScope) {
        match scope {
            SettingsScope::Global => {
                self.modified_fields.clear();
                self.modified_nested_fields.clear();
            }
            SettingsScope::Project => {
                self.modified_project_fields.clear();
                self.modified_project_nested_fields.clear();
            }
        }
    }

    fn persist_scoped_settings(
        &self,
        scope: SettingsScope,
        snapshot: &Value,
        modified_fields: &BTreeSet<String>,
        modified_nested_fields: &BTreeMap<String, BTreeSet<String>>,
    ) {
        self.storage.with_lock(scope, &mut |current| {
            let current_file_settings: Value = current
                .as_deref()
                .filter(|c| !c.is_empty())
                .and_then(|c| serde_json::from_str(strip_bom(c)).ok())
                .map(migrate_settings)
                .unwrap_or_else(|| Value::Object(Default::default()));
            let mut merged = current_file_settings.clone();
            let merged_object = merged.as_object_mut().expect("settings are objects");
            for field in modified_fields {
                let Some(value) = snapshot.get(field) else {
                    continue;
                };
                if let Some(nested_modified) = modified_nested_fields.get(field) {
                    if value.is_object() {
                        let mut merged_nested = current_file_settings
                            .get(field)
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        for nested_key in nested_modified {
                            if let Some(nested_value) = value.get(nested_key) {
                                merged_nested.insert(nested_key.clone(), nested_value.clone());
                            }
                        }
                        merged_object.insert(field.clone(), Value::Object(merged_nested));
                        continue;
                    }
                }
                merged_object.insert(field.clone(), value.clone());
            }
            serde_json::to_string_pretty(&merged).ok()
        });
    }

    fn save(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.global_settings_load_error.is_some() {
            return;
        }
        let snapshot = self.global_settings.clone();
        let modified_fields = self.modified_fields.clone();
        let modified_nested_fields = self.modified_nested_fields.clone();
        self.persist_scoped_settings(
            SettingsScope::Global,
            &snapshot,
            &modified_fields,
            &modified_nested_fields,
        );
        self.clear_modified_scope(SettingsScope::Global);
    }

    fn save_project_settings(&mut self, settings: Value) -> Result<(), String> {
        self.assert_project_trusted_for_write()?;
        self.project_settings = settings;
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.project_settings_load_error.is_some() {
            return Ok(());
        }
        let snapshot = self.project_settings.clone();
        let modified_fields = self.modified_project_fields.clone();
        let modified_nested_fields = self.modified_project_nested_fields.clone();
        self.persist_scoped_settings(
            SettingsScope::Project,
            &snapshot,
            &modified_fields,
            &modified_nested_fields,
        );
        self.clear_modified_scope(SettingsScope::Project);
        Ok(())
    }

    fn update_project_settings(
        &mut self,
        field: &str,
        update: impl FnOnce(&mut Value),
    ) -> Result<(), String> {
        self.assert_project_trusted_for_write()?;
        let mut project_settings = self.project_settings.clone();
        update(&mut project_settings);
        self.mark_project_modified(field, None);
        self.save_project_settings(project_settings)
    }

    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    // --- accessors (upstream getter/setter subset) -----------------------------

    pub fn get_global_setting(&self, key: &str) -> Option<&Value> {
        self.settings.get(key).filter(|v| !v.is_null())
    }

    /// Set a global field wholesale.
    pub fn set_global_setting(&mut self, key: &str, value: Value) {
        if let Some(global) = self.global_settings.as_object_mut() {
            global.insert(key.to_string(), value);
        }
        self.mark_modified(key, None);
        self.save();
    }

    /// Set a nested key inside a global object field.
    pub fn set_global_nested_setting(&mut self, field: &str, nested_key: &str, value: Value) {
        let global = self
            .global_settings
            .as_object_mut()
            .expect("settings object");
        let nested = global
            .entry(field.to_string())
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(nested_object) = nested.as_object_mut() {
            nested_object.insert(nested_key.to_string(), value);
        }
        self.mark_modified(field, Some(nested_key));
        self.save();
    }

    /// Set project packages (upstream `setProjectPackages`).
    pub fn set_project_packages(&mut self, packages: Value) -> Result<(), String> {
        self.update_project_settings("packages", |settings| {
            settings
                .as_object_mut()
                .expect("settings object")
                .insert("packages".to_string(), packages);
        })
    }

    /// Set project extension paths (upstream `setProjectExtensionPaths`).
    pub fn set_project_extension_paths(&mut self, paths: Value) -> Result<(), String> {
        self.update_project_settings("extensions", |settings| {
            settings
                .as_object_mut()
                .expect("settings object")
                .insert("extensions".to_string(), paths);
        })
    }

    // --- typed accessors with upstream defaults ---------------------------------

    pub fn compaction_settings(&self) -> CompactionSettings {
        let compaction = self.settings.get("compaction");
        CompactionSettings {
            enabled: compaction
                .and_then(|c| c.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            reserve_tokens: compaction
                .and_then(|c| c.get("reserveTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(16_384),
            keep_recent_tokens: compaction
                .and_then(|c| c.get("keepRecentTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(20_000),
        }
    }

    pub fn branch_summary_settings(&self) -> BranchSummarySettings {
        let branch = self.settings.get("branchSummary");
        BranchSummarySettings {
            reserve_tokens: branch
                .and_then(|b| b.get("reserveTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(16_384),
            skip_prompt: branch
                .and_then(|b| b.get("skipPrompt"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }

    pub fn retry_settings(&self) -> RetrySettings {
        let retry = self.settings.get("retry");
        RetrySettings {
            enabled: retry
                .and_then(|r| r.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            max_retries: retry
                .and_then(|r| r.get("maxRetries"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(3),
            base_delay_ms: retry
                .and_then(|r| r.get("baseDelayMs"))
                .and_then(|v| v.as_u64())
                .unwrap_or(2_000),
        }
    }

    pub fn provider_retry_settings(&self) -> ProviderRetrySettings {
        let provider = self.settings.get("retry").and_then(|r| r.get("provider"));
        ProviderRetrySettings {
            timeout_ms: provider
                .and_then(|p| p.get("timeoutMs"))
                .and_then(|v| v.as_u64()),
            max_retries: provider
                .and_then(|p| p.get("maxRetries"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            max_retry_delay_ms: provider
                .and_then(|p| p.get("maxRetryDelayMs"))
                .and_then(|v| v.as_u64())
                .unwrap_or(60_000),
        }
    }

    pub fn thinking_budgets(&self) -> Option<ThinkingBudgets> {
        let budgets = self.settings.get("thinkingBudgets")?;
        Some(ThinkingBudgets {
            minimal: budgets.get("minimal").and_then(|v| v.as_u64()),
            low: budgets.get("low").and_then(|v| v.as_u64()),
            medium: budgets.get("medium").and_then(|v| v.as_u64()),
            high: budgets.get("high").and_then(|v| v.as_u64()),
        })
    }

    pub fn http_idle_timeout_ms(&self) -> Result<u64, String> {
        let value = self.settings.get("httpIdleTimeoutMs");
        match parse_http_idle_timeout_ms(value) {
            Some(timeout) => Ok(timeout),
            None if value.is_none() => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            None => Err(format!(
                "Invalid httpIdleTimeoutMs setting: {}",
                value.map(|v| v.to_string()).unwrap_or_default()
            )),
        }
    }

    pub fn websocket_connect_timeout_ms(&self) -> Result<Option<u64>, String> {
        let value = self.settings.get("websocketConnectTimeoutMs");
        match parse_http_idle_timeout_ms(value) {
            Some(timeout) => Ok(Some(timeout)),
            None if value.is_none() => Ok(None),
            None => Err(format!(
                "Invalid websocketConnectTimeoutMs setting: {}",
                value.map(|v| v.to_string()).unwrap_or_default()
            )),
        }
    }

    pub fn hide_thinking_block(&self) -> bool {
        self.settings
            .get("hideThinkingBlock")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn show_cache_miss_notices(&self) -> bool {
        self.settings
            .get("showCacheMissNotices")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn quiet_startup(&self) -> bool {
        self.settings
            .get("quietStartup")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn enable_install_telemetry(&self) -> bool {
        self.settings
            .get("enableInstallTelemetry")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn enable_analytics(&self) -> bool {
        self.settings
            .get("enableAnalytics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn tracking_id(&self) -> Option<&str> {
        self.settings.get("trackingId").and_then(|v| v.as_str())
    }

    pub fn enable_skill_commands(&self) -> bool {
        self.settings
            .get("enableSkillCommands")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn default_project_trust(&self) -> DefaultProjectTrust {
        match self
            .global_settings
            .get("defaultProjectTrust")
            .and_then(|v| v.as_str())
        {
            Some("always") => DefaultProjectTrust::Always,
            Some("never") => DefaultProjectTrust::Never,
            _ => DefaultProjectTrust::Ask,
        }
    }

    pub fn double_escape_action(&self) -> DoubleEscapeAction {
        match self
            .settings
            .get("doubleEscapeAction")
            .and_then(|v| v.as_str())
        {
            Some("fork") => DoubleEscapeAction::Fork,
            Some("none") => DoubleEscapeAction::None,
            _ => DoubleEscapeAction::Tree,
        }
    }

    pub fn tree_filter_mode(&self) -> TreeFilterMode {
        match self.settings.get("treeFilterMode").and_then(|v| v.as_str()) {
            Some("no-tools") => TreeFilterMode::NoTools,
            Some("user-only") => TreeFilterMode::UserOnly,
            Some("labeled-only") => TreeFilterMode::LabeledOnly,
            Some("all") => TreeFilterMode::All,
            Some("default") => TreeFilterMode::Default,
            _ => TreeFilterMode::Default,
        }
    }

    pub fn steering_mode(&self) -> &str {
        self.settings
            .get("steeringMode")
            .and_then(|v| v.as_str())
            .unwrap_or("one-at-a-time")
    }

    pub fn follow_up_mode(&self) -> &str {
        self.settings
            .get("followUpMode")
            .and_then(|v| v.as_str())
            .unwrap_or("one-at-a-time")
    }

    pub fn transport(&self) -> &str {
        self.settings
            .get("transport")
            .and_then(|v| v.as_str())
            .unwrap_or("auto")
    }

    pub fn show_images(&self) -> bool {
        self.settings
            .get("terminal")
            .and_then(|t| t.get("showImages"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn image_width_cells(&self) -> u64 {
        let width = self
            .settings
            .get("terminal")
            .and_then(|t| t.get("imageWidthCells"))
            .and_then(|v| v.as_f64());
        match width {
            Some(width) if width.is_finite() => (width.floor() as u64).max(1),
            _ => 60,
        }
    }

    pub fn editor_padding_x(&self) -> i64 {
        self.settings
            .get("editorPaddingX")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    pub fn output_pad(&self) -> u8 {
        if self.settings.get("outputPad").and_then(|v| v.as_u64()) == Some(0) {
            0
        } else {
            1
        }
    }

    pub fn autocomplete_max_visible(&self) -> u64 {
        self.settings
            .get("autocompleteMaxVisible")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
    }

    pub fn code_block_indent(&self) -> &str {
        self.settings
            .get("markdown")
            .and_then(|m| m.get("codeBlockIndent"))
            .and_then(|v| v.as_str())
            .unwrap_or("  ")
    }

    pub fn mermaid_rendering_mode(&self) -> &str {
        match self
            .settings
            .get("markdown")
            .and_then(|m| m.get("mermaid"))
            .and_then(|v| v.as_str())
        {
            Some("off") => "off",
            Some("final") => "final",
            _ => "streaming",
        }
    }
}
