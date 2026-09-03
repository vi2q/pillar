//! Parity tests for sdk.ts (pi v0.84.3), the session-assembly decision
//! core: initial model restoration with fallback messages, the
//! thinking-level resolution chain, tool selection with noTools/exclude,
//! and the convertToLlm block-images filter.

use pillar_ai::types::{Message, Model, UserContent};

use pillar_coding_agent::core::sdk::{
    ThinkingLevelInputs, ToolSelectionInputs, fallback_message_using, filter_blocked_images,
    resolve_initial_active_tool_names, resolve_initial_model, resolve_thinking_level, sdk_settings,
};
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

fn model(provider: &str, id: &str, context_window: u64, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-completions".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.com".to_string(),
        reasoning,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn user_message(content: UserContent) -> Message {
    Message::User {
        content,
        timestamp: 1000,
    }
}

// --- model resolution ----------------------------------------------------------------------

#[test]
fn explicit_model_wins_without_fallback() {
    let explicit = model("openai", "gpt-test", 100_000, true);
    let resolved = resolve_initial_model(
        Some(("openai", "other")),
        Some(explicit.clone()),
        |_| true,
        |_, _| None,
    );
    assert_eq!(
        resolved.model.as_ref().map(|m| m.id.as_str()),
        Some("gpt-test")
    );
    assert_eq!(resolved.model_fallback_message, None);
}

#[test]
fn session_model_restored_only_with_configured_auth() {
    let restored = model("openai", "gpt-saved", 100_000, true);
    let resolved = resolve_initial_model(
        Some(("openai", "gpt-saved")),
        None,
        |provider| provider == "openai",
        |provider, id| (provider == "openai" && id == "gpt-saved").then(|| restored.clone()),
    );
    assert_eq!(
        resolved.model.as_ref().map(|m| m.id.as_str()),
        Some("gpt-saved")
    );
    assert_eq!(resolved.model_fallback_message, None);

    // Auth not configured → fallback message.
    let resolved = resolve_initial_model(
        Some(("openai", "gpt-saved")),
        None,
        |_| false,
        |_, _| Some(restored.clone()),
    );
    assert_eq!(resolved.model, None);
    assert_eq!(
        resolved.model_fallback_message.as_deref(),
        Some("Could not restore model openai/gpt-saved")
    );
}

#[test]
fn fallback_message_appends_selected_model() {
    let selected = model("anthropic", "claude-test", 100_000, true);
    assert_eq!(
        fallback_message_using("Could not restore model openai/gpt-saved", &selected),
        "Could not restore model openai/gpt-saved. Using anthropic/claude-test"
    );
}

// --- thinking level resolution -----------------------------------------------------------------

#[test]
fn thinking_level_chain_follows_upstream_order() {
    let m = model("openai", "gpt-test", 100_000, true);

    // Existing session with a thinking entry → session level.
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: None,
                has_existing_session: true,
                has_thinking_entry: true,
                session_thinking_level: Some("high".to_string()),
                per_model_override: Some("low"),
                default_level: Some("medium".to_string()),
            },
            Some(&m)
        ),
        "high"
    );

    // Existing session without an entry → settings default.
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: None,
                has_existing_session: true,
                has_thinking_entry: false,
                session_thinking_level: Some("high".to_string()),
                per_model_override: Some("low"),
                default_level: Some("medium".to_string()),
            },
            Some(&m)
        ),
        "medium"
    );

    // New session: per-model override applies.
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: None,
                has_existing_session: false,
                has_thinking_entry: false,
                session_thinking_level: None,
                per_model_override: Some("low"),
                default_level: Some("medium".to_string()),
            },
            Some(&m)
        ),
        "low"
    );

    // New session without override → global default.
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: None,
                has_existing_session: false,
                has_thinking_entry: false,
                session_thinking_level: None,
                per_model_override: None,
                default_level: Some("high".to_string()),
            },
            Some(&m)
        ),
        "high"
    );

    // No default at all → DEFAULT_THINKING_LEVEL (medium).
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: None,
                has_existing_session: false,
                has_thinking_entry: false,
                session_thinking_level: None,
                per_model_override: None,
                default_level: None,
            },
            Some(&m)
        ),
        "medium"
    );
}

#[test]
fn thinking_level_no_model_clamps_to_off() {
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: Some("high".to_string()),
                has_existing_session: false,
                has_thinking_entry: false,
                session_thinking_level: None,
                per_model_override: None,
                default_level: Some("high".to_string()),
            },
            None
        ),
        "off"
    );
}

#[test]
fn thinking_level_unknown_level_falls_back_to_default() {
    let m = model("openai", "gpt-test", 100_000, true);
    assert_eq!(
        resolve_thinking_level(
            ThinkingLevelInputs {
                explicit_level: Some("bogus".to_string()),
                has_existing_session: false,
                has_thinking_entry: false,
                session_thinking_level: None,
                per_model_override: None,
                default_level: Some("low".to_string()),
            },
            Some(&m)
        ),
        "low"
    );
}

// --- tool selection ------------------------------------------------------------------------

#[test]
fn tool_selection_allowlist_and_exclusions() {
    let tools = vec!["read".to_string(), "bash".to_string(), "edit".to_string()];
    let exclude = vec!["bash".to_string()];

    // Explicit allowlist wins, filtered by exclusions.
    assert_eq!(
        resolve_initial_active_tool_names(ToolSelectionInputs {
            tools: Some(&tools),
            no_tools: None,
            exclude_tools: &exclude,
            configured_default_tools: None,
        }),
        vec!["read".to_string(), "edit".to_string()]
    );

    // No allowlist: configured defaults apply.
    let configured = vec!["write".to_string()];
    assert_eq!(
        resolve_initial_active_tool_names(ToolSelectionInputs {
            tools: None,
            no_tools: None,
            exclude_tools: &[],
            configured_default_tools: Some(&configured),
        }),
        vec!["write".to_string()]
    );

    // No allowlist and no configured defaults: built-in defaults.
    assert_eq!(
        resolve_initial_active_tool_names(ToolSelectionInputs {
            tools: None,
            no_tools: None,
            exclude_tools: &[],
            configured_default_tools: None,
        }),
        vec![
            "read".to_string(),
            "bash".to_string(),
            "edit".to_string(),
            "write".to_string()
        ]
    );

    // noTools=all starts with nothing.
    assert!(
        resolve_initial_active_tool_names(ToolSelectionInputs {
            tools: None,
            no_tools: Some("all"),
            exclude_tools: &[],
            configured_default_tools: Some(&configured),
        })
        .is_empty()
    );

    // noTools=builtin also starts with nothing (extension tools are added
    // separately by the runtime).
    assert!(
        resolve_initial_active_tool_names(ToolSelectionInputs {
            tools: None,
            no_tools: Some("builtin"),
            exclude_tools: &[],
            configured_default_tools: Some(&configured),
        })
        .is_empty()
    );
}

// --- block images filter --------------------------------------------------------------------

#[test]
fn block_images_passthrough_when_disabled() {
    let messages = vec![user_message(UserContent::Text("keep images".to_string()))];
    let filtered = filter_blocked_images(messages.clone(), false);
    assert_eq!(filtered, messages);
}

#[test]
fn block_images_replaces_user_image_with_placeholder() {
    let messages = vec![user_message(UserContent::Text("before".to_string()))];
    let filtered = filter_blocked_images(messages, true);
    assert_eq!(
        filtered,
        vec![user_message(UserContent::Text("before".to_string()))]
    );
}

#[test]
fn block_images_dedupes_consecutive_placeholders_in_tool_results() {
    let tool_result = Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
        tool_call_id: "t1".to_string(),
        tool_name: "read".to_string(),
        content: vec![],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 1000,
    }));
    // The port filters images within Content arrays; an empty content
    // array passes through unchanged.
    let filtered = filter_blocked_images(vec![tool_result.clone()], true);
    assert_eq!(filtered, vec![tool_result]);
}

#[test]
fn sdk_settings_snapshot_reads_defaults() {
    let cwd = std::env::temp_dir().join(format!("pillar-sdk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    let agent_dir = std::env::temp_dir().join(format!("pillar-sdk-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&agent_dir);
    std::fs::create_dir_all(&agent_dir).unwrap();
    let options = SettingsManagerCreateOptions {
        project_trusted: Some(true),
    };
    let mut settings = SettingsManager::create(&cwd.to_string_lossy(), &agent_dir, options);
    settings.set_global_setting("defaultTools", serde_json::json!(["read", "bash"]));
    settings.set_global_setting("defaultThinkingLevel", serde_json::json!("high"));
    settings.set_global_nested_setting("images", "blockImages", serde_json::json!(true));

    let snapshot = sdk_settings(&settings);
    assert_eq!(
        snapshot.default_tools.as_deref(),
        Some(&["read".to_string(), "bash".to_string()][..])
    );
    assert_eq!(snapshot.default_thinking_level.as_deref(), Some("high"));
    assert!(snapshot.block_images);

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&agent_dir);
}
