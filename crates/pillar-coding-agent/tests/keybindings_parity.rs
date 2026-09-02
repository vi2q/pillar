//! Port of the upstream coding-agent keybindings tests (pi v0.84.3):
//! keybindings.test.ts (Windows defaults), keybindings-migration.test.ts.

use std::collections::BTreeMap;

use pillar_coding_agent::core::keybindings::{
    KeybindingsManager, app_definitions, keybindings, migrate_keybindings_config,
    use_windows_keybindings,
};

fn env(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn app_keys(platform: &str, env: &BTreeMap<String, String>, id: &str) -> Vec<String> {
    keybindings(platform, env)
        .get(id)
        .map(|definition| definition.default_keys.clone())
        .unwrap_or_default()
}

// --- keybindings.test.ts ----------------------------------------------------

#[test]
fn uses_windows_keybindings_on_native_windows() {
    assert!(use_windows_keybindings("win32", &env(&[])));
}

#[test]
fn uses_windows_keybindings_in_wsl() {
    assert!(use_windows_keybindings(
        "linux",
        &env(&[("WSL_DISTRO_NAME", "Ubuntu")])
    ));
    assert!(use_windows_keybindings(
        "linux",
        &env(&[("WSL_INTEROP", "/run/WSL/123_interop")])
    ));
}

#[test]
fn does_not_use_windows_keybindings_from_wt_session_alone() {
    assert!(!use_windows_keybindings(
        "linux",
        &env(&[("WT_SESSION", "session")])
    ));
}

#[test]
fn keeps_non_windows_defaults_on_other_platforms() {
    assert!(!use_windows_keybindings("linux", &env(&[])));
    assert!(!use_windows_keybindings("darwin", &env(&[])));
}

#[test]
fn applies_detected_defaults_consistently() {
    let windows = env(&[]);
    let linux = env(&[]);
    let non_win = keybindings("linux", &linux);

    assert_eq!(
        app_keys("win32", &windows, "app.clipboard.pasteImage"),
        vec!["alt+v".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "app.clipboard.pasteImage"),
        vec!["ctrl+v".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "tui.altScreen.search"),
        vec!["ctrl+f".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "tui.altScreen.search"),
        vec!["ctrl+shift+f".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "app.message.followUp"),
        vec!["ctrl+q".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "app.message.followUp"),
        vec!["alt+enter".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "app.model.cycleBackward"),
        vec!["alt+p".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "app.model.cycleBackward"),
        vec!["shift+ctrl+p".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "tui.editor.undo"),
        vec!["ctrl+z".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "tui.editor.undo"),
        vec!["ctrl+-".to_string()]
    );
    // WSL counts as windowsKeybindings.
    assert_eq!(
        app_keys(
            "linux",
            &env(&[("WSL_DISTRO_NAME", "Ubuntu")]),
            "tui.editor.undo"
        ),
        vec!["alt+z".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "tui.altScreen.previousPrompt"),
        vec!["ctrl+up".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "tui.altScreen.previousPrompt"),
        vec!["ctrl+shift+up".to_string(), "ctrl+up".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "tui.altScreen.nextPrompt"),
        vec!["ctrl+down".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "tui.altScreen.nextPrompt"),
        vec!["ctrl+shift+down".to_string(), "ctrl+down".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "app.message.dequeue"),
        vec!["alt+q".to_string()]
    );
    assert_eq!(
        app_keys("linux", &linux, "app.message.dequeue"),
        vec!["alt+up".to_string()]
    );
    assert_eq!(
        app_keys("win32", &windows, "app.suspend"),
        Vec::<String>::new()
    );
    assert_eq!(
        app_keys("linux", &linux, "app.suspend"),
        vec!["ctrl+z".to_string()]
    );
    let _ = non_win;
}

// --- keybindings-migration.test.ts -------------------------------------------

fn json_config(entries: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn definition_order() -> Vec<&'static str> {
    app_definitions().keys().copied().collect()
}

#[test]
fn rewrites_old_key_names_to_namespaced_ids() {
    let raw = json_config(&[
        ("cursorUp", serde_json::json!(["up", "ctrl+p"])),
        ("expandTools", serde_json::json!("ctrl+x")),
    ]);
    let (config, migrated) = migrate_keybindings_config(&raw, &definition_order());
    assert!(migrated);
    assert_eq!(
        config.get("tui.editor.cursorUp"),
        Some(&serde_json::json!(["up", "ctrl+p"]))
    );
    assert_eq!(
        config.get("app.tools.expand"),
        Some(&serde_json::json!("ctrl+x"))
    );
    assert!(!config.contains_key("cursorUp"));
    assert!(!config.contains_key("expandTools"));
}

#[test]
fn keeps_the_namespaced_value_when_old_and_new_names_both_exist() {
    let raw = json_config(&[
        ("expandTools", serde_json::json!("ctrl+x")),
        ("app.tools.expand", serde_json::json!("ctrl+y")),
    ]);
    let (config, migrated) = migrate_keybindings_config(&raw, &definition_order());
    assert!(migrated);
    assert_eq!(
        config.get("app.tools.expand"),
        Some(&serde_json::json!("ctrl+y"))
    );
    assert_eq!(config.len(), 1);
}

#[test]
fn loads_old_key_names_in_memory_before_the_file_is_rewritten() {
    let agent_dir = std::env::temp_dir().join(format!(
        "pillar-coding-agent-keybindings-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&agent_dir);
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        agent_dir.join("keybindings.json"),
        r#"{"selectConfirm": "enter", "interrupt": "ctrl+x"}"#,
    )
    .expect("write keybindings.json");

    let manager = KeybindingsManager::create(&agent_dir);
    let user = manager.get_user_bindings();
    assert_eq!(
        user.get("tui.select.confirm"),
        Some(&vec!["enter".to_string()])
    );
    assert_eq!(user.get("app.interrupt"), Some(&vec!["ctrl+x".to_string()]));

    let effective = manager.get_effective_config();
    assert_eq!(
        effective.get("tui.select.confirm"),
        Some(&vec!["enter".to_string()])
    );
    assert_eq!(
        effective.get("app.interrupt"),
        Some(&vec!["ctrl+x".to_string()])
    );
}

#[test]
fn non_string_entries_are_dropped() {
    let raw = json_config(&[
        ("app.clear", serde_json::json!(42)),
        ("app.exit", serde_json::json!("ctrl+d")),
    ]);
    let (config, migrated) = migrate_keybindings_config(&raw, &definition_order());
    assert!(!migrated);
    assert_eq!(config.len(), 1);
    assert_eq!(config.get("app.exit"), Some(&serde_json::json!("ctrl+d")));
}
