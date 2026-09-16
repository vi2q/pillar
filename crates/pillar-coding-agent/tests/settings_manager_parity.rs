//! Parity tests for settings-manager.ts + http-dispatcher.ts settings half
//! (pi v0.84.3): storage layering, deep merge, migrations, field-wise
//! modified tracking, project trust gating, and typed accessors with
//! upstream defaults.

use serde_json::json;
use std::sync::Arc;

use pillar_coding_agent::core::http_dispatcher::{
    DEFAULT_HTTP_IDLE_TIMEOUT_MS, format_http_idle_timeout_ms, parse_http_idle_timeout_ms,
};
use pillar_coding_agent::core::settings_manager::{
    DoubleEscapeAction, InMemorySettingsStorage, SettingsManager, SettingsManagerCreateOptions,
    SettingsScope, SettingsStorage as _, TreeFilterMode, deep_merge_settings, migrate_settings,
};

/// Read back the raw stored content of a scope from the in-memory storage.
fn stored_value(storage: &Arc<InMemorySettingsStorage>, scope: SettingsScope) -> String {
    let mut out = String::new();
    storage.with_lock(scope, &mut |current| {
        out = current.unwrap_or_default();
        None
    });
    out
}

fn manager_with(global: serde_json::Value, project: serde_json::Value) -> SettingsManager {
    let storage = Arc::new(InMemorySettingsStorage::default());
    storage.with_lock(SettingsScope::Global, &mut |_| {
        Some(serde_json::to_string_pretty(&global).unwrap())
    });
    storage.with_lock(SettingsScope::Project, &mut |_| {
        Some(serde_json::to_string_pretty(&project).unwrap())
    });
    SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default())
}

// --- http dispatcher settings half ------------------------------------------------

#[test]
fn default_http_idle_timeout_is_five_minutes() {
    assert_eq!(DEFAULT_HTTP_IDLE_TIMEOUT_MS, 300_000);
}

#[test]
fn parse_http_idle_timeout_accepts_strings_numbers_and_disabled() {
    assert_eq!(
        parse_http_idle_timeout_ms(Some(&json!("disabled"))),
        Some(0)
    );
    assert_eq!(
        parse_http_idle_timeout_ms(Some(&json!("DISABLED"))),
        Some(0)
    );
    assert_eq!(parse_http_idle_timeout_ms(Some(&json!(""))), None);
    assert_eq!(
        parse_http_idle_timeout_ms(Some(&json!("120000"))),
        Some(120_000)
    );
    assert_eq!(parse_http_idle_timeout_ms(Some(&json!(4500.7))), Some(4500));
    assert_eq!(parse_http_idle_timeout_ms(Some(&json!(-1))), None);
    assert_eq!(parse_http_idle_timeout_ms(Some(&json!(true))), None);
    assert_eq!(parse_http_idle_timeout_ms(None), None);
}

#[test]
fn format_http_idle_timeout_uses_choices() {
    assert_eq!(format_http_idle_timeout_ms(30_000), "30 sec");
    assert_eq!(format_http_idle_timeout_ms(300_000), "5 min");
    assert_eq!(format_http_idle_timeout_ms(0), "disabled");
    assert_eq!(format_http_idle_timeout_ms(45_000), "45 sec");
}

// --- deep merge ---------------------------------------------------------------------

#[test]
fn deep_merge_nested_objects_recursively() {
    let base = json!({"compaction": {"enabled": true, "reserveTokens": 1}, "theme": "dark"});
    let overrides = json!({"compaction": {"reserveTokens": 2}, "theme": "light"});
    let merged = deep_merge_settings(&base, &overrides);
    assert_eq!(merged["compaction"]["enabled"], json!(true));
    assert_eq!(merged["compaction"]["reserveTokens"], json!(2));
    assert_eq!(merged["theme"], json!("light"));
}

// --- migrations ------------------------------------------------------------------------

#[test]
fn migrate_queue_mode_to_steering_mode() {
    let migrated = migrate_settings(json!({"queueMode": "all"}));
    assert_eq!(migrated["steeringMode"], json!("all"));
    assert!(migrated.get("queueMode").is_none());
}

#[test]
fn migrate_websockets_boolean_to_transport() {
    let migrated = migrate_settings(json!({"websockets": true}));
    assert_eq!(migrated["transport"], json!("websocket"));
    assert!(migrated.get("websockets").is_none());
    let migrated = migrate_settings(json!({"websockets": false}));
    assert_eq!(migrated["transport"], json!("sse"));
}

#[test]
fn migrate_legacy_skills_object_to_array() {
    let migrated = migrate_settings(json!({
        "skills": {"enableSkillCommands": false, "customDirectories": ["/a", "/b"]}
    }));
    assert_eq!(migrated["skills"], json!(["/a", "/b"]));
    assert_eq!(migrated["enableSkillCommands"], json!(false));

    // Empty custom directories delete the skills field.
    let migrated = migrate_settings(json!({
        "skills": {"enableSkillCommands": true, "customDirectories": []}
    }));
    assert!(migrated.get("skills").is_none());
    assert_eq!(migrated["enableSkillCommands"], json!(true));
}

#[test]
fn migrate_retry_max_delay_ms_to_provider() {
    let migrated = migrate_settings(json!({"retry": {"maxDelayMs": 5000}}));
    assert_eq!(
        migrated["retry"]["provider"]["maxRetryDelayMs"],
        json!(5000)
    );
    assert!(migrated["retry"].get("maxDelayMs").is_none());

    // Existing provider value wins.
    let migrated = migrate_settings(json!({
        "retry": {"maxDelayMs": 5000, "provider": {"maxRetryDelayMs": 9000}}
    }));
    assert_eq!(
        migrated["retry"]["provider"]["maxRetryDelayMs"],
        json!(9000)
    );
}

// --- layering and loading --------------------------------------------------------------------

#[test]
fn project_settings_override_global_with_merge() {
    let manager = manager_with(
        json!({"theme": "dark", "compaction": {"enabled": true, "reserveTokens": 100}}),
        json!({"theme": "light", "compaction": {"reserveTokens": 200}}),
    );
    assert_eq!(manager.settings()["theme"], json!("light"));
    assert_eq!(manager.settings()["compaction"]["enabled"], json!(true));
    assert_eq!(
        manager.settings()["compaction"]["reserveTokens"],
        json!(200)
    );
}

#[test]
fn untrusted_project_settings_are_ignored() {
    let manager = SettingsManager::from_storage(
        Arc::new(InMemorySettingsStorage::default()),
        SettingsManagerCreateOptions {
            project_trusted: Some(false),
        },
    );
    assert!(!manager.is_project_trusted());
    assert_eq!(manager.project_settings(), &json!({}));
}

#[test]
fn invalid_json_records_an_error() {
    let storage = Arc::new(InMemorySettingsStorage::default());
    storage.with_lock(SettingsScope::Global, &mut |_| Some("not json".to_string()));
    let mut manager =
        SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
    let errors = manager.drain_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].scope, SettingsScope::Global);
}

// --- field-wise persistence --------------------------------------------------------------------

#[test]
fn global_setter_persists_only_modified_field_merging_with_file() {
    // Simulate a file that already has fields the manager hasn't touched.
    let storage = Arc::new(InMemorySettingsStorage::default());
    storage.with_lock(SettingsScope::Global, &mut |_| {
        Some(json!({"theme": "dark", "keepMe": true}).to_string())
    });
    let mut manager =
        SettingsManager::from_storage(storage.clone(), SettingsManagerCreateOptions::default());
    manager.set_global_setting("quietStartup", json!(true));
    manager.drain_errors();

    let saved: serde_json::Value =
        serde_json::from_str(&stored_value(&storage, SettingsScope::Global)).unwrap();
    assert_eq!(saved["quietStartup"], json!(true));
    assert_eq!(saved["theme"], json!("dark"), "untouched field preserved");
    assert_eq!(saved["keepMe"], json!(true));
}

#[test]
fn nested_setter_merges_nested_keys() {
    let storage = Arc::new(InMemorySettingsStorage::default());
    storage.with_lock(SettingsScope::Global, &mut |_| {
        Some(json!({"terminal": {"showImages": true, "custom": 1}}).to_string())
    });
    let mut manager =
        SettingsManager::from_storage(storage.clone(), SettingsManagerCreateOptions::default());
    manager.set_global_nested_setting("terminal", "imageWidthCells", json!(40));
    manager.drain_errors();

    let saved: serde_json::Value =
        serde_json::from_str(&stored_value(&storage, SettingsScope::Global)).unwrap();
    assert_eq!(saved["terminal"]["showImages"], json!(true));
    assert_eq!(saved["terminal"]["custom"], json!(1));
    assert_eq!(saved["terminal"]["imageWidthCells"], json!(40));
}

#[test]
fn project_write_requires_trust() {
    let mut manager = SettingsManager::from_storage(
        Arc::new(InMemorySettingsStorage::default()),
        SettingsManagerCreateOptions {
            project_trusted: Some(false),
        },
    );
    assert!(manager.set_project_packages(json!([])).is_err());
    // Mark trusted: write succeeds.
    manager.set_project_trusted(true);
    assert!(manager.set_project_packages(json!(["npm:foo"])).is_ok());
}

#[test]
fn set_project_trusted_untrusted_drops_project_settings() {
    let mut manager = manager_with(json!({}), json!({"theme": "project"}));
    assert_eq!(manager.settings()["theme"], json!("project"));
    manager.set_project_trusted(false);
    assert_eq!(manager.settings()["theme"], json!(serde_json::Value::Null));
    // Re-trusting reloads project settings.
    manager.set_project_trusted(true);
    assert_eq!(manager.settings()["theme"], json!("project"));
}

#[test]
fn apply_overrides_layers_on_top() {
    let mut manager = manager_with(json!({"theme": "dark"}), json!({}));
    manager.apply_overrides(&json!({"theme": "override"}));
    assert_eq!(manager.settings()["theme"], json!("override"));
}

// --- typed accessors with defaults -----------------------------------------------------------------

#[test]
fn compaction_settings_defaults() {
    let manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    let settings = manager.compaction_settings();
    assert!(settings.enabled);
    assert_eq!(settings.reserve_tokens, 16_384);
    assert_eq!(settings.keep_recent_tokens, 20_000);
}

#[test]
fn compaction_settings_from_config() {
    let manager = SettingsManager::in_memory(
        json!({"compaction": {"enabled": false, "reserveTokens": 100, "keepRecentTokens": 200}}),
        SettingsManagerCreateOptions::default(),
    );
    let settings = manager.compaction_settings();
    assert!(!settings.enabled);
    assert_eq!(settings.reserve_tokens, 100);
    assert_eq!(settings.keep_recent_tokens, 200);
}

#[test]
fn branch_summary_and_retry_defaults() {
    let manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    let branch = manager.branch_summary_settings();
    assert_eq!(branch.reserve_tokens, 16_384);
    assert!(!branch.skip_prompt);
    let retry = manager.retry_settings();
    assert!(retry.enabled);
    assert_eq!(retry.max_retries, 3);
    assert_eq!(retry.base_delay_ms, 2_000);
    let provider = manager.provider_retry_settings();
    assert_eq!(provider.max_retry_delay_ms, 60_000);
    assert_eq!(provider.timeout_ms, None);
}

#[test]
fn boolean_accessors_use_upstream_defaults() {
    let manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    assert!(!manager.hide_thinking_block());
    assert!(!manager.show_cache_miss_notices());
    assert!(!manager.quiet_startup());
    assert!(manager.enable_install_telemetry(), "telemetry defaults on");
    assert!(!manager.enable_analytics());
    assert!(manager.enable_skill_commands());
    assert!(manager.show_images());
    assert_eq!(manager.image_width_cells(), 60);
    assert_eq!(manager.editor_padding_x(), 0);
    assert_eq!(manager.output_pad(), 1);
    assert_eq!(manager.autocomplete_max_visible(), 5);
    assert_eq!(manager.code_block_indent(), "  ");
    assert_eq!(manager.mermaid_rendering_mode(), "streaming");
    assert_eq!(manager.steering_mode(), "one-at-a-time");
    assert_eq!(manager.follow_up_mode(), "one-at-a-time");
    assert_eq!(manager.transport(), "auto");
    assert_eq!(manager.double_escape_action(), DoubleEscapeAction::Tree);
    assert_eq!(manager.tree_filter_mode(), TreeFilterMode::Default);
    assert_eq!(
        manager.default_project_trust(),
        pillar_coding_agent::core::settings_manager::DefaultProjectTrust::Ask
    );
}

#[test]
fn http_idle_timeout_default_and_invalid() {
    let manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    assert_eq!(
        manager.http_idle_timeout_ms().unwrap(),
        DEFAULT_HTTP_IDLE_TIMEOUT_MS
    );
    let manager = SettingsManager::in_memory(
        json!({"httpIdleTimeoutMs": "bogus"}),
        SettingsManagerCreateOptions::default(),
    );
    assert!(manager.http_idle_timeout_ms().is_err());
    let manager = SettingsManager::in_memory(
        json!({"httpIdleTimeoutMs": "disabled"}),
        SettingsManagerCreateOptions::default(),
    );
    assert_eq!(manager.http_idle_timeout_ms().unwrap(), 0);
}

#[test]
fn in_memory_settings_are_loaded_and_migrated() {
    let manager = SettingsManager::in_memory(
        json!({"queueMode": "all", "theme": "dark"}),
        SettingsManagerCreateOptions::default(),
    );
    // Migration applied at construction.
    assert_eq!(manager.settings()["steeringMode"], json!("all"));
    assert_eq!(manager.settings()["theme"], json!("dark"));
}

// --- settings-selector accessors (upstream getter/setter pairs) ---------------------

#[test]
fn settings_selector_accessors_default_and_write() {
    let mut manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());

    // Defaults.
    assert!(manager.image_auto_resize());
    assert!(!manager.block_images());
    assert!(!manager.collapse_changelog());
    assert!(!manager.show_terminal_progress());
    assert_eq!(manager.fullscreen_exit_output(), "transcript");
    assert_eq!(manager.fullscreen_scrollbar(), "auto");
    assert!(manager.fullscreen_copy_on_select());
    assert!(manager.warnings().as_object().unwrap().is_empty());
    assert!(manager.all_model_thinking_levels().is_empty());

    // Writes land in the global scope under the upstream keys.
    manager.set_image_auto_resize(false);
    manager.set_block_images(true);
    manager.set_collapse_changelog(true);
    manager.set_show_terminal_progress(true);
    manager.set_fullscreen_exit_output("resume-hint");
    manager.set_fullscreen_scrollbar("hidden");
    manager.set_fullscreen_copy_on_select(false);
    manager.set_show_images(false);
    manager.set_image_width_cells(0); // clamped to 1
    manager.set_enable_skill_commands(false);
    manager.set_transport("websocket");
    manager.set_http_idle_timeout_ms(120_000);
    manager.set_hide_thinking_block(true);
    manager.set_show_cache_miss_notices(true);
    manager.set_quiet_startup(true);
    manager.set_enable_install_telemetry(false);
    manager.set_editor_padding_x(9); // clamped to 3
    manager.set_output_pad(0);
    manager.set_autocomplete_max_visible(99); // clamped to 20
    manager.set_clear_on_shrink(true);
    manager.set_show_hardware_cursor(true);
    manager.set_mermaid_rendering_mode("off");
    manager.set_double_escape_action(DoubleEscapeAction::Fork);
    manager.set_tree_filter_mode(TreeFilterMode::UserOnly);
    manager.set_default_project_trust(
        pillar_coding_agent::core::settings_manager::DefaultProjectTrust::Never,
    );
    manager.set_warnings(json!({ "anthropicExtraUsage": false }));

    assert!(!manager.image_auto_resize());
    assert!(manager.block_images());
    assert!(manager.collapse_changelog());
    assert!(manager.show_terminal_progress());
    assert_eq!(manager.fullscreen_exit_output(), "resume-hint");
    assert_eq!(manager.fullscreen_scrollbar(), "hidden");
    assert!(!manager.fullscreen_copy_on_select());
    assert!(!manager.show_images());
    assert_eq!(manager.image_width_cells(), 1);
    assert!(!manager.enable_skill_commands());
    assert_eq!(manager.transport(), "websocket");
    assert_eq!(manager.http_idle_timeout_ms().unwrap(), 120_000);
    assert!(manager.hide_thinking_block());
    assert!(manager.show_cache_miss_notices());
    assert!(manager.quiet_startup());
    assert!(!manager.enable_install_telemetry());
    assert_eq!(manager.editor_padding_x(), 3);
    assert_eq!(manager.output_pad(), 0);
    assert_eq!(manager.autocomplete_max_visible(), 20);
    assert!(manager.clear_on_shrink());
    assert!(manager.show_hardware_cursor());
    assert_eq!(manager.mermaid_rendering_mode(), "off");
    assert_eq!(manager.double_escape_action(), DoubleEscapeAction::Fork);
    assert_eq!(manager.tree_filter_mode(), TreeFilterMode::UserOnly);
    assert_eq!(
        manager.default_project_trust(),
        pillar_coding_agent::core::settings_manager::DefaultProjectTrust::Never
    );
    assert_eq!(manager.warnings()["anthropicExtraUsage"], json!(false));

    // Nested writes create the parent object.
    assert_eq!(manager.settings()["terminal"]["showImages"], json!(false));
    assert_eq!(manager.settings()["images"]["blockImages"], json!(true));
    assert_eq!(manager.settings()["markdown"]["mermaid"], json!("off"));
}

#[test]
fn model_thinking_levels_map_and_removal() {
    let mut manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    manager.set_model_thinking_level("anthropic", "claude-opus-5", "high");
    manager.set_model_thinking_level("opencode-go", "omen-alpha", "low");

    assert_eq!(
        manager.model_thinking_level("anthropic", "claude-opus-5"),
        Some("high".to_string())
    );
    let all = manager.all_model_thinking_levels();
    assert_eq!(all.len(), 2);
    assert_eq!(all["opencode-go/omen-alpha"], "low");

    // Removing the only key drops the whole object (upstream behaviour).
    manager.remove_model_thinking_level("anthropic", "claude-opus-5");
    manager.remove_model_thinking_level("opencode-go", "omen-alpha");
    assert!(manager.all_model_thinking_levels().is_empty());
    assert!(manager.settings().get("modelThinkingLevels").is_none());
}

#[test]
fn clear_on_shrink_and_hardware_cursor_fall_back_to_env() {
    // The env fallbacks match the TUI options (`PILLAR_*`).
    let manager = SettingsManager::in_memory(
        json!({"terminal": {"clearOnShrink": true}, "showHardwareCursor": false}),
        SettingsManagerCreateOptions::default(),
    );
    assert!(manager.clear_on_shrink());
    assert!(!manager.show_hardware_cursor());

    // Explicit settings win over the env defaults.
    let manager = SettingsManager::in_memory(json!({}), SettingsManagerCreateOptions::default());
    assert!(!manager.clear_on_shrink());
    assert!(!manager.show_hardware_cursor());
}
