//! Luau extension loading path (docs/rules/04-luau-extensions.md): the
//! pillar-specific extension surface. Luau extensions live in
//! `~/.pillar/extensions` (global) and `<cwd>/.pillar/extensions`
//! (project-local) as `*.luau` files or `*/index.luau` subdirectories.
//! The actual script execution lives in the (optionally-present)
//! `pillar-extensions` crate, injected here through the
//! [`LuauExtensionLoader`] trait so this crate never depends on the
//! runtime crate.
//!
//! divergence: upstream pi discovers `.ts`/`.js` extension files under
//! `.pillar/extensions`; pillar's documented discovery (docs/rules/04) is
//! `.luau`/`index.luau` under `.pillar/extensions` with the same
//! global-first ordering.

use std::path::{Path, PathBuf};

use crate::core::extensions_loader::ExtensionCache;
use crate::core::extensions_runner::ExtensionRunner;

// The loader contract lives in the extension contract crate (the VM crate
// implements it; this crate discovers the paths and drives the runner).
pub use pillar_extensions_contract::LuauExtensionLoader;

/// Default global extensions directory (`~/.pillar/extensions`).
pub fn default_global_extensions_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".pillar").join("extensions"))
}

/// Default project-local extensions directory (`<cwd>/.pillar/extensions`).
pub fn default_project_extensions_dir(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".pillar").join("extensions")
}

fn scan_luau_directory(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirectories: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirectories.push(path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("luau") {
            files.push(path);
        }
    }
    files.sort();
    out.extend(files);
    subdirectories.sort();
    for subdirectory in subdirectories {
        let index = subdirectory.join("index.luau");
        if index.is_file() {
            out.push(index);
        }
    }
}

/// Discover Luau extension paths (docs/rules/04): global first, then
/// project-local; each scope contributes `*.luau` files and `*/index.luau`
/// subdirectories sorted by name. Configured paths pass through: a `.luau`
/// file is included directly, a directory is scanned. Canonical-path
/// dedupe, global wins.
pub fn discover_luau_paths(
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
    configured_paths: &[String],
    cwd: &str,
) -> Vec<String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let resolved_cwd = crate::core::tools::path_utils::resolve_to_cwd(cwd, "/");
    if let Some(global_dir) = global_dir {
        scan_luau_directory(global_dir, &mut paths);
    }
    if let Some(project_dir) = project_dir {
        scan_luau_directory(project_dir, &mut paths);
    }
    for configured in configured_paths {
        let resolved = crate::core::tools::path_utils::resolve_to_cwd(
            configured,
            &resolved_cwd.to_string_lossy(),
        );
        if resolved.is_dir() {
            scan_luau_directory(&resolved, &mut paths);
        } else if resolved.extension().and_then(|e| e.to_str()) == Some("luau") {
            paths.push(resolved);
        }
    }

    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    paths
        .into_iter()
        .filter_map(|path| {
            let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
            seen.insert(resolved)
                .then(|| path.to_string_lossy().to_string())
        })
        .collect()
}

/// Load discovered Luau extension paths into a runner using the injected
/// loader (upstream `loadExtensions` + runner assembly). Per-path errors
/// are collected; one failing file does not abort the rest.
pub fn build_luau_runner(
    paths: &[String],
    cwd: &str,
    loader: &dyn LuauExtensionLoader,
    use_cache: bool,
) -> (ExtensionRunner, Vec<(String, String)>) {
    let mut cache = ExtensionCache::new();
    let mut loader_fn = |path: &str| loader.load_extension(path);
    let (extensions, errors) = cache.load_extensions(paths, cwd, use_cache, &mut loader_fn);
    (ExtensionRunner::new(extensions), errors)
}
