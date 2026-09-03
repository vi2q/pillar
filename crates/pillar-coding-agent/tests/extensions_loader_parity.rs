//! Parity tests for extensions/loader.ts (pi v0.84.3), the runtime-state
//! and discovery core: throwing action stubs, stale invalidation with
//! tracked unsubscription, queued provider registrations, discovery
//! ordering, and the cwd/generation-keyed module cache.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pillar_coding_agent::core::extensions_loader::{
    DEFAULT_STALE_MESSAGE, ExtensionCache, ExtensionRuntime, discover_and_load_extension_paths,
    discover_extensions_in_dir,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-extload-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- runtime state -----------------------------------------------------------------------

#[test]
fn actions_are_uninitialized_before_bind() {
    let runtime = ExtensionRuntime::new();
    let error = runtime
        .call_action("sendMessage", &[serde_json::json!("hi")])
        .unwrap_err();
    assert!(
        error.contains("Extension runtime not initialized"),
        "{error}"
    );
}

#[test]
fn bound_actions_run_after_bind() {
    let mut runtime = ExtensionRuntime::new();
    runtime.bind_action(
        "getSessionName",
        Arc::new(|_args| Ok(serde_json::json!("my session"))),
    );
    let result = runtime.call_action("getSessionName", &[]).unwrap();
    assert_eq!(result, serde_json::json!("my session"));
}

#[test]
fn invalidate_uses_default_message_and_is_once_only() {
    let mut runtime = ExtensionRuntime::new();
    runtime.assert_active().unwrap();
    runtime.invalidate(None);
    assert_eq!(runtime.assert_active().unwrap_err(), DEFAULT_STALE_MESSAGE);
    // Second invalidate does not overwrite.
    runtime.invalidate(Some("custom"));
    assert_eq!(runtime.assert_active().unwrap_err(), DEFAULT_STALE_MESSAGE);
}

#[test]
fn invalidate_unsubscribes_tracked_subscriptions() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let mut runtime = ExtensionRuntime::new();
    let unsubscribe = runtime.track_event_bus_subscription(Arc::new(move || {
        calls2.fetch_add(1, Ordering::SeqCst);
    }));

    // Idempotent unsubscribe.
    unsubscribe();
    unsubscribe();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Invalidation unsubscribes remaining tracked subscriptions.
    let calls3 = calls.clone();
    runtime.track_event_bus_subscription(Arc::new(move || {
        calls3.fetch_add(1, Ordering::SeqCst);
    }));
    runtime.invalidate(None);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // Tracked unsubscriber after invalidation is a no-op (already drained).
}

#[test]
fn provider_registrations_queue_and_drain() {
    let mut runtime = ExtensionRuntime::new();
    runtime.queue_provider_registration("my-provider", "{}", "ext-a");
    runtime.queue_provider_registration("other", "{}", "ext-b");
    runtime.unqueue_provider_registration("other");

    let drained = runtime.drain_pending_provider_registrations();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].name, "my-provider");
    assert_eq!(drained[0].extension_path, "ext-a");
    // Second drain is empty.
    assert!(runtime.drain_pending_provider_registrations().is_empty());
}

// --- discovery -------------------------------------------------------------------------------

#[test]
fn discover_extensions_in_dir_finds_files_and_manifest_dirs() {
    let dir = temp_dir("disc");
    std::fs::write(dir.join("top.ts"), "").unwrap();
    std::fs::write(dir.join("skip.txt"), "").unwrap();

    let manifest_dir = dir.join("manifest-pkg");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::write(
        manifest_dir.join("package.json"),
        r#"{"pi": {"extensions": ["custom.ts"]}}"#,
    )
    .unwrap();
    std::fs::write(manifest_dir.join("custom.ts"), "").unwrap();
    std::fs::write(manifest_dir.join("index.ts"), "").unwrap();

    let index_dir = dir.join("index-pkg");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("index.ts"), "").unwrap();

    let discovered = discover_extensions_in_dir(&dir);
    assert!(discovered.contains(&dir.join("top.ts")), "{discovered:?}");
    assert!(!discovered.contains(&dir.join("skip.txt")));
    // Manifest entry wins over index.ts.
    assert!(discovered.contains(&manifest_dir.join("custom.ts")));
    assert!(!discovered.contains(&manifest_dir.join("index.ts")));
    assert!(discovered.contains(&index_dir.join("index.ts")));
}

#[test]
fn discover_and_load_ordering_project_global_configured() {
    let cwd = temp_dir("order-cwd");
    let agent_dir = temp_dir("order-agent");

    let project_ext = cwd.join(".pi").join("extensions").join("proj.ts");
    std::fs::create_dir_all(project_ext.parent().unwrap()).unwrap();
    std::fs::write(&project_ext, "").unwrap();

    let global_ext = agent_dir.join("extensions").join("glob.ts");
    std::fs::create_dir_all(global_ext.parent().unwrap()).unwrap();
    std::fs::write(&global_ext, "").unwrap();

    let configured_dir = temp_dir("order-configured");
    std::fs::write(configured_dir.join("cfg.ts"), "").unwrap();

    let paths = discover_and_load_extension_paths(
        &[configured_dir.to_string_lossy().to_string()],
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
    );
    assert_eq!(
        paths,
        vec![
            project_ext.to_string_lossy().to_string(),
            global_ext.to_string_lossy().to_string(),
            configured_dir.join("cfg.ts").to_string_lossy().to_string(),
        ]
    );
}

#[test]
fn discover_and_load_dedupes_canonical_paths() {
    let cwd = temp_dir("dedupe-cwd");
    let agent_dir = temp_dir("dedupe-agent");
    let ext = cwd.join(".pi").join("extensions").join("same.ts");
    std::fs::create_dir_all(ext.parent().unwrap()).unwrap();
    std::fs::write(&ext, "").unwrap();

    let paths = discover_and_load_extension_paths(
        &[ext.to_string_lossy().to_string()],
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
    );
    assert_eq!(paths.len(), 1);
}

// --- cache ---------------------------------------------------------------------------------

#[test]
fn cache_hits_skip_loader_and_clear_bumps_generation() {
    let mut cache = ExtensionCache::new();
    let cwd = temp_dir("cache-cwd");
    let mut loads = 0usize;

    let ext = pillar_coding_agent::core::extensions_runner::HostExtension {
        path: "ext-a".to_string(),
        handlers: BTreeMap::new(),
        commands: Vec::new(),
        tools: BTreeMap::new(),
        flags: BTreeMap::new(),
        shortcuts: BTreeMap::new(),
    };
    let paths = vec!["ext-a".to_string()];
    let (first, errors) =
        cache.load_extensions(&paths, &cwd.to_string_lossy(), true, &mut |_path| {
            loads += 1;
            Ok(Some(ext.clone()))
        });
    assert_eq!(first.len(), 1);
    assert!(errors.is_empty());
    assert_eq!(loads, 1);

    // Second load hits the cache.
    let (second, _) = cache.load_extensions(&paths, &cwd.to_string_lossy(), true, &mut |_path| {
        loads += 1;
        Ok(None)
    });
    assert_eq!(second.len(), 1);
    assert_eq!(loads, 1);

    // clear() bumps generation: next load re-invokes the loader.
    cache.clear();
    let (third, _) = cache.load_extensions(&paths, &cwd.to_string_lossy(), true, &mut |_path| {
        loads += 1;
        Ok(None)
    });
    assert!(third.is_empty());
    assert_eq!(loads, 2);
}

#[test]
fn cache_clears_on_cwd_change() {
    let mut cache = ExtensionCache::new();
    let cwd_a = temp_dir("cache-a");
    let cwd_b = temp_dir("cache-b");
    let mut loads = 0usize;
    let paths = vec!["ext-a".to_string()];

    let mut load = |cache: &mut ExtensionCache, cwd: &std::path::Path| {
        cache.load_extensions(&paths, &cwd.to_string_lossy(), true, &mut |_| {
            loads += 1;
            Ok(None)
        });
    };
    load(&mut cache, &cwd_a);
    // Same cwd: the None result is not cached, so the loader runs again.
    load(&mut cache, &cwd_a);
    // Different cwd clears everything; loader runs again.
    load(&mut cache, &cwd_b);
    assert_eq!(loads, 3);
}

#[test]
fn no_cache_mode_skips_caching() {
    let mut cache = ExtensionCache::new();
    let cwd = temp_dir("nocache-cwd");
    let mut loads = 0usize;
    let paths = vec!["ext-a".to_string()];
    for _ in 0..2 {
        cache.load_extensions(&paths, &cwd.to_string_lossy(), false, &mut |_| {
            loads += 1;
            Ok(None)
        });
    }
    assert_eq!(loads, 2);
}

#[test]
fn load_errors_collected_per_path() {
    let mut cache = ExtensionCache::new();
    let cwd = temp_dir("err-cwd");
    let paths = vec!["bad-path".to_string()];
    let (extensions, errors) =
        cache.load_extensions(&paths, &cwd.to_string_lossy(), false, &mut |_| {
            Err("module failed".to_string())
        });
    assert!(extensions.is_empty());
    assert_eq!(
        errors,
        vec![("bad-path".to_string(), "module failed".to_string())]
    );
}
