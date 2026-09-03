//! Port of packages/coding-agent/src/core/extensions/runner.ts (pi
//! v0.84.3), the execution core: extension handler dispatch order,
//! short-circuiting semantics for session-before events, tool_call
//! blocking, tool_result chaining, message_end role preservation, input
//! transform/handled chaining, reserved-keybinding shortcut conflicts,
//! command invocation-name dedupe (`name:2`), and stale-runner guards.
//!
//! divergences: extension handlers are host-supplied closures per event
//! type (the JS extension runtime, Extension objects, and TUI renderers
//! are not ported); context/command context objects become plain getter
//! sets; the model registry is not ported so provider registration is a
//! callback.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::skills::ResourceDiagnostic;

/// Extension flag definition (upstream `ExtensionFlag` subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFlag {
    /// "boolean" | "string".
    pub kind: &'static str,
    pub description: String,
}

/// A host-supplied extension (upstream `Extension`, capability subset).
#[derive(Clone)]
pub struct HostExtension {
    /// Extension path used in diagnostics (`<inline:N>` for factories).
    pub path: String,
    /// Handlers by event type, in registration order.
    pub handlers: BTreeMap<String, Vec<ExtensionHandler>>,
    /// Registered commands in registration order.
    pub commands: Vec<RegisteredCommand>,
    /// Registered tools by name (first per name wins across extensions).
    pub tools: BTreeMap<String, ()>,
    /// Registered flags by name.
    pub flags: BTreeMap<String, ExtensionFlag>,
    /// Keyboard shortcuts by normalized key id.
    pub shortcuts: BTreeMap<String, ExtensionShortcut>,
}

/// A handler closure (upstream an async JS handler; the port is sync).
pub type ExtensionHandler =
    std::sync::Arc<dyn Fn(&ExtensionEventPayload) -> HandlerResult + Send + Sync>;

/// The event payload passed to handlers (upstream the event object; the
/// port keeps the JSON shape so hosts can extend).
pub type ExtensionEventPayload = serde_json::Value;

/// A handler result: a JSON value or an error message.
pub type HandlerResult = Result<Option<serde_json::Value>, String>;

/// A command registered by an extension (upstream `RegisteredCommand`).
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub source_path: String,
}

/// A command after invocation-name dedupe (upstream `ResolvedCommand`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCommand {
    pub name: String,
    pub description: String,
    pub source_path: String,
    pub invocation_name: String,
}

/// A keyboard shortcut registered by an extension (upstream
/// `ExtensionShortcut`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionShortcut {
    pub extension_path: String,
    pub description: String,
}

/// An extension error (upstream `ExtensionError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
    pub stack: Option<String>,
}

/// Keybindings that extensions cannot override (upstream
/// `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`).
pub const RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS: [&str; 17] = [
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// Built-in keybinding entry (upstream `BuiltInKeyBindings` value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinKeybinding {
    pub keybinding: String,
    pub restrict_override: bool,
}

/// Build a normalized key -> builtin keybinding map (upstream
/// `buildBuiltinKeybindings`): multiple actions may bind the same key; the
/// reserved action wins regardless of iteration order.
pub fn build_builtin_keybindings(
    resolved_keybindings: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, BuiltinKeybinding> {
    let mut builtin: BTreeMap<String, BuiltinKeybinding> = BTreeMap::new();
    for (keybinding, keys) in resolved_keybindings {
        let restrict_override =
            RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.contains(&keybinding.as_str());
        for key in keys {
            let normalized = key.to_lowercase();
            if let Some(existing) = builtin.get(&normalized) {
                if existing.restrict_override && !restrict_override {
                    continue;
                }
            }
            builtin.insert(
                normalized,
                BuiltinKeybinding {
                    keybinding: keybinding.clone(),
                    restrict_override,
                },
            );
        }
    }
    builtin
}

type ErrorListener = Box<dyn Fn(&ExtensionError) + Send>;

/// The extension runner (upstream `ExtensionRunner`).
pub struct ExtensionRunner {
    extensions: Vec<HostExtension>,
    error_listeners: Vec<ErrorListener>,
    shortcut_diagnostics: Vec<ResourceDiagnostic>,
    command_diagnostics: Vec<ResourceDiagnostic>,
    stale_message: Option<String>,
    flag_values: BTreeMap<String, serde_json::Value>,
    has_ui: bool,
}

impl ExtensionRunner {
    pub fn new(extensions: Vec<HostExtension>) -> Self {
        Self {
            extensions,
            error_listeners: Vec::new(),
            shortcut_diagnostics: Vec::new(),
            command_diagnostics: Vec::new(),
            stale_message: None,
            flag_values: BTreeMap::new(),
            has_ui: false,
        }
    }

    pub fn set_has_ui(&mut self, has_ui: bool) {
        self.has_ui = has_ui;
    }

    pub fn has_ui(&self) -> bool {
        self.has_ui
    }

    pub fn extension_paths(&self) -> Vec<String> {
        self.extensions.iter().map(|e| e.path.clone()).collect()
    }

    /// All registered tools across extensions; first per name wins
    /// (upstream `getAllRegisteredTools`).
    pub fn all_registered_tool_names(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut names = Vec::new();
        for ext in &self.extensions {
            for name in ext.tools.keys() {
                if seen.insert(name.clone()) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// The extension that registered a tool, if any (upstream
    /// `getToolDefinition` resolves across extensions in order).
    pub fn tool_owner(&self, tool_name: &str) -> Option<String> {
        for ext in &self.extensions {
            if ext.tools.contains_key(tool_name) {
                return Some(ext.path.clone());
            }
        }
        None
    }

    /// All flags; first per name wins (upstream `getFlags`).
    pub fn flags(&self) -> BTreeMap<String, ExtensionFlag> {
        let mut all = BTreeMap::new();
        for ext in &self.extensions {
            for (name, flag) in &ext.flags {
                all.entry(name.clone()).or_insert_with(|| flag.clone());
            }
        }
        all
    }

    pub fn set_flag_value(&mut self, name: &str, value: serde_json::Value) {
        self.flag_values.insert(name.to_string(), value);
    }

    pub fn flag_values(&self) -> BTreeMap<String, serde_json::Value> {
        self.flag_values.clone()
    }

    /// Resolve extension shortcuts against builtin keybindings (upstream
    /// `getShortcuts`): reserved builtins skip with a diagnostic, non
    /// reserved builtins warn and are overridden, extension-vs-extension
    /// conflicts warn and the later one wins.
    pub fn shortcuts(
        &mut self,
        resolved_keybindings: &BTreeMap<String, Vec<String>>,
    ) -> BTreeMap<String, ExtensionShortcut> {
        self.shortcut_diagnostics.clear();
        let builtin = build_builtin_keybindings(resolved_keybindings);
        let mut extension_shortcuts: BTreeMap<String, ExtensionShortcut> = BTreeMap::new();

        for ext in &self.extensions {
            for (key, shortcut) in &ext.shortcuts {
                let normalized = key.to_lowercase();
                let builtin_entry = builtin.get(&normalized);
                if builtin_entry.is_some_and(|b| b.restrict_override) {
                    let message = format!(
                        "Extension shortcut '{key}' from {} conflicts with built-in shortcut. Skipping.",
                        shortcut.extension_path
                    );
                    self.shortcut_diagnostics.push(ResourceDiagnostic::Warning {
                        message,
                        path: shortcut.extension_path.clone(),
                    });
                    continue;
                }
                if let Some(b) = builtin_entry {
                    if !b.restrict_override {
                        let message = format!(
                            "Extension shortcut conflict: '{key}' is built-in shortcut for {} and {}. Using {}.",
                            b.keybinding, shortcut.extension_path, shortcut.extension_path
                        );
                        self.shortcut_diagnostics.push(ResourceDiagnostic::Warning {
                            message,
                            path: shortcut.extension_path.clone(),
                        });
                    }
                }
                if let Some(existing) = extension_shortcuts.get(&normalized) {
                    let message = format!(
                        "Extension shortcut conflict: '{key}' registered by both {} and {}. Using {}.",
                        existing.extension_path, shortcut.extension_path, shortcut.extension_path
                    );
                    self.shortcut_diagnostics.push(ResourceDiagnostic::Warning {
                        message,
                        path: shortcut.extension_path.clone(),
                    });
                }
                extension_shortcuts.insert(normalized, shortcut.clone());
            }
        }
        extension_shortcuts
    }

    pub fn shortcut_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.shortcut_diagnostics
    }

    /// Mark the runner stale (upstream `invalidate`): the first message
    /// wins, matching upstream's once-only semantics.
    pub fn invalidate(&mut self, message: &str) {
        if self.stale_message.is_none() {
            self.stale_message = Some(message.to_string());
        }
    }

    pub fn assert_active(&self) -> Result<(), String> {
        match &self.stale_message {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    pub fn on_error(&mut self, listener: Box<dyn Fn(&ExtensionError) + Send>) {
        self.error_listeners.push(listener);
    }

    pub fn emit_error(&self, error: ExtensionError) {
        for listener in &self.error_listeners {
            listener(&error);
        }
    }

    pub fn has_handlers(&self, event_type: &str) -> bool {
        for ext in &self.extensions {
            if ext
                .handlers
                .get(event_type)
                .is_some_and(|handlers| !handlers.is_empty())
            {
                return true;
            }
        }
        false
    }

    /// Resolve command invocation names (upstream
    /// `resolveRegisteredCommands`): duplicated names get `name:2`,
    /// `name:3`, ...; collisions with already-taken names skip ahead.
    pub fn registered_commands(&mut self) -> Vec<ResolvedCommand> {
        self.command_diagnostics.clear();
        let mut commands: Vec<&RegisteredCommand> = Vec::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for ext in &self.extensions {
            for command in &ext.commands {
                commands.push(command);
                *counts.entry(command.name.clone()).or_insert(0) += 1;
            }
        }

        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut taken: BTreeSet<String> = BTreeSet::new();
        let mut resolved = Vec::new();
        for command in commands {
            let occurrence = seen.get(&command.name).copied().unwrap_or(0) + 1;
            seen.insert(command.name.clone(), occurrence);

            let mut invocation_name = if counts.get(&command.name).copied().unwrap_or(0) > 1 {
                format!("{}:{}", command.name, occurrence)
            } else {
                command.name.clone()
            };
            if taken.contains(&invocation_name) {
                let mut suffix = occurrence;
                loop {
                    suffix += 1;
                    invocation_name = format!("{}:{}", command.name, suffix);
                    if !taken.contains(&invocation_name) {
                        break;
                    }
                }
            }
            taken.insert(invocation_name.clone());
            resolved.push(ResolvedCommand {
                name: command.name.clone(),
                description: command.description.clone(),
                source_path: command.source_path.clone(),
                invocation_name,
            });
        }
        resolved
    }

    pub fn command_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.command_diagnostics
    }

    pub fn command(&mut self, name: &str) -> Option<ResolvedCommand> {
        self.registered_commands()
            .into_iter()
            .find(|command| command.invocation_name == name)
    }

    /// Generic event dispatch (upstream `emit`): handlers run in extension
    /// order; errors go to listeners without stopping other handlers;
    /// session-before events short-circuit on `cancel: true`.
    pub fn emit(&self, event: &ExtensionEventPayload) -> Option<serde_json::Value> {
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let mut result: Option<serde_json::Value> = None;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get(event_type) else {
                continue;
            };
            for handler in handlers {
                let is_session_before = matches!(
                    event_type,
                    "session_before_switch"
                        | "session_before_fork"
                        | "session_before_compact"
                        | "session_before_tree"
                );
                match handler(event) {
                    Ok(Some(handler_result)) => {
                        if is_session_before {
                            result = Some(handler_result.clone());
                            if handler_result
                                .get("cancel")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false)
                            {
                                return result;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: event_type.to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        result
    }

    /// `message_end` chaining (upstream `emitMessageEnd`): handlers
    /// transform the message; role changes are rejected; returns the final
    /// message when modified.
    pub fn emit_message_end(&self, message: serde_json::Value) -> Option<serde_json::Value> {
        let mut current = message.clone();
        let mut modified = false;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("message_end") else {
                continue;
            };
            for handler in handlers {
                let event = serde_json::json!({ "type": "message_end", "message": current });
                match handler(&event) {
                    Ok(Some(handler_result)) => {
                        let Some(new_message) = handler_result.get("message") else {
                            continue;
                        };
                        let current_role = current.get("role").and_then(serde_json::Value::as_str);
                        let new_role = new_message.get("role").and_then(serde_json::Value::as_str);
                        if current_role != new_role {
                            self.emit_error(ExtensionError {
                                extension_path: ext.path.clone(),
                                event: "message_end".to_string(),
                                error:
                                    "message_end handlers must return a message with the same role"
                                        .to_string(),
                                stack: None,
                            });
                            continue;
                        }
                        current = new_message.clone();
                        modified = true;
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "message_end".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        modified.then_some(current)
    }

    /// `tool_result` chaining (upstream `emitToolResult`): fields merge
    /// across handlers; returns undefined when nothing changed.
    pub fn emit_tool_result(&self, event: &ExtensionEventPayload) -> Option<serde_json::Value> {
        let mut current = event.clone();
        let mut modified = false;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("tool_result") else {
                continue;
            };
            for handler in handlers {
                match handler(&current) {
                    Ok(Some(handler_result)) => {
                        for field in ["content", "details", "isError", "usage"] {
                            if let Some(value) = handler_result.get(field) {
                                if let Some(obj) = current.as_object_mut() {
                                    obj.insert(field.to_string(), value.clone());
                                }
                                modified = true;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "tool_result".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        modified.then_some(current)
    }

    /// `tool_call` dispatch (upstream `emitToolCall`): non-error results
    /// accumulate; `block: true` short-circuits. Unlike most emit paths,
    /// handler errors propagate (upstream lets them throw).
    pub fn emit_tool_call(
        &self,
        event: &ExtensionEventPayload,
    ) -> Result<Option<serde_json::Value>, String> {
        let mut result: Option<serde_json::Value> = None;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("tool_call") else {
                continue;
            };
            for handler in handlers {
                if let Some(handler_result) = handler(event)? {
                    result = Some(handler_result.clone());
                    if handler_result
                        .get("block")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        return Ok(result);
                    }
                }
            }
        }
        Ok(result)
    }

    /// `user_bash` dispatch (upstream `emitUserBash`): the first
    /// non-undefined result wins.
    pub fn emit_user_bash(&self, event: &ExtensionEventPayload) -> Option<serde_json::Value> {
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("user_bash") else {
                continue;
            };
            for handler in handlers {
                match handler(event) {
                    Ok(Some(result)) => return Some(result),
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "user_bash".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        None
    }

    /// `context` chaining (upstream `emitContext`): each handler may
    /// replace the message list wholesale.
    pub fn emit_context(&self, messages: serde_json::Value) -> serde_json::Value {
        let mut current = messages;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("context") else {
                continue;
            };
            for handler in handlers {
                let event = serde_json::json!({ "type": "context", "messages": current });
                match handler(&event) {
                    Ok(Some(result)) => {
                        if let Some(new_messages) = result.get("messages") {
                            current = new_messages.clone();
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "context".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        current
    }

    /// `before_agent_start` combining (upstream `emitBeforeAgentStart`):
    /// system prompt overrides chain; messages accumulate.
    pub fn emit_before_agent_start(
        &self,
        prompt: &str,
        system_prompt: &str,
    ) -> Option<serde_json::Value> {
        let mut current_system_prompt = system_prompt.to_string();
        let mut messages: Vec<serde_json::Value> = Vec::new();
        let mut system_prompt_modified = false;
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("before_agent_start") else {
                continue;
            };
            for handler in handlers {
                let event = serde_json::json!({
                    "type": "before_agent_start",
                    "prompt": prompt,
                    "systemPrompt": current_system_prompt,
                });
                match handler(&event) {
                    Ok(Some(result)) => {
                        if let Some(message) = result.get("message") {
                            messages.push(message.clone());
                        }
                        if let Some(new_prompt) = result.get("systemPrompt") {
                            current_system_prompt =
                                new_prompt.as_str().unwrap_or_default().to_string();
                            system_prompt_modified = true;
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "before_agent_start".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        if !messages.is_empty() || system_prompt_modified {
            Some(serde_json::json!({
                "messages": if messages.is_empty() { None } else { Some(serde_json::Value::Array(messages)) },
                "systemPrompt": if system_prompt_modified { Some(serde_json::Value::String(current_system_prompt)) } else { None },
            }))
        } else {
            None
        }
    }

    /// `input` chaining (upstream `emitInput`): "handled" short-circuits,
    /// "transform" chains text/images, and the final state determines the
    /// returned action.
    pub fn emit_input(
        &self,
        text: &str,
        source: &str,
        streaming_behavior: Option<&str>,
    ) -> serde_json::Value {
        let mut current_text = text.to_string();
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("input") else {
                continue;
            };
            for handler in handlers {
                let event = serde_json::json!({
                    "type": "input",
                    "text": current_text,
                    "source": source,
                    "streamingBehavior": streaming_behavior,
                });
                match handler(&event) {
                    Ok(Some(result)) => {
                        let action = result.get("action").and_then(serde_json::Value::as_str);
                        if action == Some("handled") {
                            return result;
                        }
                        if action == Some("transform") {
                            if let Some(new_text) =
                                result.get("text").and_then(serde_json::Value::as_str)
                            {
                                current_text = new_text.to_string();
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "input".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        if current_text != text {
            serde_json::json!({ "action": "transform", "text": current_text })
        } else {
            serde_json::json!({ "action": "continue" })
        }
    }

    /// `resources_discover` collection (upstream `emitResourcesDiscover`).
    pub fn emit_resources_discover(&self, cwd: &str, reason: &str) -> DiscoveredResources {
        let mut discovered = DiscoveredResources::default();
        for ext in &self.extensions {
            let Some(handlers) = ext.handlers.get("resources_discover") else {
                continue;
            };
            for handler in handlers {
                let event = serde_json::json!({ "type": "resources_discover", "cwd": cwd, "reason": reason });
                match handler(&event) {
                    Ok(Some(result)) => {
                        for (field, target) in [
                            ("skillPaths", &mut discovered.skill_paths),
                            ("promptPaths", &mut discovered.prompt_paths),
                            ("themePaths", &mut discovered.theme_paths),
                        ] {
                            if let Some(paths) =
                                result.get(field).and_then(serde_json::Value::as_array)
                            {
                                target.extend(
                                    paths
                                        .iter()
                                        .filter_map(|p| p.as_str())
                                        .map(|p| (p.to_string(), ext.path.clone())),
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.emit_error(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: "resources_discover".to_string(),
                        error: message,
                        stack: None,
                    }),
                }
            }
        }
        discovered
    }
}

/// Resources discovered by extensions (upstream the
/// `emitResourcesDiscover` result).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredResources {
    pub skill_paths: Vec<(String, String)>,
    pub prompt_paths: Vec<(String, String)>,
    pub theme_paths: Vec<(String, String)>,
}

/// Emit a session_shutdown event if any handler exists (upstream
/// `emitSessionShutdownEvent`): returns whether it was emitted.
pub fn emit_session_shutdown_event(
    runner: &mut ExtensionRunner,
    reason: &str,
    target: Option<&str>,
) -> bool {
    if !runner.has_handlers("session_shutdown") {
        return false;
    }
    let event = serde_json::json!({
        "type": "session_shutdown",
        "reason": reason,
        "targetSessionFile": target,
    });
    let _ = runner.emit(&event);
    true
}

/// The project trust decision from one handler (upstream
/// `ProjectTrustEventResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrustDecision {
    Yes,
    No,
    Undecided,
}

/// Emit project_trust across a load result (upstream
/// `emitProjectTrustEvent`): the first handler returning yes/no wins;
/// undecided falls through; errors accumulate without stopping.
pub fn emit_project_trust_event(
    extensions: &[HostExtension],
    trusted: bool,
) -> (Option<ProjectTrustDecision>, Vec<ExtensionError>) {
    let mut errors = Vec::new();
    for ext in extensions {
        let Some(handlers) = ext.handlers.get("project_trust") else {
            continue;
        };
        for handler in handlers {
            let event = serde_json::json!({ "type": "project_trust", "trusted": trusted });
            match handler(&event) {
                Ok(Some(result)) => {
                    let decision = match result.get("trusted").and_then(serde_json::Value::as_str) {
                        Some("yes") => Some(ProjectTrustDecision::Yes),
                        Some("no") => Some(ProjectTrustDecision::No),
                        _ => None,
                    };
                    match decision {
                        Some(decision) => return (Some(decision), errors),
                        None => continue,
                    }
                }
                Ok(None) => {}
                Err(error) => errors.push(ExtensionError {
                    extension_path: ext.path.clone(),
                    event: "project_trust".to_string(),
                    error,
                    stack: None,
                }),
            }
        }
    }
    (None, errors)
}
