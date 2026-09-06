//! Parity tests for the Luau extension loading path
//! (docs/rules/04-luau-extensions.md): `.luau`/`index.luau` discovery
//! with global-first ordering, and runner assembly over an injected
//! [`LuauExtensionLoader`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pillar_coding_agent::core::extensions_luau::{
    LuauExtensionLoader, build_luau_runner, discover_luau_paths,
};
use pillar_coding_agent::core::extensions_runner::{ExtensionHandler, HostExtension};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-luau-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- discovery --------------------------------------------------------------------------

#[test]
fn discovers_luau_files_and_index_subdirectories_sorted() {
    let global = temp_dir("global");
    let project = temp_dir("project");

    // Global: two files (sorted), one subdir with index.luau.
    std::fs::create_dir_all(global.join("a")).unwrap();
    std::fs::write(global.join("b.luau"), "return nil").unwrap();
    std::fs::write(global.join("a.luau"), "return nil").unwrap();
    std::fs::write(global.join("a").join("index.luau"), "return nil").unwrap();
    // A non-luau file and a subdir without index.luau are ignored.
    std::fs::write(global.join("notes.txt"), "x").unwrap();
    std::fs::create_dir_all(global.join("empty")).unwrap();

    // Project: one file.
    std::fs::write(project.join("proj.luau"), "return nil").unwrap();

    let paths = discover_luau_paths(Some(&global), Some(&project), &[], "");
    assert_eq!(
        paths,
        vec![
            global.join("a.luau").to_string_lossy().to_string(),
            global.join("b.luau").to_string_lossy().to_string(),
            global
                .join("a")
                .join("index.luau")
                .to_string_lossy()
                .to_string(),
            project.join("proj.luau").to_string_lossy().to_string(),
        ],
        "{paths:?}"
    );
}

#[test]
fn global_scoped_entries_come_before_project_scoped() {
    let global = temp_dir("g2");
    let project = temp_dir("p2");
    std::fs::write(global.join("z.luau"), "").unwrap();
    std::fs::write(project.join("a.luau"), "").unwrap();

    let paths = discover_luau_paths(Some(&global), Some(&project), &[], "");
    assert_eq!(paths.len(), 2);
    assert!(paths[0].contains("g2"), "{paths:?}");
    assert!(paths[1].contains("p2"), "{paths:?}");
}

#[test]
fn configured_paths_accept_files_and_directories() {
    let dir = temp_dir("cfg");
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("one.luau"), "").unwrap();
    std::fs::write(nested.join("two.luau"), "").unwrap();
    let file = dir.join("single.luau");
    std::fs::write(&file, "").unwrap();

    let paths = discover_luau_paths(
        None,
        None,
        &[
            nested.to_string_lossy().to_string(),
            file.to_string_lossy().to_string(),
        ],
        "",
    );
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert!(paths.contains(&file.to_string_lossy().to_string()));
    // A configured directory expands its .luau files sorted.
    let nested_paths: Vec<&String> = paths.iter().filter(|p| p.contains("nested")).collect();
    assert_eq!(nested_paths.len(), 2);
}

#[test]
fn duplicate_paths_are_deduplicated() {
    let dir = temp_dir("dup");
    std::fs::write(dir.join("x.luau"), "").unwrap();
    let dir_string = dir.to_string_lossy().to_string();
    let paths = discover_luau_paths(Some(&dir), None, std::slice::from_ref(&dir_string), "");
    assert_eq!(paths.len(), 1, "{paths:?}");
}

// --- runner assembly --------------------------------------------------------------------

/// A fake loader returning a canned extension for paths containing
/// "good" and an error for paths containing "bad".
fn fake_loader() -> impl LuauExtensionLoader {
    move |path: &str| -> pillar_coding_agent::core::extensions_loader::LoadOutcome {
        if path.contains("bad") {
            return Err("type-check failed: 1:1: boom".to_string());
        }
        let mut handlers: BTreeMap<String, Vec<ExtensionHandler>> = BTreeMap::new();
        handlers.insert("tool_call".to_string(), vec![Arc::new(|_event| Ok(None))]);
        Ok(Some(HostExtension {
            path: path.to_string(),
            handlers,
            commands: vec![
                pillar_coding_agent::core::extensions_runner::RegisteredCommand {
                    name: "hello".to_string(),
                    description: "Say hello".to_string(),
                    source_path: path.to_string(),
                },
            ],
            tools: BTreeMap::new(),
            flags: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
        }))
    }
}

#[test]
fn build_runner_loads_all_paths_and_collects_per_path_errors() {
    let dir = temp_dir("runner");
    let good = dir.join("good.luau");
    let bad = dir.join("bad.luau");
    std::fs::write(&good, "").unwrap();
    std::fs::write(&bad, "").unwrap();

    let (mut runner, errors) = build_luau_runner(
        &[
            good.to_string_lossy().to_string(),
            bad.to_string_lossy().to_string(),
        ],
        "",
        &fake_loader(),
        false,
    );

    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].0.contains("bad.luau"));
    assert!(errors[0].1.contains("type-check failed"));

    // The good extension surfaced its registrations.
    assert!(runner.has_handlers("tool_call"));
    assert!(
        runner
            .extension_paths()
            .iter()
            .any(|p| p.contains("good.luau"))
    );
    let commands = runner.command("hello");
    assert!(commands.is_some(), "command registered");
}

#[test]
fn build_runner_cache_skips_loader_for_duplicate_paths() {
    let dir = temp_dir("cache");
    let good = dir.join("good.luau");
    std::fs::write(&good, "").unwrap();
    let path = good.to_string_lossy().to_string();

    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let loader = {
        let loads = Arc::clone(&loads);
        move |path: &str| -> pillar_coding_agent::core::extensions_loader::LoadOutcome {
            loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if path.contains("bad") {
                return Err("type-check failed: 1:1: boom".to_string());
            }
            Ok(Some(HostExtension {
                path: path.to_string(),
                handlers: BTreeMap::new(),
                commands: Vec::new(),
                tools: BTreeMap::new(),
                flags: BTreeMap::new(),
                shortcuts: BTreeMap::new(),
            }))
        }
    };

    // Same path twice in one run: the second hit comes from the cache.
    let (runner, errors) = build_luau_runner(&[path.clone(), path.clone()], "", &loader, true);
    assert!(errors.is_empty());
    assert_eq!(
        loads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "loader invoked once for a duplicated path"
    );
    assert_eq!(runner.extension_paths().len(), 2, "one entry per path");

    // A fresh build with a fresh cache re-invokes the loader.
    let loads_before = loads.load(std::sync::atomic::Ordering::SeqCst);
    let (_, errors) = build_luau_runner(std::slice::from_ref(&path), "", &loader, true);
    assert!(errors.is_empty());
    assert_eq!(
        loads.load(std::sync::atomic::Ordering::SeqCst),
        loads_before + 1,
        "new cache re-invokes the loader"
    );
}

#[test]
fn default_dirs_point_at_pillar_locations() {
    use pillar_coding_agent::core::extensions_luau::{
        default_global_extensions_dir, default_project_extensions_dir,
    };
    let global = default_global_extensions_dir().expect("HOME set");
    assert!(
        global.ends_with(Path::new(".pillar/extensions")),
        "{global:?}"
    );
    let project = default_project_extensions_dir("/work");
    assert_eq!(project, PathBuf::from("/work/.pillar/extensions"));
}
