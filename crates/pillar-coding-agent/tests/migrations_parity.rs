//! One-time startup migrations (pi v0.84.3 `migrations.ts`): the legacy
//! credential files, the v0.30.0 session misplacement, the `commands/` →
//! `prompts/` rename, the keybindings rewrite, and the managed binaries.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pillar_coding_agent::migrations::{
    migrate_auth_to_auth_json, migrate_sessions_from_agent_root, run_migrations,
};

fn temp_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-migrations-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `oauth.json` and `settings.json` `apiKeys` become one `auth.json`; a
/// present `auth.json` skips everything.
#[test]
fn legacy_credentials_become_auth_json() {
    let dir = temp_dir("auth");
    std::fs::write(
        dir.join("oauth.json"),
        r#"{"anthropic":{"refresh":"r","access":"a","expires":1}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("settings.json"),
        r#"{"theme":"dark","apiKeys":{"openai":"sk-1","anthropic":"ignored"}}"#,
    )
    .unwrap();

    let providers = migrate_auth_to_auth_json(&dir);
    assert_eq!(providers, vec!["anthropic", "openai"]);

    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["anthropic"]["type"], serde_json::json!("oauth"));
    assert_eq!(auth["anthropic"]["access"], serde_json::json!("a"));
    assert_eq!(
        auth["openai"],
        serde_json::json!({ "type": "api_key", "key": "sk-1" })
    );
    assert!(dir.join("oauth.json.migrated").exists());

    // `apiKeys` is removed from settings.json, the rest survives.
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["theme"], serde_json::json!("dark"));
    assert!(settings.get("apiKeys").is_none());

    // A second run must not touch the now-present auth.json.
    assert!(migrate_auth_to_auth_json(&dir).is_empty());
}

/// Sessions the v0.30.0 bug left in the agent root move to their cwd's
/// session directory.
#[test]
fn root_sessions_move_to_their_cwd_directory() {
    let dir = temp_dir("sessions");
    let cwd = "/tmp/project";
    std::fs::write(
        dir.join("2024-01-01_session.jsonl"),
        format!("{{\"type\":\"session\",\"cwd\":\"{cwd}\"}}\n{{}}\n"),
    )
    .unwrap();

    migrate_sessions_from_agent_root(&dir);

    assert!(!dir.join("2024-01-01_session.jsonl").exists());
    let moved = dir
        .join("sessions")
        .join("--tmp-project--")
        .join("2024-01-01_session.jsonl");
    assert!(moved.exists(), "{moved:?}");
}

/// `commands/` is renamed to `prompts/`, and deprecated `hooks/` / custom
/// `tools/` directories produce warnings.
#[test]
fn extension_system_migration_renames_and_warns() {
    let cwd = temp_dir("ext-cwd");
    let agent_dir = temp_dir("ext-agent");
    std::fs::create_dir_all(agent_dir.join("commands")).unwrap();
    std::fs::create_dir_all(agent_dir.join("hooks")).unwrap();
    std::fs::create_dir_all(agent_dir.join("tools")).unwrap();
    std::fs::write(agent_dir.join("tools").join("mine.ts"), "x").unwrap();

    let result = run_migrations(&cwd.to_string_lossy(), &agent_dir);

    assert!(agent_dir.join("prompts").is_dir());
    assert!(!agent_dir.join("commands").exists());
    assert!(
        result
            .deprecation_warnings
            .iter()
            .any(|warning| warning.contains("hooks/ directory")),
        "{:?}",
        result.deprecation_warnings
    );
    assert!(
        result
            .deprecation_warnings
            .iter()
            .any(|warning| warning.contains("custom tools")),
        "{:?}",
        result.deprecation_warnings
    );
}

/// Legacy keybinding names are rewritten in place.
#[test]
fn keybindings_config_is_migrated_in_place() {
    let cwd = temp_dir("keys-cwd");
    let agent_dir = temp_dir("keys-agent");
    std::fs::write(
        agent_dir.join("keybindings.json"),
        r#"{"cursorUp":"ctrl+p"}"#,
    )
    .unwrap();

    let result = run_migrations(&cwd.to_string_lossy(), &agent_dir);
    assert!(result.migrated_auth_providers.is_empty());

    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent_dir.join("keybindings.json")).unwrap())
            .unwrap();
    assert_eq!(
        config["tui.editor.cursorUp"],
        serde_json::json!("ctrl+p"),
        "{config}"
    );
    assert!(config.get("cursorUp").is_none(), "{config}");
}

/// The managed `fd` / `rg` binaries move from `tools/` to `bin/`.
#[test]
fn managed_binaries_move_to_bin() {
    let cwd = temp_dir("bin-cwd");
    let agent_dir = temp_dir("bin-agent");
    std::fs::create_dir_all(agent_dir.join("tools")).unwrap();
    std::fs::write(agent_dir.join("tools").join("rg"), "binary").unwrap();

    run_migrations(&cwd.to_string_lossy(), &agent_dir);

    assert!(!agent_dir.join("tools").join("rg").exists());
    assert!(agent_dir.join("bin").join("rg").exists());
    assert_eq!(
        std::fs::read_to_string(agent_dir.join("bin").join("rg")).unwrap(),
        "binary"
    );
}
