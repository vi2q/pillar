//! Parity tests for extensions/runner.ts (pi v0.84.3), the execution
//! core: builtin keybinding building with reserved-action precedence,
//! shortcut conflicts, command invocation-name dedupe, session-before
//! short-circuiting, tool_call blocking, tool_result chaining,
//! message_end role checks, input transform/handled chaining, context
//! replacement, before_agent_start combining, and project trust.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pillar_coding_agent::core::extensions_runner::{
    ExtensionError, ExtensionFlag, ExtensionRunner, HostExtension,
    RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS, RegisteredCommand, build_builtin_keybindings,
    emit_project_trust_event, emit_session_shutdown_event,
};
use pillar_coding_agent::core::skills::ResourceDiagnostic;

fn handler(
    f: impl Fn(&serde_json::Value) -> pillar_coding_agent::core::extensions_runner::HandlerResult
    + Send
    + Sync
    + 'static,
) -> pillar_coding_agent::core::extensions_runner::ExtensionHandler {
    Arc::new(f)
}

fn extension(path: &str, handlers: &[(&str, usize)]) -> HostExtension {
    let mut map = BTreeMap::new();
    for (event, count) in handlers {
        map.insert(
            event.to_string(),
            (0..*count).map(|_| handler(|_| Ok(None))).collect(),
        );
    }
    HostExtension {
        path: path.to_string(),
        handlers: map,
        commands: Vec::new(),
        tools: BTreeMap::new(),
        flags: BTreeMap::new(),
        shortcuts: BTreeMap::new(),
        message_renderers: Default::default(),
        entry_renderers: Default::default(),
        markdown_transformer: None,
    }
}

// --- builtin keybindings -----------------------------------------------------------------

#[test]
fn builtin_keybindings_normalize_and_reserve() {
    let mut resolved = BTreeMap::new();
    resolved.insert(
        "app.interrupt".to_string(),
        vec!["escape".to_string(), "CTRL+C".to_string()],
    );
    resolved.insert("app.tools.expand".to_string(), vec!["TAB".to_string()]);

    let builtins = build_builtin_keybindings(&resolved);
    assert_eq!(builtins["escape"].keybinding, "app.interrupt");
    assert!(builtins["escape"].restrict_override);
    assert_eq!(builtins["ctrl+c"].keybinding, "app.interrupt");
    assert_eq!(builtins["tab"].keybinding, "app.tools.expand");
    assert!(builtins["tab"].restrict_override);
}

#[test]
fn reserved_action_wins_keybinding_race() {
    let mut resolved = BTreeMap::new();
    // Non-reserved action binds "x" first conceptually, but the reserved
    // one wins regardless of iteration order (BTreeMap iterates sorted;
    // test both orders via two keys).
    resolved.insert("app.custom.one".to_string(), vec!["x".to_string()]);
    resolved.insert("app.interrupt".to_string(), vec!["x".to_string()]);
    let builtins = build_builtin_keybindings(&resolved);
    assert_eq!(builtins["x"].keybinding, "app.interrupt");
    assert!(builtins["x"].restrict_override);

    // Reversed insertion: reserved still wins.
    let mut reversed = BTreeMap::new();
    reversed.insert("app.interrupt".to_string(), vec!["y".to_string()]);
    reversed.insert("app.custom.two".to_string(), vec!["y".to_string()]);
    let builtins = build_builtin_keybindings(&reversed);
    assert_eq!(builtins["y"].keybinding, "app.interrupt");
}

#[test]
fn reserved_list_matches_upstream_size() {
    assert_eq!(RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.len(), 17);
    assert!(RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.contains(&"app.exit"));
    assert!(RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.contains(&"tui.input.submit"));
}

// --- shortcuts ------------------------------------------------------------------------------

#[test]
fn shortcut_reserved_conflict_skips_and_diagnoses() {
    let mut ext = extension("ext-a", &[]);
    ext.shortcuts.insert(
        "escape".to_string(),
        pillar_coding_agent::core::extensions_runner::ExtensionShortcut {
            extension_path: "ext-a".to_string(),
            description: "mine".to_string(),
        },
    );
    let mut resolved = BTreeMap::new();
    resolved.insert("app.interrupt".to_string(), vec!["escape".to_string()]);

    let mut runner = ExtensionRunner::new(vec![ext]);
    runner.set_has_ui(true);
    let shortcuts = runner.shortcuts(&resolved);
    assert!(shortcuts.is_empty(), "{shortcuts:?}");
    let diagnostics = runner.shortcut_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(matches!(
        &diagnostics[0],
        ResourceDiagnostic::Warning { message, .. }
            if message.contains("conflicts with built-in shortcut. Skipping.")
    ));
}

#[test]
fn shortcut_non_reserved_override_warns_and_wins() {
    let mut ext = extension("ext-a", &[]);
    ext.shortcuts.insert(
        "f9".to_string(),
        pillar_coding_agent::core::extensions_runner::ExtensionShortcut {
            extension_path: "ext-a".to_string(),
            description: "mine".to_string(),
        },
    );
    let mut resolved = BTreeMap::new();
    resolved.insert("app.custom.thing".to_string(), vec!["f9".to_string()]);

    let mut runner = ExtensionRunner::new(vec![ext]);
    runner.set_has_ui(true);
    let shortcuts = runner.shortcuts(&resolved);
    assert!(shortcuts.contains_key("f9"));
    assert!(
        runner.shortcut_diagnostics()[0]
            .message()
            .contains("Using ext-a")
    );
}

#[test]
fn shortcut_extension_vs_extension_later_wins() {
    let mut a = extension("ext-a", &[]);
    a.shortcuts.insert(
        "f9".to_string(),
        pillar_coding_agent::core::extensions_runner::ExtensionShortcut {
            extension_path: "ext-a".to_string(),
            description: "first".to_string(),
        },
    );
    let mut b = extension("ext-b", &[]);
    b.shortcuts.insert(
        "f9".to_string(),
        pillar_coding_agent::core::extensions_runner::ExtensionShortcut {
            extension_path: "ext-b".to_string(),
            description: "second".to_string(),
        },
    );

    let resolved = BTreeMap::new();
    let mut runner = ExtensionRunner::new(vec![a, b]);
    runner.set_has_ui(true);
    let shortcuts = runner.shortcuts(&resolved);
    assert_eq!(shortcuts["f9"].extension_path, "ext-b");
    assert!(
        runner.shortcut_diagnostics()[0]
            .message()
            .contains("registered by both ext-a and ext-b")
    );
}

// --- commands ---------------------------------------------------------------------------------

#[test]
fn command_invocation_name_dedupe() {
    let mut a = extension("ext-a", &[]);
    a.commands = vec![RegisteredCommand {
        name: "deploy".to_string(),
        description: "A".to_string(),
        source_path: "ext-a".to_string(),
    }];
    let mut b = extension("ext-b", &[]);
    b.commands = vec![
        RegisteredCommand {
            name: "deploy".to_string(),
            description: "B".to_string(),
            source_path: "ext-b".to_string(),
        },
        RegisteredCommand {
            name: "unique".to_string(),
            description: "U".to_string(),
            source_path: "ext-b".to_string(),
        },
    ];

    let mut runner = ExtensionRunner::new(vec![a, b]);
    let commands = runner.registered_commands();
    // Upstream keeps the numeric suffix for duplicated names: the first
    // occurrence is "deploy:1", not "deploy".
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c.invocation_name.as_str())
        .collect();
    assert_eq!(names, ["deploy:1", "deploy:2", "unique"]);

    // getCommand resolves by invocation name.
    assert!(runner.command("deploy:1").is_some());
    assert!(runner.command("deploy").is_none());
}

#[test]
fn command_collision_skips_taken_suffixes() {
    // Commands literally named "deploy:2" plus a duplicate "deploy" force
    // the dedupe to skip to "deploy:3".
    let mut a = extension("ext-a", &[]);
    a.commands = vec![RegisteredCommand {
        name: "deploy".to_string(),
        description: "A".to_string(),
        source_path: "ext-a".to_string(),
    }];
    let mut b = extension("ext-b", &[]);
    b.commands = vec![
        RegisteredCommand {
            name: "deploy".to_string(),
            description: "B".to_string(),
            source_path: "ext-b".to_string(),
        },
        RegisteredCommand {
            name: "deploy:2".to_string(),
            description: "C".to_string(),
            source_path: "ext-b".to_string(),
        },
    ];
    let mut runner = ExtensionRunner::new(vec![a, b]);
    let commands = runner.registered_commands();
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c.invocation_name.as_str())
        .collect();
    // The literal "deploy:2" command also collides with the dedupe's
    // "deploy:2", so it skips to "deploy:2:2" (name includes the base's
    // colon; upstream builds `${name}:${suffix}` verbatim).
    assert_eq!(names, ["deploy:1", "deploy:2", "deploy:2:2"]);
}

// --- flags / tools ------------------------------------------------------------------------------

#[test]
fn flags_first_wins_and_flag_values_round_trip() {
    let mut a = extension("ext-a", &[]);
    a.flags.insert(
        "verbose".to_string(),
        ExtensionFlag {
            kind: "boolean",
            description: "A".to_string(),
        },
    );
    let mut b = extension("ext-b", &[]);
    b.flags.insert(
        "verbose".to_string(),
        ExtensionFlag {
            kind: "boolean",
            description: "B".to_string(),
        },
    );

    let mut runner = ExtensionRunner::new(vec![a, b]);
    assert_eq!(runner.flags()["verbose"].description, "A");
    runner.set_flag_value("verbose", serde_json::json!(true));
    assert_eq!(runner.flag_values()["verbose"], serde_json::json!(true));
}

#[test]
fn tool_first_registration_wins() {
    let mut a = extension("ext-a", &[]);
    a.tools.insert("tool1".to_string(), ());
    let mut b = extension("ext-b", &[]);
    b.tools.insert("tool1".to_string(), ());
    b.tools.insert("tool2".to_string(), ());

    let runner = ExtensionRunner::new(vec![a, b]);
    assert_eq!(runner.all_registered_tool_names(), ["tool1", "tool2"]);
    assert_eq!(runner.tool_owner("tool1"), Some("ext-a".to_string()));
    assert_eq!(runner.tool_owner("tool2"), Some("ext-b".to_string()));
}

// --- emit semantics ---------------------------------------------------------------------------

#[test]
fn session_before_switch_short_circuits_on_cancel() {
    let order = Arc::new(AtomicUsize::new(0));
    let order2 = order.clone();
    let mut first = extension("ext-a", &[]);
    first.handlers.insert(
        "session_before_switch".to_string(),
        vec![handler(move |_| {
            order2.fetch_add(1, Ordering::SeqCst);
            Ok(Some(serde_json::json!({"cancel": true})))
        })],
    );
    let mut second = extension("ext-b", &[]);
    second.handlers.insert(
        "session_before_switch".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"cancel": false}))))],
    );

    let runner = ExtensionRunner::new(vec![first, second]);
    let result = runner.emit(&serde_json::json!({"type": "session_before_switch"}));
    assert_eq!(result.unwrap()["cancel"], serde_json::json!(true));
    assert_eq!(order.load(Ordering::SeqCst), 1); // second handler never ran
}

#[test]
fn emit_collects_errors_without_stopping_other_handlers() {
    let errors: Arc<Mutex<Vec<ExtensionError>>> = Arc::new(Mutex::new(Vec::new()));
    let errors2 = errors.clone();

    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "session_start".to_string(),
        vec![handler(|_| Err("boom".to_string()))],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "session_start".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"ok": true}))))],
    );

    let mut runner = ExtensionRunner::new(vec![a, b]);
    runner.on_error(Box::new(move |error| {
        errors2.lock().unwrap().push(error.clone())
    }));
    // Non-session-before events return undefined (upstream emit result).
    let result = runner.emit(&serde_json::json!({"type": "session_start"}));
    assert!(result.is_none());
    let errors = errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].extension_path, "ext-a");
    assert_eq!(errors[0].error, "boom");
}

#[test]
fn tool_call_block_short_circuits_and_errors_propagate() {
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "tool_call".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"block": true}))))],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "tool_call".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"block": false}))))],
    );
    let runner = ExtensionRunner::new(vec![a, b]);
    let result = runner
        .emit_tool_call(&serde_json::json!({"type": "tool_call", "toolName": "bash"}))
        .unwrap();
    assert_eq!(result.unwrap()["block"], serde_json::json!(true));

    // Handler errors propagate (upstream lets them throw).
    let mut failing = extension("ext-c", &[]);
    failing.handlers.insert(
        "tool_call".to_string(),
        vec![handler(|_| Err("extension failed".to_string()))],
    );
    let runner = ExtensionRunner::new(vec![failing]);
    let error = runner
        .emit_tool_call(&serde_json::json!({"type": "tool_call"}))
        .unwrap_err();
    assert_eq!(error, "extension failed");
}

#[test]
fn tool_result_chains_fields_and_reports_modified() {
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "tool_result".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"isError": true}))))],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "tool_result".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({"content": [{"type": "text"}]})))
        })],
    );

    let runner = ExtensionRunner::new(vec![a, b]);
    let event = serde_json::json!({"type": "tool_result", "content": [], "isError": false});
    let result = runner.emit_tool_result(&event).unwrap();
    assert_eq!(result["isError"], serde_json::json!(true));
    assert_eq!(result["content"], serde_json::json!([{"type": "text"}]));

    // No modification → undefined.
    let mut no_change = extension("ext-c", &[]);
    no_change
        .handlers
        .insert("tool_result".to_string(), vec![handler(|_| Ok(None))]);
    let runner = ExtensionRunner::new(vec![no_change]);
    assert!(runner.emit_tool_result(&event).is_none());
}

#[test]
fn message_end_rejects_role_changes_and_chains() {
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "message_end".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({
                "message": {"role": "assistant", "content": "changed"}
            })))
        })],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "message_end".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({
                "message": {"role": "user", "content": "bad role"}
            })))
        })],
    );

    let runner = ExtensionRunner::new(vec![a, b]);
    let message = serde_json::json!({"role": "assistant", "content": "original"});
    let result = runner.emit_message_end(message).unwrap();
    assert_eq!(result["content"], "changed");

    // Unmodified → undefined.
    let runner = ExtensionRunner::new(vec![extension("ext-c", &[])]);
    assert!(
        runner
            .emit_message_end(serde_json::json!({"role": "assistant", "content": "x"}))
            .is_none()
    );
}

#[test]
fn input_transform_chains_and_handled_short_circuits() {
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "input".to_string(),
        vec![handler(|_| {
            Ok(Some(
                serde_json::json!({"action": "transform", "text": "one"}),
            ))
        })],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "input".to_string(),
        vec![handler(|_| {
            Ok(Some(
                serde_json::json!({"action": "transform", "text": "two"}),
            ))
        })],
    );
    let runner = ExtensionRunner::new(vec![a, b]);
    let result = runner.emit_input("start", "interactive", None);
    assert_eq!(result["action"], "transform");
    assert_eq!(result["text"], "two");

    // handled short-circuits.
    let mut handled = extension("ext-c", &[]);
    handled.handlers.insert(
        "input".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({"action": "handled"})))
        })],
    );
    let runner = ExtensionRunner::new(vec![handled]);
    let result = runner.emit_input("start", "interactive", None);
    assert_eq!(result["action"], "handled");

    // No handler → continue with unchanged text.
    let runner = ExtensionRunner::new(vec![extension("ext-d", &[])]);
    let result = runner.emit_input("same", "interactive", None);
    assert_eq!(result["action"], "continue");
}

#[test]
fn context_replaces_messages_and_before_agent_start_combines() {
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "context".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({"messages": [1, 2]})))
        })],
    );
    let mut b = extension("ext-b", &[]);
    b.handlers.insert(
        "context".to_string(),
        vec![handler(|event| {
            let messages = event["messages"].as_array().unwrap();
            Ok(Some(serde_json::json!({"messages": [messages, 3]})))
        })],
    );
    let runner = ExtensionRunner::new(vec![a, b]);
    let result = runner.emit_context(serde_json::json!([]));
    assert_eq!(result, serde_json::json!([[1, 2], 3]));

    // before_agent_start: prompt override + message accumulation.
    let mut a = extension("ext-a", &[]);
    a.handlers.insert(
        "before_agent_start".to_string(),
        vec![handler(|_| {
            Ok(Some(
                serde_json::json!({"message": {"role": "user"}, "systemPrompt": "new"}),
            ))
        })],
    );
    let runner = ExtensionRunner::new(vec![a]);
    let result = runner.emit_before_agent_start("prompt", "base").unwrap();
    assert_eq!(result["systemPrompt"], "new");
    assert_eq!(result["messages"][0]["role"], "user");

    // No changes → undefined.
    let runner = ExtensionRunner::new(vec![extension("ext-c", &[])]);
    assert!(runner.emit_before_agent_start("p", "s").is_none());
}

// --- shutdown / trust ---------------------------------------------------------------------------

#[test]
fn shutdown_event_emitted_only_with_handlers() {
    let called = Arc::new(AtomicUsize::new(0));
    let called2 = called.clone();
    let mut with_handlers = extension("ext-a", &[]);
    with_handlers.handlers.insert(
        "session_shutdown".to_string(),
        vec![handler(move |_| {
            called2.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        })],
    );
    let mut runner = ExtensionRunner::new(vec![with_handlers]);
    assert!(emit_session_shutdown_event(&mut runner, "quit", None));
    assert_eq!(called.load(Ordering::SeqCst), 1);

    let mut runner = ExtensionRunner::new(vec![extension("ext-b", &[])]);
    assert!(!emit_session_shutdown_event(&mut runner, "quit", None));
}

#[test]
fn project_trust_first_decision_wins_and_undecided_falls_through() {
    let mut undecided = extension("ext-a", &[]);
    undecided.handlers.insert(
        "project_trust".to_string(),
        vec![handler(|_| {
            Ok(Some(serde_json::json!({"trusted": "undecided"})))
        })],
    );
    let mut yes = extension("ext-b", &[]);
    yes.handlers.insert(
        "project_trust".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"trusted": "yes"}))))],
    );
    let mut after = extension("ext-c", &[]);
    after.handlers.insert(
        "project_trust".to_string(),
        vec![handler(|_| Ok(Some(serde_json::json!({"trusted": "no"}))))],
    );

    let (decision, errors) = emit_project_trust_event(&[undecided, yes, after], false);
    assert_eq!(
        decision,
        Some(pillar_coding_agent::core::extensions_runner::ProjectTrustDecision::Yes)
    );
    assert!(errors.is_empty());

    // All undecided → None.
    let (decision, errors) = emit_project_trust_event(&[extension("ext-d", &[])], true);
    assert_eq!(decision, None);
    assert!(errors.is_empty());
}

// --- stale runner -------------------------------------------------------------------------------

#[test]
fn invalidate_records_first_message_and_assert_active_fails() {
    let mut runner = ExtensionRunner::new(vec![extension("ext-a", &[])]);
    runner.assert_active().unwrap();
    runner.invalidate("stale now");
    runner.invalidate("second message");
    let error = runner.assert_active().unwrap_err();
    assert_eq!(error, "stale now");
}

trait MessageExt {
    fn message(&self) -> &str;
}
impl MessageExt for ResourceDiagnostic {
    fn message(&self) -> &str {
        match self {
            ResourceDiagnostic::Warning { message, .. }
            | ResourceDiagnostic::Collision { message, .. } => message,
        }
    }
}
