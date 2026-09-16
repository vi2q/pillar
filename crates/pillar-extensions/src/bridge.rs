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

use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError, ToolExecuteFn};
use pillar_ai::types::{Content, Tool};
use pillar_coding_agent::core::extensions_runner::{
    ExtensionEventPayload, ExtensionFlag, ExtensionHandler, ExtensionShortcut, HostExtension,
    RegisteredCommand,
};
use pillar_coding_agent::core::extensions_types::{
    EntryRenderOptions, EntryRenderer, MarkdownTransformContext, MarkdownTransformer,
    MessageRenderOptions, MessageRenderer,
};
use pillar_coding_agent::core::messages::{CustomContent, CustomMessage};
use pillar_coding_agent::core::session_entries::CustomEntry;
use pillar_coding_agent::modes::interactive::theme::Theme;

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
    // registration order; one runner handler per Luau handler, and each
    // runner handler invokes exactly that Luau handler — the runner owns the
    // chain, so dispatching the whole event here would run N handlers N times.
    // Only this extension's registrations: the runtime's registry is shared by
    // every extension, so an unfiltered bridge would re-claim the earlier
    // ones (duplicating commands and handlers)
    // (docs/ARCHITECTURE-REVIEW-s05c0.md B).
    for (event, identity) in registry
        .event_handlers
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
        let runtime = Arc::clone(runtime);
        let dispatch_event = event.clone();
        let handler_id = identity.clone();
        let handler: ExtensionHandler = Arc::new(move |payload: &ExtensionEventPayload| {
            let mut runtime = runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = runtime
                .dispatch_handler(&dispatch_event, &handler_id, payload.clone())
                .map_err(|error: ExtensionLoadError| error.to_string())?;
            match outcome {
                crate::runtime::HandlerOutcome::None => Ok(None),
                crate::runtime::HandlerOutcome::Block { reason } => {
                    // A handler that blocks must stop the action whichever key
                    // its consumer reads: `block` (tool_call) or `cancel`
                    // (session_before_*). The reason is optional, so the safe
                    // keys are set unconditionally.
                    let mut result = serde_json::json!({ "block": true, "cancel": true });
                    if let Some(reason) = reason {
                        result["reason"] = serde_json::Value::String(reason);
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
    for (name, opts) in registry
        .commands
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
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
    for (name, opts) in registry
        .flags
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
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
    for (key, opts) in registry
        .shortcuts
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
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

    // Custom renderers (upstream the extension's `messageRenderers` /
    // `entryRenderers` maps and its single `markdownTransformer`): the Lua
    // function answers a declarative component description
    // ([`declarative_lines`]) which the bridge converts with the active
    // theme into themed lines. The presentation adapter wraps them. A renderer error is reported to stderr and answers the upstream
    // failure notice so the transcript still shows something.
    let mut message_renderers = std::collections::BTreeMap::new();
    for custom_type in registry
        .message_renderers
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
        if message_renderers.contains_key(custom_type) {
            continue;
        }
        let runtime = Arc::clone(runtime);
        let key = custom_type.clone();
        let hook = key.clone();
        let renderer: MessageRenderer = Arc::new(
            move |message: &CustomMessage, options: &MessageRenderOptions, theme: &Theme| {
                let payload = custom_message_payload(message);
                let options = renderer_options(options.expanded, options.output_pad);
                // Lock only for the call: the renderer may call back into
                // the host API.
                let result = runtime
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .render_custom_message(&key, &payload, &options);
                match result {
                    Ok(value) => value.and_then(|value| declarative_lines(theme, &value)),
                    Err(error) => Some(renderer_error_lines(theme, &hook, &error)),
                }
            },
        );
        message_renderers.insert(custom_type.clone(), renderer);
    }

    let mut entry_renderers = std::collections::BTreeMap::new();
    for custom_type in registry
        .entry_renderers
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
    {
        if entry_renderers.contains_key(custom_type) {
            continue;
        }
        let runtime = Arc::clone(runtime);
        let key = custom_type.clone();
        let hook = key.clone();
        let renderer: EntryRenderer = Arc::new(
            move |entry: &CustomEntry, options: &EntryRenderOptions, theme: &Theme| {
                let payload = custom_entry_payload(entry);
                let options = serde_json::json!({ "expanded": options.expanded });
                let result = runtime
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .render_custom_entry(&key, &payload, &options);
                match result {
                    Ok(value) => value.and_then(|value| declarative_lines(theme, &value)),
                    Err(error) => Some(renderer_error_lines(theme, &hook, &error)),
                }
            },
        );
        entry_renderers.insert(custom_type.clone(), renderer);
    }

    let markdown_transformer = registry
        .markdown_transformers
        .iter()
        .filter(|registration| registration.owner == path)
        .map(|registration| &registration.value)
        .next_back()
        .map(|identity| {
            let runtime = Arc::clone(runtime);
            let identity = identity.clone();
            let transformer: MarkdownTransformer =
                Arc::new(move |markdown: &str, context: &MarkdownTransformContext| {
                    let context = serde_json::json!({
                        "messageType": context.message_type.as_str(),
                        "isStreaming": context.is_streaming,
                        "availableWidth": context.available_width,
                    });
                    let result = runtime
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .transform_markdown(&identity, markdown, &context);
                    match result {
                        Ok(value) => value,
                        Err(error) => {
                            eprintln!("pillar-extensions: markdown transformer failed: {error}");
                            None
                        }
                    }
                });
            transformer
        });

    Ok(HostExtension {
        path: path.to_string(),
        handlers,
        commands,
        tools: registry
            .tools
            .iter()
            .filter(|registration| registration.owner == path)
            .filter_map(|registration| {
                registration
                    .value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(|name| (name.to_string(), ()))
            })
            .collect(),
        flags,
        shortcuts,
        message_renderers,
        entry_renderers,
        markdown_transformer,
    })
}

/// Options handed to a custom-message renderer (upstream `MessageRenderOptions`).
fn renderer_options(expanded: bool, output_pad: usize) -> serde_json::Value {
    serde_json::json!({ "expanded": expanded, "outputPad": output_pad })
}

/// Drop absent (`null`) top-level keys: the Lua boundary turns JSON `null`
/// into an empty table, so an extension could not tell "no data" from "empty
/// data" (`if entry.data == nil`).
fn without_absent_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(entries) => serde_json::Value::Object(
            entries
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .collect(),
        ),
        other => other,
    }
}

/// The payload handed to a custom-message renderer (upstream `CustomMessage`).
fn custom_message_payload(message: &CustomMessage) -> serde_json::Value {
    without_absent_keys(serde_json::json!({
        "customType": message.custom_type,
        "content": message
            .content
            .iter()
            .map(|content| match content {
                CustomContent::Text(text) => serde_json::json!({ "type": "text", "text": text }),
                CustomContent::Image { data, mime_type } => {
                    serde_json::json!({ "type": "image", "data": data, "mimeType": mime_type })
                }
            })
            .collect::<Vec<_>>(),
        "display": message.display,
        "details": message.details,
        "timestamp": message.timestamp,
    }))
}

/// The payload handed to a custom-entry renderer (upstream `CustomEntry`).
fn custom_entry_payload(entry: &CustomEntry) -> serde_json::Value {
    without_absent_keys(serde_json::json!({
        "customType": entry.custom_type,
        "id": entry.base.id,
        "data": entry.data,
    }))
}

/// The lines the failure notice shows when a renderer throws (upstream the
/// notice `CustomEntryComponent` shows), styled with the theme's error colour.
fn renderer_error_lines(theme: &Theme, custom_type: &str, message: &str) -> Vec<String> {
    eprintln!("pillar-extensions: [{custom_type}] renderer failed: {message}");
    let text = format!("[{custom_type}] renderer failed: {message}");
    vec![theme.try_fg("error", &text).unwrap_or(text)]
}

/// Convert a Lua renderer's declarative result into themed lines (upstream the
/// renderer returns a live `Component`; the port returns the description and
/// the presentation adapter wraps it):
///
/// - `nil` → `None`: fall back to the default rendering (messages) or skip
///   the entry (entries).
/// - a string → one plain line.
/// - `{ text = ..., style = ... }` → one styled line.
/// - `{ lines = { line, ... } }` → one line per entry, where a line is a
///   string or a list of `{ text, style }` segments. An empty line list
///   answers `None` (nothing to show).
///
/// `style` is a theme foreground colour name (`text`, `dim`, `accent`,
/// `success`, `error`, …); an unknown name renders unstyled.
fn declarative_lines(theme: &Theme, value: &serde_json::Value) -> Option<Vec<String>> {
    let lines = match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Object(object) => {
            if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_array) {
                lines
                    .iter()
                    .map(|line| declarative_line(theme, line))
                    .collect()
            } else if object.contains_key("text") {
                vec![declarative_line(theme, value)]
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if lines.join("\n").trim().is_empty() {
        return None;
    }
    Some(lines)
}

/// One declarative line: a plain string or a list of styled segments.
fn declarative_line(theme: &Theme, line: &serde_json::Value) -> String {
    match line {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(_) => declarative_segment(theme, line),
        serde_json::Value::Array(segments) => segments
            .iter()
            .map(|segment| declarative_segment(theme, segment))
            .collect(),
        _ => String::new(),
    }
}

/// One styled segment (`{ text = ..., style = ... }`).
fn declarative_segment(theme: &Theme, segment: &serde_json::Value) -> String {
    let text = segment
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match segment.get("style").and_then(serde_json::Value::as_str) {
        Some(style) => theme
            .try_fg(style, text)
            .unwrap_or_else(|| text.to_string()),
        None => text.to_string(),
    }
}

/// Build the callable agent tools for every tool an extension registered
/// (upstream the runner adding the extension's tools to the session tool
/// set). The returned tools dispatch into the shared Luau runtime.
pub fn bridge_to_agent_tools(runtime: &Arc<Mutex<ExtensionRuntime>>) -> Vec<AgentTool> {
    let definitions = runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .registry()
        .tools;
    definitions
        .into_iter()
        .map(|registration| registration.value)
        .filter_map(|definition| {
            let name = definition
                .get("name")
                .and_then(serde_json::Value::as_str)?
                .to_string();
            if name.is_empty() {
                return None;
            }
            let description = definition
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let label = definition
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&name)
                .to_string();
            let parameters = definition
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));

            let runtime = Arc::clone(runtime);
            let execute_name = name.clone();
            let execute: Arc<ToolExecuteFn> = Arc::new(
                move |tool_call_id: String,
                      arguments: serde_json::Value,
                      signal: Option<pillar_agent::abort::AbortSignal>,
                      on_update: Option<pillar_agent::types::AgentToolUpdateCallback>| {
                    let runtime = Arc::clone(&runtime);
                    let name = execute_name.clone();
                    Box::pin(async move {
                        let result = runtime
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .call_tool(&name, &tool_call_id, arguments, signal, on_update);
                        match result {
                            Ok(json) => Ok(tool_result_from_json(json)),
                            Err(message) => Err(ToolExecuteError(message)),
                        }
                    })
                },
            );

            Some(AgentTool {
                tool: Tool {
                    name,
                    description,
                    parameters,
                    constrained_sampling: None,
                },
                label,
                prepare_arguments: None,
                execute,
                execution_mode: None,
            })
        })
        .collect()
}

/// Convert an extension tool's Lua value into the agent's result shape:
/// `content` blocks deserialize verbatim (they cross the LLM boundary),
/// everything else is passed through. A missing or malformed `content` becomes
/// a single text block with the raw JSON, so a broken tool never drops its
/// output silently. Shared by the bridge's execute and the runtime's
/// `on_update` callback.
pub fn tool_result_from_json(json: serde_json::Value) -> AgentToolResult {
    let content = json
        .get("content")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<Content>>(value).ok())
        .unwrap_or_else(|| {
            vec![Content::text(match json.get("content") {
                Some(value) => value.to_string(),
                None => json.to_string(),
            })]
        });
    AgentToolResult {
        content,
        details: json
            .get("details")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        usage: json
            .get("usage")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        added_tool_names: json
            .get("addedToolNames")
            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok()),
        terminate: json
            .get("terminate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
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

    /// A registered Luau tool becomes a callable agent tool: the execute
    /// closure dispatches into the VM and returns the content / details.
    #[tokio::test]
    async fn registered_tools_become_callable_agent_tools() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "tool.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_tool({
                        name = "greet",
                        label = "Greet",
                        description = "Greet someone",
                        parameters = pillar.schema.object({
                            name = pillar.schema.string(),
                        }),
                        execute = function(tool_call_id, params, signal, on_update, ctx)
                            return {
                                content = { { type = "text", text = "Hello, " .. params.name .. "!" } },
                                details = { call = tool_call_id },
                            }
                        end,
                    })
                    return nil
                "#,
                )
                .unwrap();
        }
        let tools = bridge_to_agent_tools(&runtime);
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.tool.name, "greet");
        assert_eq!(tool.label, "Greet");
        assert_eq!(tool.tool.description, "Greet someone");
        assert_eq!(
            tool.tool.parameters["properties"]["name"]["type"],
            serde_json::json!("string")
        );

        let result = (tool.execute)(
            "call-1".to_string(),
            serde_json::json!({ "name": "World" }),
            None,
            None,
        )
        .await
        .expect("tool executes");
        assert_eq!(result.content.len(), 1);
        assert_eq!(
            result.content[0],
            pillar_ai::types::Content::text("Hello, World!")
        );
        assert_eq!(result.details, serde_json::json!({ "call": "call-1" }));
    }

    /// A tool without a registered execute function cannot be called and
    /// still yields a readable error.
    #[tokio::test]
    async fn tools_without_execute_report_an_error() {
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "noop.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_tool({ name = "noop", description = "no execute" })
                    return nil
                "#,
                )
                .unwrap();
        }
        let tools = bridge_to_agent_tools(&runtime);
        assert_eq!(tools.len(), 1);
        let error = (tools[0].execute)("call-2".to_string(), serde_json::json!({}), None, None)
            .await
            .expect_err("no execute");
        assert!(error.0.contains("noop"), "{error:?}");
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
        assert_eq!(extension.shortcuts["ctrl+alt+k"].description, "Do a thing");
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

    fn install_dark_theme() {
        pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));
    }

    /// A Luau message renderer reaches the runner and its declarative
    /// description becomes a component through the active theme.
    #[test]
    fn message_renderer_bridges_into_the_runner() {
        install_dark_theme();
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "card.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_message_renderer("my-card", function(message, options)
                        return {
                            lines = {
                                { { text = "TITLE ", style = "accent" }, { text = message.details.title } },
                            },
                        }
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("card.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let renderer = runner.get_message_renderer("my-card").expect("registered");
        assert!(runner.get_message_renderer("other").is_none());

        let theme = pillar_coding_agent::modes::interactive::theme::theme();
        let message = CustomMessage {
            custom_type: "my-card".to_string(),
            content: vec![CustomContent::Text("body".to_string())],
            display: true,
            details: Some(serde_json::json!({ "title": "hello" })),
            timestamp: 0,
        };
        let options = MessageRenderOptions {
            expanded: true,
            output_pad: 0,
        };
        let rendered = renderer(&message, &options, &theme)
            .expect("rendered lines")
            .join("\n");
        // The accent colour wraps the styled segment and the unstyled one
        // follows it.
        assert!(
            rendered.contains(&theme.fg("accent", "TITLE ")),
            "{rendered:?}"
        );
        assert!(rendered.contains("hello"), "{rendered:?}");
        assert!(!rendered.contains("style"), "{rendered:?}");
    }

    /// A Luau entry renderer answers the entry's content; `nil` and an empty
    /// line list skip the entry.
    #[test]
    fn entry_renderer_bridges_into_the_runner() {
        install_dark_theme();
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "widget.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_entry_renderer("widget", function(entry, options)
                        if entry.data == nil then return nil end
                        return "widget " .. tostring(entry.data.value)
                    end)
                    pillar.register_entry_renderer("empty-widget", function() return { lines = {} } end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("widget.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let theme = pillar_coding_agent::modes::interactive::theme::theme();
        let options = EntryRenderOptions { expanded: false };

        let renderer = runner.get_entry_renderer("widget").expect("registered");
        let entry = CustomEntry {
            base: pillar_coding_agent::core::session_entries::SessionEntryBase {
                id: "e1".to_string(),
                ..Default::default()
            },
            custom_type: "widget".to_string(),
            data: Some(serde_json::json!({ "value": 3 })),
        };
        let rendered = renderer(&entry, &options, &theme)
            .expect("rendered lines")
            .join("\n");
        assert!(rendered.contains("widget 3"), "{rendered:?}");

        // No data → nil → fall back.
        let empty = CustomEntry {
            data: None,
            ..entry.clone()
        };
        assert!(renderer(&empty, &options, &theme).is_none());

        // An empty line list renders nothing, so the entry is skipped.
        let empty_renderer = runner
            .get_entry_renderer("empty-widget")
            .expect("registered");
        assert!(empty_renderer(&entry, &options, &theme).is_none());
    }

    /// A renderer that raises surfaces the upstream failure notice (the port
    /// has no error channel on a renderer, so the bridge answers a component).
    #[test]
    fn renderer_errors_become_the_failure_notice() {
        install_dark_theme();
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "broken.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_message_renderer("broken", function()
                        error("renderer blew up")
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("broken.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let theme = pillar_coding_agent::modes::interactive::theme::theme();
        let renderer = runner.get_message_renderer("broken").expect("registered");
        let message = CustomMessage {
            custom_type: "broken".to_string(),
            content: Vec::new(),
            display: true,
            details: None,
            timestamp: 0,
        };
        let rendered = renderer(
            &message,
            &MessageRenderOptions {
                expanded: false,
                output_pad: 0,
            },
            &theme,
        )
        .expect("failure notice")
        .join("\n");
        assert!(rendered.contains("renderer failed"), "{rendered:?}");
        assert!(rendered.contains("renderer blew up"), "{rendered:?}");
    }

    /// The markdown transformer reaches the runner and runs inside the VM.
    #[test]
    fn markdown_transformer_bridges_into_the_runner() {
        install_dark_theme();
        let runtime = shared_runtime();
        {
            let mut runtime = runtime.lock().unwrap();
            runtime
                .load_extension(
                    "transform.luau",
                    r#"
                    local pillar = require("@pillar")
                    pillar.register_markdown_transformer(function(markdown, context)
                        if context.messageType ~= "user" then return nil end
                        return "// " .. markdown
                    end)
                    return nil
                "#,
                )
                .unwrap();
        }
        let extension = bridge_to_runner("transform.luau", &runtime).unwrap();
        let runner = ExtensionRunner::new(vec![extension]);
        let transformers = runner.get_markdown_transformers();
        assert_eq!(transformers.len(), 1);
        let context = MarkdownTransformContext {
            message_type: pillar_coding_agent::core::extensions_types::MarkdownMessageType::User,
            is_streaming: false,
            available_width: 80,
        };
        assert_eq!(
            transformers[0]("hello", &context),
            Some("// hello".to_string())
        );
        let assistant = MarkdownTransformContext {
            message_type:
                pillar_coding_agent::core::extensions_types::MarkdownMessageType::Assistant,
            ..context.clone()
        };
        assert_eq!(transformers[0]("hello", &assistant), None);
    }
}
