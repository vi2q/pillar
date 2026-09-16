//! The bridge from pillar-extensions to pillar-coding-agent's
//! ExtensionRunner: a loaded [`ExtensionRuntime`] becomes a
//! [`HostExtension`] whose per-event handlers dispatch into the Luau
//! VM, preserving registration order and the block/cancel semantics.
//!
//! divergences: the luaur VM handle is `Send` under the `send`
//! feature (serialized access, mirroring mlua's documented `send`
//! contract); the shared runtime sits behind a Mutex so the sync
//! handler closures can dispatch; handler errors map to the runner's
//! error-string path (the runner reports them via its listeners
//! without stopping other handlers).

use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::extensions_runner::{
    ExtensionEventPayload, ExtensionFlag, ExtensionHandler, ExtensionShortcut, HostExtension,
    RegisteredCommand,
};

use crate::runtime::{ExtensionLoadError, ExtensionRuntime};

/// Errors bridging the Luau runtime into runner handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// A handler (or the dispatch) failed; the message goes to the
    /// runner's error listeners.
    Handler(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handler(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Build a runner extension from a shared runtime: one handler entry
/// per event type that has Luau handlers, in the runner's
/// registration-order semantics (the VM-side handler list preserves
/// the order).
pub fn bridge_to_runner(
    path: &str,
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> Result<HostExtension, BridgeError> {
    let registry = runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .registry();
    let mut handlers: std::collections::BTreeMap<String, Vec<ExtensionHandler>> =
        Default::default();
    // Group the registry's (event, identity) pairs by event, keeping
    // registration order; one runner handler per Luau handler.
    for (event, _identity) in &registry.event_handlers {
        let runtime = Arc::clone(runtime);
        let dispatch_event = event.clone();
        let handler: ExtensionHandler = Arc::new(move |payload: &ExtensionEventPayload| {
            let mut runtime = runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = runtime
                .dispatch(&dispatch_event, payload.clone())
                .map_err(|error: ExtensionLoadError| error.to_string())?;
            match outcome {
                crate::runtime::HandlerOutcome::None => Ok(None),
                crate::runtime::HandlerOutcome::Block { reason } => {
                    // Upstream the runner's session-before short-circuit
                    // reads `cancel: true`; tool_call reads
                    // `block: true` — surface both shapes.
                    let mut result = serde_json::json!({ "cancel": true });
                    if let Some(reason) = reason {
                        result["reason"] = serde_json::Value::String(reason);
                        result["block"] = serde_json::Value::Bool(true);
                    }
                    Ok(Some(result))
                }
                crate::runtime::HandlerOutcome::Table(json) => Ok(Some(json)),
            }
        });
        handlers.entry(event.clone()).or_default().push(handler);
    }
    // Command registrations flow through as runner commands (the
    // handler dispatches the command event into the VM).
    let mut commands = Vec::new();
    for (name, opts) in &registry.commands {
        let description = opts
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.to_string());
        commands.push(RegisteredCommand {
            name: name.clone(),
            description: description.unwrap_or_default(),
            source_path: path.to_string(),
        });
    }
    // Flag registrations (upstream `registerFlag(name, { type, description })`):
    // the kind is the static "boolean" / "string" pair the host types.
    let mut flags = std::collections::BTreeMap::new();
    for (name, opts) in &registry.flags {
        let kind = match opts.get("type").and_then(serde_json::Value::as_str) {
            Some("string") => "string",
            _ => "boolean",
        };
        let description = opts
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        flags
            .entry(name.clone())
            .or_insert_with(|| ExtensionFlag { kind, description });
    }

    // Shortcut registrations (upstream `registerShortcut(key, { description })`):
    // keys are normalized the way the runner's conflict check expects.
    let mut shortcuts = std::collections::BTreeMap::new();
    for (key, opts) in &registry.shortcuts {
        let description = opts
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        shortcuts
            .entry(key.to_lowercase())
            .or_insert_with(|| ExtensionShortcut {
                extension_path: path.to_string(),
                description,
            });
    }

    Ok(HostExtension {
        path: path.to_string(),
        handlers,
        commands,
        tools: registry
            .tools
            .iter()
            .filter_map(|definition| {
                definition
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(|name| (name.to_string(), ()))
            })
            .collect(),
        flags,
        shortcuts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ExtensionRuntime;
    use luaur_rt::LuaSerdeExt;
    use pillar_coding_agent::core::extensions_runner::ExtensionRunner;

    fn shared_runtime() -> Arc<Mutex<ExtensionRuntime>> {
        Arc::new(Mutex::new(ExtensionRuntime::new()))
    }

    /// The bridge produces one runner handler per event with Luau
    /// handlers; the runner's emit dispatches into the VM. Only
    /// session-before events surface values through emit (upstream
    /// semantics); other events are observed via VM side effects.
    #[test]
    fn runner_emit_dispatches_into_the_vm() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "ext.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.on("session_start", function(event)
                        __seen = event.reason
                        return nil
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("ext.luau", &runtime).unwrap();
        assert!(extension.handlers.contains_key("session_start"));
        let runner = ExtensionRunner::new(vec![extension]);
        runner.emit(&serde_json::json!({
            "type": "session_start",
            "reason": "startup",
        }));
        // The handler ran inside the VM (side effect visible).
        let vm = runtime.lock().unwrap();
        let seen: serde_json::Value = vm
            .vm()
            .load("return __seen")
            .call(())
            .and_then(|value| vm.vm().from_value(value))
            .unwrap_or(serde_json::Value::Null);
        assert_eq!(seen, "startup");
    }

    /// A block return maps to the runner's cancel short-circuit.
    #[test]
    fn block_maps_to_cancel_short_circuit() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "blocker.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.on("session_before_switch", function(event)
                        return { block = true, reason = "no" }
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("blocker.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let result = runner.emit(&serde_json::json!({
            "type": "session_before_switch",
            "reason": "new",
        }));
        let result = result.expect("blocked result");
        assert_eq!(result["cancel"], true);
        assert_eq!(result["reason"], "no");
    }

    /// Command registrations surface as runner commands.
    #[test]
    fn commands_surface_as_runner_commands() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "cmd.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_command("hello", { description = "Say hello" })
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("cmd.luau", &runtime).unwrap();
        assert_eq!(extension.commands.len(), 1);
        assert_eq!(extension.commands[0].name, "hello");
        assert_eq!(extension.commands[0].description, "Say hello");
        let mut runner = ExtensionRunner::new(vec![extension]);
        assert!(
            runner
                .registered_commands()
                .iter()
                .any(|command| command.name == "hello")
        );
    }

    /// Tool registrations surface in the runner's tool set.
    #[test]
    fn tools_surface_in_the_runner_tool_set() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "tool.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_tool({ name = "greet" })
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("tool.luau", &runtime).unwrap();
        assert!(extension.tools.contains_key("greet"));
    }

    /// Flag and shortcut registrations surface in the runner's tables.
    #[test]
    fn flags_and_shortcuts_surface_in_the_runner() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "reg.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_flag("verbose", { type = "boolean", description = "Verbose output" })
                    pillar.register_flag("name", { type = "string" })
                    pillar.register_shortcut("Ctrl+Alt+K", { description = "Do a thing" })
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("reg.luau", &runtime).unwrap();
        assert_eq!(extension.flags["verbose"].kind, "boolean");
        assert_eq!(extension.flags["verbose"].description, "Verbose output");
        assert_eq!(extension.flags["name"].kind, "string");
        assert_eq!(extension.flags["name"].description, "");
        // Shortcut keys are normalized to lowercase.
        assert!(extension.shortcuts.contains_key("ctrl+alt+k"));
        assert_eq!(
            extension.shortcuts["ctrl+alt+k"].description,
            "Do a thing"
        );
        assert_eq!(extension.shortcuts["ctrl+alt+k"].extension_path, "reg.luau");

        let mut runner = ExtensionRunner::new(vec![extension]);
        assert_eq!(runner.flags().len(), 2);
        let resolved = runner.shortcuts(&std::collections::BTreeMap::new());
        assert!(resolved.contains_key("ctrl+alt+k"));
    }

    /// A Luau handler error becomes the runner's error-string path.
    #[test]
    fn handler_errors_become_error_strings() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "erroring.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.on("agent_start", function()
                        error("boom")
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("erroring.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let reported = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&reported);
        let mut runner = runner;
        runner.on_error(Box::new(move |error| {
            sink.lock().unwrap().push(error.error.clone());
        }));
        let result = runner.emit(&serde_json::json!({ "type": "agent_start" }));
        // The runner reports the error via listeners; the emit result
        // stays None (no value).
        assert!(result.is_none());
        assert!(
            !reported.lock().unwrap().is_empty(),
            "expected reported errors"
        );
    }
}
