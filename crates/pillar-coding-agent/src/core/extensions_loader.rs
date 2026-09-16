//! Port of packages/coding-agent/src/core/extensions/loader.ts (pi
//! v0.84.3), the runtime-state and discovery core:
//! `ExtensionRuntime` (throwing action stubs until bind, stale
//! invalidation with tracked event-bus unsubscription, queued provider
//! registrations, flag values), extension path discovery
//! (`discoverExtensionsInDir` + `discoverAndLoadExtensions` ordering:
//! project .pillar/extensions, agentDir/extensions, then configured paths
//! with manifest/index expansion), and the module cache keyed by cwd +
//! generation.
//!
//! divergences: loading actual TS/JS extension modules requires a JS
//! runtime and is not ported — `load_extension` is a host-injected
//! callback; the cache stores loaded extension values provided by the
//! host instead of module factories.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::core::extensions_runner::{ExtensionFlag, HostExtension, RegisteredCommand};
use crate::core::package_manager::resolve_extension_entries;
use crate::core::tools::path_utils::resolve_to_cwd;

const CONFIG_DIR_NAME: &str = crate::core::settings_manager::CONFIG_DIR_NAME;

/// The default stale-ctx message (upstream `invalidate` default).
pub const DEFAULT_STALE_MESSAGE: &str = "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

/// A queued provider registration (upstream
/// `pendingProviderRegistrations` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProviderRegistration {
    pub name: String,
    pub config_json: String,
    pub extension_path: String,
}

type ActionFn =
    Arc<dyn Fn(&[serde_json::Value]) -> Result<serde_json::Value, String> + Send + Sync>;

/// The shared extension runtime state (upstream `ExtensionRuntimeState`).
/// Action methods are host-injected closures; before binding they return
/// the "not initialized" error, matching upstream's throwing stubs.
pub struct ExtensionRuntime {
    state_stale_message: Option<String>,
    event_bus_unsubscribers: Vec<Arc<dyn Fn() + Send + Sync>>,
    pub flag_values: BTreeMap<String, serde_json::Value>,
    pub pending_provider_registrations: Vec<PendingProviderRegistration>,
    /// Action closures; None means "not initialized" (upstream throwing
    /// stubs).
    actions: BTreeMap<String, ActionFn>,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    /// Create a runtime with uninitialized action stubs (upstream
    /// `createExtensionRuntime`).
    pub fn new() -> Self {
        Self {
            state_stale_message: None,
            event_bus_unsubscribers: Vec::new(),
            flag_values: BTreeMap::new(),
            pending_provider_registrations: Vec::new(),
            actions: BTreeMap::new(),
        }
    }

    pub fn assert_active(&self) -> Result<(), String> {
        match &self.state_stale_message {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    /// Mark the runtime stale; the first message wins and all tracked
    /// event-bus subscriptions are unsubscribed (upstream `invalidate`).
    pub fn invalidate(&mut self, message: Option<&str>) {
        if self.state_stale_message.is_some() {
            return;
        }
        self.state_stale_message = Some(message.unwrap_or(DEFAULT_STALE_MESSAGE).to_string());
        for unsubscribe in self.event_bus_unsubscribers.drain(..) {
            unsubscribe();
        }
    }

    /// Retain an event-bus subscription until invalidation (upstream
    /// `trackEventBusSubscription`). The returned unsubscribe is a no-op
    /// after invalidation or after being called once.
    pub fn track_event_bus_subscription(
        &mut self,
        unsubscribe: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let active = Arc::new(Mutex::new(true));
        let tracked_active = active.clone();
        let tracked_unsubscribe: Arc<dyn Fn() + Send + Sync> = {
            let active = active.clone();
            let unsubscribe = unsubscribe.clone();
            Arc::new(move || {
                let mut guard = active.lock().unwrap();
                if !*guard {
                    return;
                }
                *guard = false;
                drop(guard);
                unsubscribe();
            })
        };
        self.event_bus_unsubscribers
            .push(tracked_unsubscribe.clone());
        drop(tracked_active);
        tracked_unsubscribe
    }

    /// Bind an action implementation (upstream Runner.bindCore replaces
    /// the throwing stubs).
    pub fn bind_action(&mut self, name: &str, action: ActionFn) {
        self.actions.insert(name.to_string(), action);
    }

    /// Call an action (upstream calling an `ExtensionActions` method):
    /// returns the not-initialized error before bind.
    pub fn call_action(
        &self,
        name: &str,
        args: &[serde_json::Value],
    ) -> Result<serde_json::Value, String> {
        self.assert_active()?;
        match self.actions.get(name) {
            Some(action) => action(args),
            None => Err("Extension runtime not initialized. Action methods cannot be called during extension loading."
                .to_string()),
        }
    }

    /// Queue a provider registration (upstream pre-bind
    /// `registerProvider`).
    pub fn queue_provider_registration(
        &mut self,
        name: &str,
        config_json: &str,
        extension_path: &str,
    ) {
        self.pending_provider_registrations
            .push(PendingProviderRegistration {
                name: name.to_string(),
                config_json: config_json.to_string(),
                extension_path: extension_path.to_string(),
            });
    }

    /// Drop queued registrations for a provider name (upstream
    /// `unregisterProvider` pre-bind).
    pub fn unqueue_provider_registration(&mut self, name: &str) {
        self.pending_provider_registrations
            .retain(|r| r.name != name);
    }

    /// Drain queued registrations for bind (upstream bindCore flush).
    pub fn drain_pending_provider_registrations(&mut self) -> Vec<PendingProviderRegistration> {
        std::mem::take(&mut self.pending_provider_registrations)
    }
}

// ============================================================================
// Discovery (upstream discoverExtensionsInDir / discoverAndLoadExtensions)
// ============================================================================

fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js")
}

/// Discover extension entry files in a directory (upstream
/// `discoverExtensionsInDir`): direct .ts/.js files, plus subdirectories
/// resolved via package.json pi manifest or index.ts/index.js.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut discovered = Vec::new();
    if !dir.is_dir() {
        return discovered;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return discovered;
    };
    let mut dir_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());
    for entry in dir_entries {
        let entry_path = dir.join(entry.file_name());
        let Ok(metadata) = std::fs::symlink_metadata(&entry_path) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if (metadata.is_file() || metadata.file_type().is_symlink()) && is_extension_file(&name) {
            discovered.push(entry_path);
            continue;
        }
        if metadata.is_dir() || metadata.file_type().is_symlink() {
            if let Some(entries) = resolve_extension_entries(&entry_path) {
                discovered.extend(entries);
            }
        }
    }
    discovered
}

/// Discover and order extension paths (upstream
/// `discoverAndLoadExtensions`): project `.pillar/extensions/`, agent
/// `extensions/`, then configured paths (directories expand via manifest
/// entries or directory discovery). Canonical path dedupe.
pub fn discover_and_load_extension_paths(
    configured_paths: &[String],
    cwd: &str,
    agent_dir: &str,
) -> Vec<String> {
    let resolved_cwd = resolve_to_cwd(cwd, "/");
    let resolved_agent_dir = resolve_to_cwd(agent_dir, "/");
    let mut all_paths: Vec<String> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    let add_paths = |paths: Vec<PathBuf>, all: &mut Vec<String>, seen: &mut BTreeSet<PathBuf>| {
        for p in paths {
            let resolved = p.canonicalize().unwrap_or_else(|_| p.clone());
            if seen.insert(resolved) {
                all.push(p.to_string_lossy().to_string());
            }
        }
    };

    // 1. Project-local extensions: cwd/.pillar/extensions/
    let local_ext_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("extensions");
    add_paths(
        discover_extensions_in_dir(&local_ext_dir),
        &mut all_paths,
        &mut seen,
    );

    // 2. Global extensions: agentDir/extensions/
    let global_ext_dir = resolved_agent_dir.join("extensions");
    add_paths(
        discover_extensions_in_dir(&global_ext_dir),
        &mut all_paths,
        &mut seen,
    );

    // 3. Explicitly configured paths.
    for p in configured_paths {
        let resolved = resolve_to_cwd(p, &resolved_cwd.to_string_lossy());
        if resolved.is_dir() {
            if let Some(entries) = resolve_extension_entries(&resolved) {
                add_paths(entries, &mut all_paths, &mut seen);
                continue;
            }
            add_paths(
                discover_extensions_in_dir(&resolved),
                &mut all_paths,
                &mut seen,
            );
            continue;
        }
        add_paths(vec![resolved], &mut all_paths, &mut seen);
    }

    all_paths
}

// ============================================================================
// Cache + load (upstream loadExtensions with host-injected module loading)
// ============================================================================

/// Load-result pair for one path (upstream `{ extension, error }`).
// The loader contract's outcome shapes live in the extension contract crate
// (the VM returns them; this crate's loader drives them).
pub use pillar_extensions_contract::{LoadOutcome, ModuleLoader};


/// Cache entry: the loaded extension keyed by resolved path, invalidated
/// by cwd change or generation bump (upstream `extensionCache`).
#[derive(Default)]
pub struct ExtensionCache {
    cache: BTreeMap<String, HostExtension>,
    cwd: Option<PathBuf>,
    generation: u64,
}

impl ExtensionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the cache and bump the generation (upstream
    /// `clearExtensionCache`).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.cwd = None;
        self.generation += 1;
    }

    /// Point the cache at a cwd, clearing on change (upstream
    /// `useExtensionCacheCwd`). Returns the generation token.
    pub fn use_cwd(&mut self, cwd: &str) -> u64 {
        let resolved = resolve_to_cwd(cwd, "/");
        if let Some(previous) = &self.cwd {
            if previous != &resolved {
                self.clear();
            }
        }
        self.cwd = Some(resolved);
        self.generation
    }

    /// Load paths with caching (upstream `loadExtensionsInternal` +
    /// `loadExtensionsCached`): cache hits skip the loader; misses invoke
    /// it and store non-None results. Errors are collected per path.
    pub fn load_extensions(
        &mut self,
        paths: &[String],
        cwd: &str,
        use_cache: bool,
        load: ModuleLoader<'_>,
    ) -> (Vec<HostExtension>, Vec<(String, String)>) {
        let generation = if use_cache {
            Some(self.use_cwd(cwd))
        } else {
            None
        };
        let mut extensions = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        for ext_path in paths {
            let resolved = resolve_to_cwd(ext_path, cwd);
            let key = resolved.to_string_lossy().to_string();
            if use_cache {
                if let Some(cached) = self.cache.get(&key) {
                    extensions.push(cached.clone());
                    continue;
                }
            }
            match load(&key) {
                Ok(Some(extension)) => {
                    if use_cache {
                        self.cache.insert(key, extension.clone());
                    }
                    extensions.push(extension);
                }
                Ok(None) => {}
                Err(error) => errors.push((ext_path.clone(), error)),
            }
        }
        let _ = generation;
        (extensions, errors)
    }
}

/// Convenience wrapper mirroring upstream `wrapRegisteredTools`: rewrites
/// registered command source paths (identity over names in the port).
pub fn wrap_registered_tools(
    tools: &[String],
    _extension_flags: &BTreeMap<String, ExtensionFlag>,
) -> Vec<String> {
    tools.to_vec()
}

/// Merge extension commands into a flat list (upstream runner-side; kept
/// here for load-result assembly).
pub fn extension_commands(ext: &HostExtension) -> Vec<RegisteredCommand> {
    ext.commands.clone()
}

/// Merge extension flags into a flat map (first wins).
pub fn extension_flags(ext: &HostExtension) -> BTreeMap<String, ExtensionFlag> {
    ext.flags.clone()
}
