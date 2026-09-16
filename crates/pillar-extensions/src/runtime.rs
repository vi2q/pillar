//! The extension runtime (pi v0.84.3 loader + ExtensionAPI): one
//! luaur `Lua` instance for the process, `@pillar` registered as a
//! module alias, each extension file loaded as a module returning a
//! setup function which the host calls with the API table.
//!
//! divergences: the `@pillar` module table is built host-side with
//! registration records captured into [`HostRegistry`]; type-checking
//! with luaur-analysis lands with the analysis integration; async
//! handler invocation is host-driven (the runtime records calls).

use std::sync::{Arc, Mutex};

use luaur_rt::{Function, Lua, LuaSerdeExt, TypeDiagnostic, Value, check_with_definitions};
use pillar_coding_agent::core::extensions_types::{
    ExtensionContextFn, ExtensionUiFn, ExtensionUiRequest,
};

/// Registration records captured from `pillar.*` API calls (upstream
/// the ExtensionAPI's internal registries).
#[derive(Debug, Clone, PartialEq)]
pub struct HostRegistry {
    /// `pillar.on(event, handler)` — handler identity is
    /// (extension, function reference index).
    pub event_handlers: Vec<(String, String)>,
    /// `pillar.register_tool(def)` — JSON-normalized definition.
    pub tools: Vec<serde_json::Value>,
    /// `pillar.register_command(name, opts)`.
    pub commands: Vec<(String, serde_json::Value)>,
    /// `pillar.register_shortcut(key, opts)`.
    pub shortcuts: Vec<(String, serde_json::Value)>,
    /// `pillar.register_flag(name, opts)`.
    pub flags: Vec<(String, serde_json::Value)>,
    /// `pillar.append_entry(type, data)`.
    pub appended_entries: Vec<(String, Option<serde_json::Value>)>,
    /// `pillar.send_message(msg)` / `send_user_message`.
    pub messages: Vec<serde_json::Value>,
    /// `pillar.set_session_name(name)`.
    pub session_names: Vec<String>,
    /// `pillar.register_message_renderer(custom_type, renderer)` — the
    /// custom types, in registration order (the renderer function itself
    /// stays in the VM, keyed by custom type).
    pub message_renderers: Vec<String>,
    /// `pillar.register_entry_renderer(custom_type, renderer)`.
    pub entry_renderers: Vec<String>,
    /// `pillar.register_markdown_transformer(transformer)` — the VM-side
    /// identities (`@0`, `@1`, …) in registration order.
    pub markdown_transformers: Vec<String>,
}

impl Default for HostRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostRegistry {
    pub fn shared() -> SharedRegistry {
        Arc::new(Mutex::new(Self::new()))
    }

    pub fn new() -> Self {
        Self {
            event_handlers: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            appended_entries: Vec::new(),
            messages: Vec::new(),
            session_names: Vec::new(),
            message_renderers: Vec::new(),
            entry_renderers: Vec::new(),
            markdown_transformers: Vec::new(),
        }
    }

    /// Handlers registered for an event, in registration order
    /// (upstream the runner's per-event handler list).
    pub fn handlers_for(&self, event: &str) -> Vec<&str> {
        self.event_handlers
            .iter()
            .filter(|(name, _)| name == event)
            .map(|(_, handler)| handler.as_str())
            .collect()
    }
}

/// A loaded extension: its source path and the setup function handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedExtension {
    pub path: String,
    /// Whether the module returned a callable setup function (pi
    /// extensions may export nothing, in which case they only
    /// side-effect on load).
    pub has_setup: bool,
}

/// Errors from loading an extension (upstream the per-file load
/// failure path: one file failing does not abort startup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionLoadError {
    /// Reading the file failed.
    Io(String),
    /// The Luau chunk failed to compile.
    Compile(String),
    /// The module did not return a function or nil (upstream rejects
    /// non-function exports).
    InvalidExport(String),
    /// The setup function itself errored.
    Setup(String),
}

impl std::fmt::Display for ExtensionLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "{message}"),
            Self::Compile(message) => write!(f, "{message}"),
            Self::InvalidExport(message) => write!(f, "{message}"),
            Self::Setup(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ExtensionLoadError {}

/// Shared registry handle (native closures capture `'static` data).
pub type SharedRegistry = Arc<Mutex<HostRegistry>>;

/// The `ctx` table every handler receives (upstream `ExtensionContext`):
/// the host facts, the `ui` bridge and the active theme. `ui_call` answers
/// `false` when the host has no UI context, which is when the wrappers are
/// no-ops (upstream `noOpUIContext`).
const CONTEXT_LUA: &str = r#"
local facts, ui_call, theme, themes, session_id, session_entries = ...

local ui = {}
local function call(op, args)
    ui_call(op, args or {})
end

ui.notify = function(message, kind) call("notify", { message = message, type = kind }) end
ui.set_status = function(key, text) call("set_status", { key = key, text = text }) end
ui.set_title = function(title) call("set_title", { title = title }) end
ui.set_working_message = function(message) call("set_working_message", { message = message }) end
ui.set_working_indicator = function(options) call("set_working_indicator", { options = options }) end
ui.set_working_visible = function(visible) call("set_working_visible", { visible = visible }) end
ui.set_hidden_thinking_label = function(label) call("set_hidden_thinking_label", { label = label }) end
ui.set_editor_text = function(text) call("set_editor_text", { text = text }) end
ui.paste_to_editor = function(text) call("paste_to_editor", { text = text }) end
ui.set_tools_expanded = function(expanded) call("set_tools_expanded", { expanded = expanded }) end
ui.get_all_themes = function() return themes end

local function styled(map, reset, color, text)
    local ansi = map[color]
    if ansi == nil then return text end
    return ansi .. text .. reset
end
theme.fg = function(color, text) return styled(theme.fgColors, "\27[39m", color, text) end
theme.bg = function(color, text) return styled(theme.bgColors, "\27[49m", color, text) end
theme.bold = function(text) return "\27[1m" .. text .. "\27[22m" end
theme.italic = function(text) return "\27[3m" .. text .. "\27[23m" end
theme.underline = function(text) return "\27[4m" .. text .. "\27[24m" end
theme.strikethrough = function(text) return "\27[9m" .. text .. "\27[29m" end
theme.inverse = function(text) return "\27[7m" .. text .. "\27[27m" end

local sessionManager = {
    getSessionId = function() return session_id() end,
    getEntries = function() return session_entries() end,
}

return {
    cwd = facts.cwd,
    mode = facts.mode,
    hasUI = facts.hasUI,
    ui = ui,
    theme = theme,
    sessionManager = sessionManager,
}
"#;

/// Host `declare` definitions for the type-checker: the `@pillar`
/// module surface (grows with the API; currently the registration
/// methods installed by [`install_pillar_api`]).
pub const PILLAR_DEFINITIONS: &str = r#"
declare pillar: {
    on: (event: string, handler: (event: any, ctx: any) -> any) -> (),
    register_tool: (definition: any) -> (),
    register_command: (name: string, opts: any) -> (),
    register_shortcut: (key: string, opts: any) -> (),
    register_flag: (name: string, opts: any) -> (),
    register_message_renderer: (custom_type: string, renderer: (message: any, options: any) -> any) -> (),
    register_entry_renderer: (custom_type: string, renderer: (entry: any, options: any) -> any) -> (),
    register_markdown_transformer: (transformer: (markdown: string, context: any) -> string?) -> (),
    get_flag: (name: string) -> any,
    append_entry: (kind: string, data: any?) -> (),
    send_message: (message: any) -> (),
    send_user_message: (message: any) -> (),
    set_session_name: (name: string) -> (),
    get_session_name: () -> string?,
    set_label: (entry_id: string, label: string?) -> (),
    get_commands: () -> { any },
    get_active_tools: () -> { string },
    get_all_tools: () -> { any },
    get_thinking_level: () -> string?,
    set_thinking_level: (level: string) -> (),
    set_model: (model: any) -> (),
    set_active_tools: (names: { string }) -> (),
    exec: (command: string, args: { number }?, opts: any?) -> any,
    fs: {
        read: (path: string) -> string?,
        write: (path: string, content: string) -> boolean,
        list: (path: string) -> { string },
        stat: (path: string) -> any,
        exists: (path: string) -> boolean,
    },
    events: {
        on: (channel: string, handler: (data: any) -> ()) -> (() -> ()),
        emit: (channel: string, data: any?) -> (),
    },
    schema: {
        string: (opts: any?) -> any,
        number: (opts: any?) -> any,
        boolean: (opts: any?) -> any,
        enum: (values: { any }) -> any,
        array: (item: any) -> any,
        object: (properties: any, opts: any?) -> any,
    },
}
"#;

/// Host command executor (upstream `pi.exec` backed by the process
/// layer): the host injects the real executor; the extension receives
/// `{ stdout, stderr, code, killed }`.
pub type ExecHost = Arc<dyn Fn(&str, &[String]) -> serde_json::Value + Send + Sync>;

/// Host callback for `pillar.get_flag` (upstream `getFlag`).
pub type GetFlagFn = Arc<dyn Fn(&str) -> Option<serde_json::Value> + Send + Sync>;
/// Host callback returning a string, used by `get_session_name` /
/// `get_thinking_level`.
pub type GetStringFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;
/// Host callback returning a JSON array, used by `get_commands` /
/// `get_active_tools` / `get_all_tools`.
pub type GetJsonFn = Arc<dyn Fn() -> serde_json::Value + Send + Sync>;
/// Host callback for `pillar.append_entry(custom_type, data?)`.
pub type AppendEntryFn =
    Arc<dyn Fn(&str, Option<serde_json::Value>) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.send_message` / `send_user_message`.
pub type SendMessageFn = Arc<dyn Fn(serde_json::Value) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.set_session_name`.
pub type SetSessionNameFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.set_label(entry_id, label?)`.
pub type SetLabelFn = Arc<dyn Fn(&str, Option<&str>) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.set_thinking_level(level)`.
pub type SetThinkingLevelFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.set_model(model)`.
pub type SetModelFn = Arc<dyn Fn(serde_json::Value) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.set_active_tools(names)`.
pub type SetActiveToolsFn = Arc<dyn Fn(serde_json::Value) -> Result<(), String> + Send + Sync>;
/// Host callback for `pillar.fs.*` (upstream `pillar.fs`): the operation
/// name (`read` / `write` / `list` / `stat` / `exists`), the path, and the
/// content for `write`.
pub type FsFn =
    Arc<dyn Fn(&str, &str, Option<&str>) -> Result<serde_json::Value, String> + Send + Sync>;

/// The host callbacks the `@pillar` API reads (upstream the pieces of
/// the runtime the ExtensionAPI reaches). Each is optional: without a
/// callback the method falls back to recording in the registry (so the
/// host can apply it later) or answers the documented default.
#[derive(Clone, Default)]
pub struct HostApi {
    /// `pillar.get_flag(name)` → the parsed CLI flag value (upstream
    /// `getFlag`).
    pub get_flag: Option<GetFlagFn>,
    /// `pillar.append_entry(custom_type, data?)` → the live session.
    pub append_entry: Option<AppendEntryFn>,
    /// `pillar.send_message(message)` → the live session.
    pub send_message: Option<SendMessageFn>,
    /// `pillar.send_user_message(content)` → the live session.
    pub send_user_message: Option<SendMessageFn>,
    /// `pillar.set_session_name(name)` → the live session.
    pub set_session_name: Option<SetSessionNameFn>,
    /// `pillar.get_session_name()` → the live session's name.
    pub get_session_name: Option<GetStringFn>,
    /// `pillar.set_label(entry_id, label?)` → the session manager.
    pub set_label: Option<SetLabelFn>,
    /// `pillar.get_commands()` → the session's slash commands.
    pub get_commands: Option<GetJsonFn>,
    /// `pillar.get_active_tools()` → the active tool names.
    pub get_active_tools: Option<GetJsonFn>,
    /// `pillar.get_all_tools()` → every configured tool's info.
    pub get_all_tools: Option<GetJsonFn>,
    /// `pillar.get_thinking_level()`.
    pub get_thinking_level: Option<GetStringFn>,
    /// `pillar.set_thinking_level(level)`.
    pub set_thinking_level: Option<SetThinkingLevelFn>,
    /// `pillar.set_model(model)` → the live session.
    pub set_model: Option<SetModelFn>,
    /// `pillar.set_active_tools(names)` → the live session.
    pub set_active_tools: Option<SetActiveToolsFn>,
    /// `pillar.fs.*` → the bounded file API (the host resolves paths and
    /// applies the same trust model as tool calls).
    pub fs: Option<FsFn>,
    /// `ctx.ui.*` → the interactive mode's UI (upstream
    /// `ExtensionUIContext`). Without it every method is a no-op, matching
    /// upstream's `noOpUIContext`.
    pub ui: Option<ExtensionUiFn>,
    /// The `ctx` facts (`cwd` / `mode` / `hasUI`; upstream the live
    /// `ExtensionContext` fields).
    pub context: Option<ExtensionContextFn>,
    /// `ctx.sessionManager.getSessionId()` (upstream the session id).
    pub session_id: Option<GetStringFn>,
    /// `ctx.sessionManager.getEntries()` — the session entries as JSON
    /// (upstream the read-only session manager).
    pub session_entries: Option<GetJsonFn>,
}

/// The process-wide extension runtime.
pub struct ExtensionRuntime {
    lua: Lua,
    registry: SharedRegistry,
    /// Host exec callback (None until the host installs one; calls
    /// before installation return the not-installed failure result).
    exec_host: Arc<Mutex<Option<ExecHost>>>,
    /// Host callbacks for the read-only API methods (`get_flag`, …).
    host_api: Arc<Mutex<HostApi>>,
}

/// Result of dispatching one event to a handler (upstream the
/// handler return value semantics).
#[derive(Debug, Clone, PartialEq)]
pub enum HandlerOutcome {
    /// Handler returned nothing (or nil).
    None,
    /// Handler returned `{ block = true, reason = "..." }`.
    Block { reason: Option<String> },
    /// Handler returned a non-block table (modifications feed the
    /// next handler).
    Table(serde_json::Value),
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    /// Create the runtime and register the `@pillar` module (upstream
    /// the ExtensionAPI construction).
    pub fn new() -> Self {
        let lua = Lua::new();
        let registry = HostRegistry::shared();
        let exec_host = Arc::new(Mutex::new(None));
        let host_api = Arc::new(Mutex::new(HostApi::default()));
        install_pillar_api(&lua, &registry, &exec_host, &host_api);
        Self {
            lua,
            registry,
            exec_host,
            host_api,
        }
    }

    /// Install the host callbacks the read-only API reads (upstream the
    /// runtime wiring the CLI flags into the ExtensionAPI).
    pub fn set_host_api(&self, api: HostApi) {
        *self
            .host_api
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = api;
    }

    /// Install the host exec callback (upstream the runtime wiring the
    /// process layer into the ExtensionAPI).
    pub fn set_exec_host(&self, exec_host: ExecHost) {
        *self
            .exec_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(exec_host);
    }

    /// A snapshot of the registration records (upstream reading the
    /// runner's registries after setup runs).
    pub fn registry(&self) -> HostRegistry {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Dispatch an event to handlers registered for it, in
    /// registration order (upstream the runner's emit loop): a
    /// `block = true` return stops the chain.
    pub fn dispatch(
        &mut self,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<HandlerOutcome, ExtensionLoadError> {
        let handlers: Vec<Value> = self
            .lua
            .load(
                r#"
                local event = ...
                local list = __pillar_handlers and __pillar_handlers[event] or {}
                return list
            "#,
            )
            .call((event,))
            .unwrap_or_default();
        let mut last_table: Option<HandlerOutcome> = None;
        // Upstream `handler(event, ctx)`: one context per dispatch.
        let context = self.context_value()?;
        for handler in handlers {
            let arg = self
                .lua
                .to_value(&payload)
                .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;
            let function = match handler {
                Value::Function(function) => function,
                _ => continue,
            };
            let result: Value = function
                .call((arg, context.clone()))
                .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;
            let outcome = match &result {
                Value::Nil => {
                    #[cfg(test)]
                    if std::env::var("DISPATCH_DEBUG").is_ok() {
                        eprintln!("DBG dispatch: nil result from handler");
                    }
                    HandlerOutcome::None
                }
                other => {
                    let json = self
                        .lua
                        .from_value::<serde_json::Value>(other.clone())
                        .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;
                    if json.get("block").and_then(serde_json::Value::as_bool) == Some(true) {
                        HandlerOutcome::Block {
                            reason: json
                                .get("reason")
                                .and_then(serde_json::Value::as_str)
                                .map(|reason| reason.to_string()),
                        }
                    } else {
                        HandlerOutcome::Table(json)
                    }
                }
            };
            if matches!(outcome, HandlerOutcome::Block { .. }) {
                return Ok(outcome);
            }
            if matches!(outcome, HandlerOutcome::Table(_)) {
                last_table = Some(outcome);
            }
        }
        Ok(last_table.unwrap_or(HandlerOutcome::None))
    }

    /// Call a registered Luau tool's `execute` (upstream the tool's execute
    /// closure): `execute(tool_call_id, params, signal, on_update, ctx)`
    /// returns `{ content, details?, usage?, addedToolNames?, terminate? }`.
    ///
    /// divergences: `signal` / `on_update` are passed as nil until the async
    /// slice lands.
    pub fn call_tool(
        &mut self,
        name: &str,
        tool_call_id: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let params_lua = self
            .lua
            .to_value(&params)
            .map_err(|error| format!("{name}: {error}"))?;
        // The `signal` / `on_update` slots stay nil until the async slice;
        // `ctx` is the same table handlers receive (upstream the tool's
        // `ctx` argument).
        let context = self
            .context_value()
            .map_err(|error| format!("{name}: {error}"))?;
        let result: Value = self
            .lua
            .load(
                r#"
                local name, tool_call_id, params, ctx = ...
                local registered = __pillar_tool_execute
                local execute = registered and registered[name]
                if execute == nil then return nil end
                return execute(tool_call_id, params, nil, nil, ctx)
            "#,
            )
            .call((name, tool_call_id, params_lua, context))
            .map_err(|error| format!("{name}: {error}"))?;
        match result {
            Value::Nil => Err(format!("tool {name} returned no result")),
            other => self
                .lua
                .from_value::<serde_json::Value>(other)
                .map_err(|error| format!("{name}: {error}")),
        }
    }

    /// Call a registered custom-message renderer (upstream `MessageRenderer`):
    /// `renderer(message, { expanded, outputPad })` answers the declarative
    /// component description (see [`crate::bridge`]) or `nil` to fall back.
    pub fn render_custom_message(
        &mut self,
        custom_type: &str,
        message: &serde_json::Value,
        options: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        self.call_declarative_renderer("__pillar_message_renderers", custom_type, message, options)
    }

    /// Call a registered custom-entry renderer (upstream `EntryRenderer`):
    /// `renderer(entry, { expanded })`.
    pub fn render_custom_entry(
        &mut self,
        custom_type: &str,
        entry: &serde_json::Value,
        options: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        self.call_declarative_renderer("__pillar_entry_renderers", custom_type, entry, options)
    }

    fn call_declarative_renderer(
        &mut self,
        table: &str,
        custom_type: &str,
        payload: &serde_json::Value,
        options: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        let payload_lua = self
            .lua
            .to_value(payload)
            .map_err(|error| format!("{custom_type}: {error}"))?;
        let options_lua = self
            .lua
            .to_value(options)
            .map_err(|error| format!("{custom_type}: {error}"))?;
        let result: Value = self
            .lua
            .load(
                r#"
                local table_name, custom_type, payload, options = ...
                local registered = _G[table_name]
                local renderer = registered and registered[custom_type]
                if renderer == nil then return nil end
                return renderer(payload, options)
            "#,
            )
            .call((table, custom_type, payload_lua, options_lua))
            .map_err(|error| format!("{custom_type}: {error}"))?;
        match result {
            Value::Nil => Ok(None),
            other => self
                .lua
                .from_value::<serde_json::Value>(other)
                .map(Some)
                .map_err(|error| format!("{custom_type}: {error}")),
        }
    }

    /// Run a registered Markdown transformer (upstream `MarkdownTransformer`):
    /// `transformer(markdown, { messageType, isStreaming, availableWidth })`
    /// answers the rewritten Markdown, or `nil` to keep it.
    pub fn transform_markdown(
        &mut self,
        identity: &str,
        markdown: &str,
        context: &serde_json::Value,
    ) -> Result<Option<String>, String> {
        let context_lua = self
            .lua
            .to_value(context)
            .map_err(|error| format!("{identity}: {error}"))?;
        let result: Value = self
            .lua
            .load(
                r#"
                local identity, markdown, context = ...
                local registered = __pillar_markdown_transformers
                local index = tonumber(string.sub(identity, 2)) + 1
                local transformer = registered and registered[index]
                if transformer == nil then return nil end
                return transformer(markdown, context)
            "#,
            )
            .call((identity, markdown, context_lua))
            .map_err(|error| format!("{identity}: {error}"))?;
        match result {
            Value::Nil => Ok(None),
            Value::String(text) => Ok(Some(text.to_string_lossy().to_string())),
            other => Err(format!(
                "{identity}: expected a string, got {}",
                other.type_name()
            )),
        }
    }

    /// The `ctx` table handed to every handler and tool `execute` (upstream
    /// `ExtensionContext`): the host facts (`cwd` / `mode` / `hasUI`), the
    /// `ui` bridge (a no-op without a host UI context) and the active theme.
    pub fn context_value(&self) -> Result<Value, ExtensionLoadError> {
        let facts = {
            let guard = self
                .host_api
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.context.as_ref().map(|context| context())
        };
        let facts = facts.unwrap_or_default();
        let facts_lua = self
            .lua
            .to_value(&serde_json::json!({
                "cwd": facts.cwd,
                "mode": facts.mode.as_str(),
                "hasUI": facts.has_ui,
            }))
            .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;

        // `ctx.ui.<op>(args)` → the host bridge; without one the Lua wrappers
        // answer the upstream `noOpUIContext` defaults.
        let ui_slot = Arc::clone(&self.host_api);
        let ui_lua = self.lua.clone();
        let ui_call = Function::wrap(move |op: String, args: Option<Value>| {
            let callback = {
                let guard = ui_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.ui.clone()
            };
            let Some(callback) = callback else {
                return Ok::<bool, luaur_rt::Error>(false);
            };
            let args = match args {
                Some(args) => ui_lua
                    .from_value::<serde_json::Value>(args)
                    .map_err(luaur_rt::Error::external)?,
                None => serde_json::Value::Null,
            };
            callback(ExtensionUiRequest { op, args }).map_err(luaur_rt::Error::external)?;
            Ok(true)
        });
        // `ctx.sessionManager` (upstream the read-only session manager): only
        // the readers extensions use so far.
        let session_readers = Arc::clone(&self.host_api);
        let session_id = Function::wrap(move || {
            let callback = {
                let guard = session_readers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.session_id.clone()
            };
            Ok::<Option<String>, luaur_rt::Error>(callback.and_then(|callback| callback()))
        });
        let entries_readers = Arc::clone(&self.host_api);
        let entries_lua = self.lua.clone();
        let session_entries = Function::wrap(move || {
            let callback = {
                let guard = entries_readers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.session_entries.clone()
            };
            let value = match callback {
                Some(callback) => callback(),
                None => serde_json::Value::Array(Vec::new()),
            };
            entries_lua
                .to_value(&value)
                .map_err(luaur_rt::Error::external)
        });
        let (theme, themes) = self.theme_table()?;
        self.lua
            .load(CONTEXT_LUA)
            .call::<Value>((facts_lua, ui_call, theme, themes, session_id, session_entries))
            .map_err(|error| ExtensionLoadError::Setup(error.to_string()))
    }

    /// The `ctx.ui.theme` table (upstream the live `Theme` object, plus
    /// `getAllThemes`): the colour maps and the theme list. Without an
    /// initialized theme the styling helpers answer plain text.
    fn theme_table(&self) -> Result<(Value, Value), ExtensionLoadError> {
        use pillar_coding_agent::modes::interactive::theme;
        let active = theme::try_theme();
        let (name, mode, fg, bg) = match &active {
            Some(theme) => (
                theme.name().map(str::to_string),
                theme.color_mode().as_str().to_string(),
                theme.fg_colors().clone(),
                theme.bg_colors().clone(),
            ),
            None => (
                None,
                String::new(),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
            ),
        };
        let value = serde_json::json!({
            "name": name,
            "mode": mode,
            "fgColors": fg,
            "bgColors": bg,
        });
        let value = self
            .lua
            .to_value(&value)
            .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;
        let themes = serde_json::Value::Array(
            theme::all_themes()
                .into_iter()
                .map(|(name, path)| serde_json::json!({ "name": name, "path": path }))
                .collect(),
        );
        let themes = self
            .lua
            .to_value(&themes)
            .map_err(|error| ExtensionLoadError::Setup(error.to_string()))?;
        Ok((value, themes))
    }

    /// VM access for the host API installation and tests.
    pub fn vm(&self) -> &Lua {
        &self.lua
    }

    /// Type-check one extension source (upstream the loader's
    /// pre-run type-check: failing files are skipped with a warning
    /// listing the diagnostics, they do not abort startup). Returns
    /// the diagnostics (host-definition diagnostics filtered out) on
    /// failure.
    pub fn type_check(&self, path: &str, source: &str) -> Result<(), Vec<TypeDiagnostic>> {
        match check_with_definitions(source, PILLAR_DEFINITIONS) {
            Ok(()) => Ok(()),
            Err(diagnostics) => {
                let diagnostics: Vec<TypeDiagnostic> = diagnostics
                    .into_iter()
                    .filter(|diagnostic| !diagnostic.in_definitions)
                    .collect();
                eprintln!(
                    "pillar-extensions: skipping {path} (type-check failed: {} diagnostics)",
                    diagnostics.len()
                );
                Err(diagnostics)
            }
        }
    }

    /// Load one extension file: compile, call the module chunk, and
    /// verify the export shape (upstream the loader's per-file path).
    pub fn load_extension(
        &mut self,
        path: &str,
        source: &str,
    ) -> Result<LoadedExtension, ExtensionLoadError> {
        // Compile + call the module chunk; both surface as Compile
        // (upstream one per-file load failure).
        let export: luaur_rt::Value = match self.lua.load(source).call(()) {
            Ok(export) => export,
            Err(error) => return Err(ExtensionLoadError::Compile(format!("{path}: {error}"))),
        };
        // Upstream `loadExtension`: a file whose default export is not a
        // function is imported for its side effects and then skipped as an
        // extension (that is also how shared helper modules work, see
        // `require("@ext/<name>")` below).
        let has_setup = matches!(&export, luaur_rt::Value::Function(_));
        // Expose the module to the other extensions under `@ext/<name>`
        // (docs/rules/04: `require("@ext/<name>")` replaces relative TS
        // imports). A directory extension (`index.luau`) is named after its
        // directory. A nil export has nothing to share.
        if !matches!(export, luaur_rt::Value::Nil)
            && let Some(name) = extension_module_name(path)
        {
            let _ = self.lua.register_module(&format!("@ext/{name}"), export);
        }
        Ok(LoadedExtension {
            path: path.to_string(),
            has_setup,
        })
    }

    /// Call a loaded extension's setup function with the `@pillar`
    /// table (upstream the factory invocation). Extensions without a
    /// setup function are skipped. The API commit/discard contract is
    /// the caller's: registration records land in the shared registry
    /// as the setup body runs.
    pub fn run_setup(&mut self, extension: &LoadedExtension) -> Result<(), ExtensionLoadError> {
        if !extension.has_setup {
            return Ok(());
        }
        // Re-run the module chunk so its setup factory is re-created
        // and invoke it with the @pillar table (upstream retains the
        // export; the port's per-file VM re-evaluation is equivalent
        // because each file is independent).
        let setup: Value = self
            .lua
            .load(
                &std::fs::read_to_string(&extension.path).map_err(|error| {
                    ExtensionLoadError::Io(format!("{}: {error}", extension.path))
                })?,
            )
            .call(())
            .map_err(|error| ExtensionLoadError::Setup(format!("{}: {error}", extension.path)))?;
        let function = match setup {
            Value::Function(function) => function,
            _ => return Ok(()),
        };
        function
            .call(())
            .map_err(|error| ExtensionLoadError::Setup(format!("{}: {error}", extension.path)))
    }

    /// The full load flow (upstream `loadExtensions`): type-check,
    /// load, and run setup for each discovered path. Per-file failures
    /// land in `errors` and the remaining files continue (one failing
    /// file never aborts startup).
    pub fn load_and_run(
        &mut self,
        discovered: &[crate::discovery::DiscoveredExtension],
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        for entry in discovered {
            let path = entry.path.to_string_lossy().to_string();
            let source = match std::fs::read_to_string(&entry.path) {
                Ok(source) => source,
                Err(error) => {
                    errors.push((path.clone(), format!("Failed to load extension: {error}")));
                    continue;
                }
            };
            // The type-check runs first; a failing file is skipped
            // with its diagnostics (upstream the same skip contract,
            // surfaced through the load error list).
            if let Err(diagnostics) = self.type_check(&path, &source) {
                let summary = diagnostics
                    .iter()
                    .map(|diagnostic| {
                        format!(
                            "{}:{}: {}",
                            diagnostic.line, diagnostic.column, diagnostic.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                errors.push((path.clone(), format!("type-check failed: {summary}")));
                continue;
            }
            match self.load_extension(&path, &source) {
                Ok(extension) => match self.run_setup(&extension) {
                    Ok(()) => loaded.push(extension.path),
                    Err(error) => {
                        errors.push((path.clone(), format!("Failed to load extension: {error}")))
                    }
                },
                Err(error) => {
                    errors.push((path.clone(), format!("Failed to load extension: {error}")))
                }
            }
        }
        (loaded, errors)
    }
}

/// The `@ext/<name>` module name for an extension path: the file stem, or
/// the directory name for `index.luau` packages.
fn extension_module_name(path: &str) -> Option<String> {
    let path = std::path::Path::new(path);
    let stem = path.file_stem()?.to_string_lossy().to_string();
    if stem == "index" {
        return path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().to_string());
    }
    Some(stem)
}

/// Install the `@pillar` module and its registration functions
/// (upstream the ExtensionAPI methods; each records into the shared
/// registry).
fn install_pillar_api(
    lua: &Lua,
    registry: &SharedRegistry,
    exec_host: &Arc<Mutex<Option<ExecHost>>>,
    host_api: &Arc<Mutex<HostApi>>,
) {
    let module = lua.create_table();

    // pillar.exec(command, args?, opts?): returns
    // { stdout, stderr, code, killed } (upstream pi.exec). The host
    // callback performs the execution; the timeout/signal options are
    // host-side (the port passes only command and args across).
    let exec_error_lua = lua.clone();
    let exec_slot = Arc::clone(exec_host);
    module
        .set(
            "exec",
            Function::wrap(move |command: String, args: Option<Vec<String>>| {
                let args = args.unwrap_or_default();
                let guard = exec_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = match guard.as_ref() {
                    Some(exec) => exec(&command, &args),
                    None => serde_json::json!({
                        "stdout": "", "stderr": "exec host not installed",
                        "code": -1, "killed": false,
                    }),
                };
                drop(guard);
                exec_error_lua
                    .to_value(&result)
                    .map_err(luaur_rt::Error::external)
            }),
        )
        .expect("set pillar.exec");

    // pillar.on(event, handler): handlers live inside the VM in a
    // host-managed table (luaur Values stay on the VM side).
    let store_lua = lua.clone();
    let registry_sink = Arc::clone(registry);
    module
        .set(
            "on",
            Function::wrap(move |event: String, handler: Value| {
                let name = match &handler {
                    Value::Function(function) => format!("{:p}", function.to_pointer()),
                    other => format!("<{}>", other.type_name()),
                };
                let store = store_lua
                    .load(
                        r#"
                        local event, handler = ...
                        __pillar_handlers = __pillar_handlers or {}
                        __pillar_handlers[event] = __pillar_handlers[event] or {}
                        table.insert(__pillar_handlers[event], handler)
                        return true
                    "#,
                    )
                    .call::<bool>((event.as_str(), handler))
                    .is_ok();
                if store {
                    registry_sink
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .event_handlers
                        .push((event, name));
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.on");

    // pillar.register_tool(def): the definition table converts to JSON at
    // the boundary (payload keys snake_cased mechanically) and its `execute`
    // function stays in the VM, keyed by tool name, so the host can invoke it
    // (upstream the registered tool's execute closure).
    let tools = Arc::clone(registry);
    let lua_tools = lua.clone();
    module
        .set(
            "register_tool",
            Function::wrap(move |definition: Value| {
                // The VM-side `execute` function cannot cross the JSON
                // boundary; keep it keyed by tool name and register the rest
                // of the definition (upstream the registered tool's closure
                // lives outside the wire definition).
                let stripped: Value = lua_tools
                    .load(
                        r#"
                        local definition = ...
                        if type(definition) == "table" and definition.name ~= nil then
                            __pillar_tool_execute = __pillar_tool_execute or {}
                            __pillar_tool_execute[definition.name] = definition.execute
                        end
                        local meta = {}
                        if type(definition) == "table" then
                            for key, value in pairs(definition) do
                                if key ~= "execute" then
                                    meta[key] = value
                                end
                            end
                        end
                        return meta
                    "#,
                    )
                    .call((definition,))?;
                let json = lua_tools.from_value::<serde_json::Value>(stripped)?;
                tools
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .tools
                    .push(json);
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_tool");

    // pillar.register_command(name, opts)
    let commands = Arc::clone(registry);
    let lua_commands = lua.clone();
    module
        .set(
            "register_command",
            Function::wrap(move |name: String, opts: Value| {
                let json = lua_commands.from_value::<serde_json::Value>(opts)?;
                commands
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .commands
                    .push((name, json));
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_command");

    // pillar.register_shortcut(key, opts)
    let shortcuts = Arc::clone(registry);
    let lua_shortcuts = lua.clone();
    module
        .set(
            "register_shortcut",
            Function::wrap(move |key: String, opts: Value| {
                let json = lua_shortcuts.from_value::<serde_json::Value>(opts)?;
                shortcuts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .shortcuts
                    .push((key, json));
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_shortcut");

    // pillar.register_flag(name, opts)
    let flags = Arc::clone(registry);
    let lua_flags = lua.clone();
    module
        .set(
            "register_flag",
            Function::wrap(move |name: String, opts: Value| {
                let json = lua_flags.from_value::<serde_json::Value>(opts)?;
                flags
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .flags
                    .push((name, json));
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_flag");

    // pillar.append_entry(type, data?): the host persists it on the live
    // session; the registry keeps a record so a host that applies later (or
    // none at all) still sees the call.
    let entries = Arc::clone(registry);
    let entries_api = Arc::clone(host_api);
    let lua_entries = lua.clone();
    module
        .set(
            "append_entry",
            Function::wrap(move |kind: String, data: Value| {
                let json = match data {
                    Value::Nil => None,
                    other => Some(lua_entries.from_value::<serde_json::Value>(other)?),
                };
                let callback = {
                    let guard = entries_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.append_entry.clone()
                };
                if let Some(callback) = callback {
                    callback(&kind, json.clone()).map_err(luaur_rt::Error::external)?;
                }
                entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .appended_entries
                    .push((kind, json));
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.append_entry");

    // pillar.send_message(msg) / pillar.send_user_message(msg): the host
    // delivers to the live session; the registry keeps a record.
    for name in ["send_message", "send_user_message"] {
        let sink = Arc::clone(registry);
        let sink_api = Arc::clone(host_api);
        let sink_lua = lua.clone();
        let is_user_message = name == "send_user_message";
        module
            .set(
                name,
                Function::wrap(move |message: Value| {
                    let json = sink_lua.from_value::<serde_json::Value>(message)?;
                    let callback = {
                        let guard = sink_api
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if is_user_message {
                            guard.send_user_message.clone()
                        } else {
                            guard.send_message.clone()
                        }
                    };
                    if let Some(callback) = callback {
                        callback(json.clone()).map_err(luaur_rt::Error::external)?;
                    }
                    sink.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .messages
                        .push(json);
                    Ok::<(), luaur_rt::Error>(())
                }),
            )
            .expect("set pillar send");
    }

    // pillar.set_session_name(name): the host sets it on the live session;
    // the registry keeps a record.
    let names = Arc::clone(registry);
    let names_api = Arc::clone(host_api);
    module
        .set(
            "set_session_name",
            Function::wrap(move |name: String| {
                let callback = {
                    let guard = names_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.set_session_name.clone()
                };
                if let Some(callback) = callback {
                    callback(&name).map_err(luaur_rt::Error::external)?;
                }
                names
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .session_names
                    .push(name);
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_session_name");

    // pillar.get_session_name(): the live session's name (upstream
    // `getSessionName`), nil when unset.
    let get_name_api = Arc::clone(host_api);
    module
        .set(
            "get_session_name",
            Function::wrap(move || {
                let callback = {
                    let guard = get_name_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.get_session_name.clone()
                };
                Ok::<Option<String>, luaur_rt::Error>(callback.and_then(|callback| callback()))
            }),
        )
        .expect("set pillar.get_session_name");

    // pillar.set_label(entry_id, label?)
    let label_api = Arc::clone(host_api);
    let lua_label = lua.clone();
    module
        .set(
            "set_label",
            Function::wrap(move |entry_id: String, label: Value| {
                let label = match label {
                    Value::Nil => None,
                    other => Some(lua_label.from_value::<String>(other)?),
                };
                let callback = {
                    let guard = label_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.set_label.clone()
                };
                if let Some(callback) = callback {
                    callback(&entry_id, label.as_deref()).map_err(luaur_rt::Error::external)?;
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_label");

    // The JSON-array getters: `get_commands` / `get_active_tools` /
    // `get_all_tools`. Without a host callback they answer an empty array.
    for name in ["get_commands", "get_active_tools", "get_all_tools"] {
        let getter_api = Arc::clone(host_api);
        let getter_lua = lua.clone();
        module
            .set(
                name,
                Function::wrap(move || {
                    let callback = {
                        let guard = getter_api
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match name {
                            "get_commands" => guard.get_commands.clone(),
                            "get_active_tools" => guard.get_active_tools.clone(),
                            _ => guard.get_all_tools.clone(),
                        }
                    };
                    let json = callback
                        .map(|callback| callback())
                        .unwrap_or_else(|| serde_json::json!([]));
                    getter_lua
                        .to_value(&json)
                        .map_err(luaur_rt::Error::external)
                }),
            )
            .expect("set pillar tool/command getter");
    }

    // pillar.set_model(model) (upstream `setModel`; the host resolves the
    // model from its provider/id and switches the session).
    let model_api = Arc::clone(host_api);
    let lua_model = lua.clone();
    module
        .set(
            "set_model",
            Function::wrap(move |model: Value| {
                let json = lua_model.from_value::<serde_json::Value>(model)?;
                let callback = {
                    let guard = model_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.set_model.clone()
                };
                if let Some(callback) = callback {
                    callback(json).map_err(luaur_rt::Error::external)?;
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_model");

    // pillar.set_active_tools(names)
    let active_api = Arc::clone(host_api);
    let lua_active = lua.clone();
    module
        .set(
            "set_active_tools",
            Function::wrap(move |names: Value| {
                let json = lua_active.from_value::<serde_json::Value>(names)?;
                let callback = {
                    let guard = active_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.set_active_tools.clone()
                };
                if let Some(callback) = callback {
                    callback(json).map_err(luaur_rt::Error::external)?;
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_active_tools");

    // pillar.get_thinking_level() / set_thinking_level(level)
    let thinking_get_api = Arc::clone(host_api);
    module
        .set(
            "get_thinking_level",
            Function::wrap(move || {
                let callback = {
                    let guard = thinking_get_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.get_thinking_level.clone()
                };
                Ok::<Option<String>, luaur_rt::Error>(callback.and_then(|callback| callback()))
            }),
        )
        .expect("set pillar.get_thinking_level");

    let thinking_set_api = Arc::clone(host_api);
    module
        .set(
            "set_thinking_level",
            Function::wrap(move |level: String| {
                let callback = {
                    let guard = thinking_set_api
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.set_thinking_level.clone()
                };
                if let Some(callback) = callback {
                    callback(&level).map_err(luaur_rt::Error::external)?;
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_thinking_level");

    // pillar.get_flag(name): the parsed CLI flag value (upstream
    // `getFlag`); `nil` when the flag was not given.
    let flag_slot = Arc::clone(host_api);
    let lua_get_flag = lua.clone();
    module
        .set(
            "get_flag",
            Function::wrap(move |name: String| {
                let value = {
                    let guard = flag_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.get_flag.as_ref().and_then(|get_flag| get_flag(&name))
                };
                match value {
                    Some(value) => lua_get_flag
                        .to_value(&value)
                        .map_err(luaur_rt::Error::external),
                    None => Ok(Value::Nil),
                }
            }),
        )
        .expect("set pillar.get_flag");

    // pillar.register_message_renderer(custom_type, renderer) /
    // pillar.register_entry_renderer(custom_type, renderer): the renderer
    // stays in the VM, keyed by custom type (upstream the extension's
    // `messageRenderers` / `entryRenderers` maps); the registry records the
    // custom type so the host can resolve it.
    let messages_sink = Arc::clone(registry);
    let messages_lua = lua.clone();
    module
        .set(
            "register_message_renderer",
            Function::wrap(move |custom_type: String, renderer: Value| {
                let store = messages_lua
                    .load(
                        r#"
                        local custom_type, renderer = ...
                        __pillar_message_renderers = __pillar_message_renderers or {}
                        __pillar_message_renderers[custom_type] = renderer
                        return true
                    "#,
                    )
                    .call::<bool>((custom_type.as_str(), renderer))
                    .is_ok();
                if store {
                    messages_sink
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .message_renderers
                        .push(custom_type);
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_message_renderer");

    let entries_sink = Arc::clone(registry);
    let entries_lua = lua.clone();
    module
        .set(
            "register_entry_renderer",
            Function::wrap(move |custom_type: String, renderer: Value| {
                let store = entries_lua
                    .load(
                        r#"
                        local custom_type, renderer = ...
                        __pillar_entry_renderers = __pillar_entry_renderers or {}
                        __pillar_entry_renderers[custom_type] = renderer
                        return true
                    "#,
                    )
                    .call::<bool>((custom_type.as_str(), renderer))
                    .is_ok();
                if store {
                    entries_sink
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry_renderers
                        .push(custom_type);
                }
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_entry_renderer");

    // pillar.register_markdown_transformer(transformer): the transformer
    // stays in the VM; the registry records its identity (`@N`) in
    // registration order (upstream keeps one transformer per extension).
    let transformers_sink = Arc::clone(registry);
    let transformers_lua = lua.clone();
    module
        .set(
            "register_markdown_transformer",
            Function::wrap(move |transformer: Value| {
                let identity = transformers_lua
                    .load(
                        r#"
                        local transformer = ...
                        __pillar_markdown_transformers = __pillar_markdown_transformers or {}
                        table.insert(__pillar_markdown_transformers, transformer)
                        return "@" .. tostring(#__pillar_markdown_transformers - 1)
                    "#,
                    )
                    .call::<String>((transformer,))
                    .map_err(luaur_rt::Error::external)?;
                transformers_sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .markdown_transformers
                    .push(identity);
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.register_markdown_transformer");

    install_schema_module(lua, &module);
    install_events_bus(lua, &module);
    install_fs_module(lua, &module, host_api);

    // Registration cannot fail for a fresh VM; surface for clarity.
    if let Err(error) = lua.register_module("@pillar", module) {
        panic!("register @pillar failed: {error}");
    }
}

/// Install `pillar.events` (upstream the `EventBus`): a per-extension
/// pub/sub with ordered delivery and an unsubscribe handle. Handler
/// errors are caught and reported to stderr, matching the upstream
/// `safeHandler` (they never break the emitter).
fn install_events_bus(lua: &Lua, module: &luaur_rt::Table) {
    let events: Value = lua
        .load(
            r#"
            local handlers = {}
            local next_key = 0

            local function remove(channel, key)
                local list = handlers[channel]
                if not list then return end
                for index = #list, 1, -1 do
                    if list[index].key == key then
                        table.remove(list, index)
                        return
                    end
                end
            end

            local function on(channel, handler)
                next_key = next_key + 1
                local key = next_key
                handlers[channel] = handlers[channel] or {}
                table.insert(handlers[channel], { key = key, handler = handler })
                return function()
                    remove(channel, key)
                end
            end

            local function emit(channel, data)
                local list = handlers[channel]
                if not list then return end
                -- Snapshot so handlers may unsubscribe during delivery;
                -- registration order is preserved.
                local snapshot = {}
                for index = 1, #list do
                    snapshot[index] = list[index].handler
                end
                for index = 1, #snapshot do
                    local ok, err = pcall(snapshot[index], data)
                    if not ok then
                        __pillar_bus_reported = __pillar_bus_reported or {}
                        table.insert(__pillar_bus_reported, channel .. ": " .. tostring(err))
                    end
                end
            end

            return { on = on, emit = emit }
        "#,
        )
        .eval::<Value>()
        .expect("install pillar.events");
    module.set("events", events).expect("set pillar.events");
}

/// Convert an optional Lua options table to a JSON object (mirrors the
/// value bridge: `nil` and non-tables become an empty object).
fn options_to_json(
    lua: &Lua,
    options: Option<Value>,
) -> Result<serde_json::Value, luaur_rt::Error> {
    match options {
        None | Some(Value::Nil) => Ok(serde_json::json!({})),
        Some(value) => {
            let json = lua.from_value::<serde_json::Value>(value)?;
            Ok(if json.is_object() {
                json
            } else {
                serde_json::json!({})
            })
        }
    }
}

/// Install `pillar.fs` (upstream the bounded file API): every call goes to
/// the host callback, so path resolution and the trust model stay host-side.
/// Missing files answer `nil` / `false` instead of raising.
fn install_fs_module(lua: &Lua, module: &luaur_rt::Table, host_api: &Arc<Mutex<HostApi>>) {
    let fs = lua.create_table();

    let call = |host_api: &Arc<Mutex<HostApi>>,
                op: &str,
                path: &str,
                content: Option<&str>|
     -> Result<Option<serde_json::Value>, String> {
        let callback = {
            let guard = host_api
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.fs.clone()
        };
        match callback {
            Some(callback) => callback(op, path, content).map(Some),
            None => Err(format!("pillar.fs.{op}: the fs host is not installed")),
        }
    };

    // pillar.fs.read(path) -> string? (nil when the file is missing)
    let read_api = Arc::clone(host_api);
    let read_lua = lua.clone();
    fs.set(
        "read",
        Function::wrap(move |path: String| {
            let result = call(&read_api, "read", &path, None).map_err(luaur_rt::Error::external)?;
            match result {
                Some(serde_json::Value::Null) | None => Ok(Value::Nil),
                Some(value) => read_lua.to_value(&value).map_err(luaur_rt::Error::external),
            }
        }),
    )
    .expect("set pillar.fs.read");

    // pillar.fs.write(path, content) -> boolean
    let write_api = Arc::clone(host_api);
    fs.set(
        "write",
        Function::wrap(move |path: String, content: String| {
            call(&write_api, "write", &path, Some(&content)).map_err(luaur_rt::Error::external)?;
            Ok::<bool, luaur_rt::Error>(true)
        }),
    )
    .expect("set pillar.fs.write");

    // pillar.fs.list(path) -> { string } (sorted names)
    let list_api = Arc::clone(host_api);
    let list_lua = lua.clone();
    fs.set(
        "list",
        Function::wrap(move |path: String| {
            let result = call(&list_api, "list", &path, None).map_err(luaur_rt::Error::external)?;
            let value = result.unwrap_or_else(|| serde_json::json!([]));
            list_lua.to_value(&value).map_err(luaur_rt::Error::external)
        }),
    )
    .expect("set pillar.fs.list");

    // pillar.fs.stat(path) -> { type, size, modified_ms }? (nil when missing)
    let stat_api = Arc::clone(host_api);
    let stat_lua = lua.clone();
    fs.set(
        "stat",
        Function::wrap(move |path: String| {
            let result = call(&stat_api, "stat", &path, None).map_err(luaur_rt::Error::external)?;
            match result {
                Some(serde_json::Value::Null) | None => Ok(Value::Nil),
                Some(value) => stat_lua.to_value(&value).map_err(luaur_rt::Error::external),
            }
        }),
    )
    .expect("set pillar.fs.stat");

    // pillar.fs.exists(path) -> boolean
    let exists_api = Arc::clone(host_api);
    fs.set(
        "exists",
        Function::wrap(move |path: String| {
            let result =
                call(&exists_api, "exists", &path, None).map_err(luaur_rt::Error::external)?;
            Ok::<bool, luaur_rt::Error>(matches!(result, Some(serde_json::Value::Bool(true))))
        }),
    )
    .expect("set pillar.fs.exists");

    module.set("fs", fs).expect("set pillar.fs");
}

/// Install `pillar.schema` (upstream the typebox builders): each builder
/// returns a plain JSON-Schema-shaped table, which the host converts at
/// the tool-registration boundary.
///
/// divergence: upstream returns typebox schema objects with a metatable
/// marker; the port returns plain tables (the boundary conversion is
/// identical) and keeps the documented field names.
fn install_schema_module(lua: &Lua, module: &luaur_rt::Table) {
    let schema = lua.create_table();

    // The scalar builders only attach `type` to the caller's options.
    for type_name in ["string", "number", "boolean"] {
        let lua_builder = lua.clone();
        schema
            .set(
                type_name,
                Function::wrap(move |options: Option<Value>| {
                    let mut json = options_to_json(&lua_builder, options)?;
                    if let Some(object) = json.as_object_mut() {
                        object.insert(
                            "type".to_string(),
                            serde_json::Value::String(type_name.to_string()),
                        );
                    }
                    lua_builder
                        .to_value(&json)
                        .map_err(luaur_rt::Error::external)
                }),
            )
            .expect("set pillar.schema scalar");
    }

    // pillar.schema.enum({...}) → { type = "string", enum = {...} }
    let lua_enum = lua.clone();
    schema
        .set(
            "enum",
            Function::wrap(move |values: Value| {
                let values = lua_enum.from_value::<serde_json::Value>(values)?;
                let json = serde_json::json!({ "type": "string", "enum": values });
                lua_enum.to_value(&json).map_err(luaur_rt::Error::external)
            }),
        )
        .expect("set pillar.schema.enum");

    // pillar.schema.array(item) → { type = "array", items = item }
    let lua_array = lua.clone();
    schema
        .set(
            "array",
            Function::wrap(move |item: Value| {
                let item = lua_array.from_value::<serde_json::Value>(item)?;
                let json = serde_json::json!({ "type": "array", "items": item });
                lua_array.to_value(&json).map_err(luaur_rt::Error::external)
            }),
        )
        .expect("set pillar.schema.array");

    // pillar.schema.object(properties, opts?) →
    // { type = "object", properties = ..., ...opts }
    let lua_object = lua.clone();
    schema
        .set(
            "object",
            Function::wrap(move |properties: Value, options: Option<Value>| {
                let properties = lua_object.from_value::<serde_json::Value>(properties)?;
                let mut json = options_to_json(&lua_object, options)?;
                if let Some(object) = json.as_object_mut() {
                    object.insert(
                        "type".to_string(),
                        serde_json::Value::String("object".to_string()),
                    );
                    object.insert("properties".to_string(), properties);
                }
                lua_object
                    .to_value(&json)
                    .map_err(luaur_rt::Error::external)
            }),
        )
        .expect("set pillar.schema.object");

    module.set("schema", schema).expect("set pillar.schema");
}

#[cfg(test)]
mod api_tests {
    use super::*;

    /// Run one inline extension's top-level body (the `require("@pillar")`
    /// convention: `load_extension` executes the chunk) and return the
    /// registry snapshot.
    fn run(body: &str) -> HostRegistry {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "api.luau",
                &format!("local pillar = require(\"@pillar\")\n{body}\nreturn nil"),
            )
            .unwrap();
        runtime.registry()
    }

    /// `pillar.get_flag(name)` reads the host flag values (upstream
    /// `getFlag`); a missing flag is nil.
    #[test]
    fn get_flag_reads_the_host_values() {
        let mut runtime = ExtensionRuntime::new();
        runtime.set_host_api(HostApi {
            get_flag: Some(Arc::new(|name: &str| match name {
                "level" => Some(serde_json::json!("high")),
                _ => None,
            })),
            ..Default::default()
        });
        runtime
            .load_extension(
                "flags.luau",
                r#"
                local pillar = require("@pillar")
                pillar.append_entry("flag", { value = pillar.get_flag("level") })
                -- A Lua table cannot hold a nil field, so record presence.
                pillar.append_entry("missing", { present = pillar.get_flag("nope") ~= nil })
                return nil
                "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(registry.appended_entries.len(), 2);
        assert_eq!(
            registry.appended_entries[0].1,
            Some(serde_json::json!({ "value": "high" }))
        );
        assert_eq!(
            registry.appended_entries[1].1,
            Some(serde_json::json!({ "present": false }))
        );
    }

    /// Without a host callback every flag is nil (the documented
    /// default).
    #[test]
    fn get_flag_without_a_host_answers_nil() {
        let registry =
            run(r#"pillar.append_entry("flag", { present = pillar.get_flag("level") ~= nil })"#);
        assert_eq!(
            registry.appended_entries[0].1,
            Some(serde_json::json!({ "present": false }))
        );
    }

    /// `pillar.events` delivers in registration order and the returned
    /// handle unsubscribes (upstream `EventBus`).
    #[test]
    fn events_deliver_in_order_and_unsubscribe() {
        let registry = run(r#"
            local seen = {}
            local unsubscribe = pillar.events.on("chan", function(data)
                table.insert(seen, data)
            end)
            pillar.events.emit("chan", "a")
            pillar.events.emit("chan", "b")
            unsubscribe()
            pillar.events.emit("chan", "c")
            pillar.append_entry("seen", { values = seen })
            "#);
        assert_eq!(
            registry.appended_entries[0].1,
            Some(serde_json::json!({ "values": ["a", "b"] }))
        );
    }

    /// A throwing handler does not stop the other handlers (upstream
    /// `safeHandler`).
    #[test]
    fn events_handler_errors_are_caught() {
        let registry = run(r#"
            pillar.events.on("chan", function()
                error("boom")
            end)
            pillar.events.on("chan", function(data)
                pillar.append_entry("ok", { value = data })
            end)
            pillar.events.emit("chan", 7)
            "#);
        assert_eq!(
            registry.appended_entries[0].1,
            Some(serde_json::json!({ "value": 7 }))
        );
    }

    /// `pillar.events` type-checks with the shipped definitions.
    #[test]
    fn events_type_checks() {
        let runtime = ExtensionRuntime::new();
        runtime
            .type_check(
                "events.luau",
                r#"
                --!strict
                local pillar = require("@pillar")
                local unsubscribe = pillar.events.on("chan", function(data)
                    return nil
                end)
                pillar.events.emit("chan", { n = 1 })
                unsubscribe()
                return nil
                "#,
            )
            .unwrap_or_else(|diagnostics| panic!("type check failed: {diagnostics:?}"));
    }

    /// The `pillar.schema` builders emit JSON-Schema-shaped tables that
    /// the tool boundary converts verbatim.
    #[test]
    fn schema_builders_emit_json_schema() {
        let registry = run(r#"
            pillar.register_tool({
                name = "greet",
                label = "Greet",
                description = "Greet someone",
                parameters = pillar.schema.object({
                    name = pillar.schema.string({ description = "Name to greet" }),
                    count = pillar.schema.number({ minimum = 0 }),
                    loud = pillar.schema.boolean(),
                    level = pillar.schema.enum({ "low", "high" }),
                    tags = pillar.schema.array(pillar.schema.string()),
                }, { required = { "name" } }),
            })
            "#);
        let tool = &registry.tools[0];
        assert_eq!(tool["name"], serde_json::json!("greet"));
        let parameters = &tool["parameters"];
        assert_eq!(parameters["type"], serde_json::json!("object"));
        assert_eq!(parameters["required"], serde_json::json!(["name"]));
        let properties = &parameters["properties"];
        assert_eq!(properties["name"]["type"], serde_json::json!("string"));
        assert_eq!(
            properties["name"]["description"],
            serde_json::json!("Name to greet")
        );
        assert_eq!(properties["count"]["type"], serde_json::json!("number"));
        assert_eq!(properties["count"]["minimum"], serde_json::json!(0));
        assert_eq!(properties["loud"]["type"], serde_json::json!("boolean"));
        assert_eq!(
            properties["level"],
            serde_json::json!({ "type": "string", "enum": ["low", "high"] })
        );
        assert_eq!(properties["tags"]["type"], serde_json::json!("array"));
        assert_eq!(
            properties["tags"]["items"]["type"],
            serde_json::json!("string")
        );
    }

    /// Handlers receive `ctx` (upstream `handler(event, ctx)`) with the host
    /// facts and a working `ui` bridge; without a UI host every `ui` call is
    /// the upstream no-op.
    #[test]
    fn context_is_passed_to_handlers() {
        use pillar_coding_agent::core::extensions_types::{
            ExtensionContextFacts, ExtensionMode, ExtensionUiRequest,
        };
        let seen: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ExtensionRuntime::new();
        let sink = Arc::clone(&seen);
        runtime.set_host_api(HostApi {
            context: Some(Arc::new(|| ExtensionContextFacts {
                cwd: "/tmp/project".to_string(),
                mode: ExtensionMode::Tui,
                has_ui: true,
            })),
            ui: Some(Arc::new(move |request| {
                sink.lock().unwrap().push(request);
                Ok(())
            })),
            ..Default::default()
        });
        runtime
            .load_extension(
                "ctx.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("session_start", function(event, ctx)
                    ctx.ui.notify("hello", "warning")
                    ctx.ui.set_status("demo", "42")
                    ctx.ui.set_tools_expanded(true)
                    return {
                        cwd = ctx.cwd,
                        mode = ctx.mode,
                        hasUI = ctx.hasUI,
                        themeName = ctx.theme.name,
                    }
                end)
                return nil
                "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("session_start", serde_json::json!({}))
            .unwrap();
        match outcome {
            HandlerOutcome::Table(table) => {
                assert_eq!(table["cwd"], serde_json::json!("/tmp/project"));
                assert_eq!(table["mode"], serde_json::json!("tui"));
                assert_eq!(table["hasUI"], serde_json::json!(true));
                // The theme table always exists; its name is either the
                // active theme (a sibling test may have initialized one) or
                // nil when no theme was ever loaded.
                assert!(
                    table["themeName"].is_null() || table["themeName"].is_string(),
                    "{:?}",
                    table["themeName"]
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].op, "notify");
        assert_eq!(
            calls[0].args,
            serde_json::json!({ "message": "hello", "type": "warning" })
        );
        assert_eq!(calls[1].op, "set_status");
        assert_eq!(
            calls[1].args,
            serde_json::json!({ "key": "demo", "text": "42" })
        );
        assert_eq!(calls[2].op, "set_tools_expanded");
        assert_eq!(calls[2].args, serde_json::json!({ "expanded": true }));
    }

    /// Without a host UI context (print / json modes) the `ui` methods are
    /// no-ops and answer the upstream defaults.
    #[test]
    fn context_ui_without_a_host_is_a_noop() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "nohost.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("session_start", function(event, ctx)
                    ctx.ui.notify("nobody listens")
                    ctx.ui.set_editor_text("draft")
                    return { themes = #ctx.ui.get_all_themes(), cwd = ctx.cwd }
                end)
                return nil
                "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("session_start", serde_json::json!({}))
            .unwrap();
        match outcome {
            HandlerOutcome::Table(table) => {
                assert_eq!(table["cwd"], serde_json::json!(""));
                assert!(table["themes"].as_u64().unwrap_or_default() > 0);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// `ctx.ui.theme` styles with the active theme's colours and leaves
    /// unknown colour names plain; `ctx` also reaches tool `execute`.
    #[test]
    fn context_theme_and_tool_context() {
        pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "theme.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_tool({
                    name = "ctx-tool",
                    description = "Probe",
                    execute = function(tool_call_id, params, signal, on_update, ctx)
                        return {
                            content = { { type = "text", text = ctx.mode } },
                            details = { styled = ctx.theme.fg("accent", "X"), plain = ctx.theme.fg("nope", "Y") },
                        }
                    end,
                })
                pillar.on("session_start", function(event, ctx)
                    return {
                        styled = ctx.theme.fg("dim", "D"),
                        plain = ctx.theme.fg("nope", "P"),
                        bold = ctx.theme.bold("B"),
                        name = ctx.theme.name,
                        themes = #ctx.ui.get_all_themes(),
                    }
                end)
                return nil
                "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("session_start", serde_json::json!({}))
            .unwrap();
        let table = match outcome {
            HandlerOutcome::Table(table) => table,
            other => panic!("unexpected outcome: {other:?}"),
        };
        let active = pillar_coding_agent::modes::interactive::theme::theme();
        assert_eq!(table["styled"], serde_json::json!(active.fg("dim", "D")));
        assert_eq!(table["plain"], serde_json::json!("P"));
        assert_eq!(table["bold"], serde_json::json!(active.bold("B")));
        assert_eq!(table["name"], serde_json::json!("dark"));
        assert!(table["themes"].as_u64().unwrap_or_default() >= 2);

        let result = runtime
            .call_tool("ctx-tool", "call-1", serde_json::json!({}))
            .unwrap();
        assert_eq!(result["content"][0]["text"], serde_json::json!("print"));
        assert_eq!(
            result["details"]["styled"],
            serde_json::json!(active.fg("accent", "X"))
        );
        assert_eq!(result["details"]["plain"], serde_json::json!("Y"));
    }

    /// A `ui` host error surfaces to the caller as a catchable error.
    #[test]
    fn context_ui_errors_surface() {
        let mut runtime = ExtensionRuntime::new();
        runtime.set_host_api(HostApi {
            ui: Some(Arc::new(|_request| Err("no ui".to_string()))),
            ..Default::default()
        });
        runtime
            .load_extension(
                "failing.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("session_start", function(event, ctx)
                    local ok, err = pcall(function() ctx.ui.notify("x") end)
                    return { ok = ok, err = tostring(err) }
                end)
                return nil
                "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("session_start", serde_json::json!({}))
            .unwrap();
        match outcome {
            HandlerOutcome::Table(table) => {
                assert_eq!(table["ok"], serde_json::json!(false));
                assert!(
                    table["err"].as_str().unwrap_or_default().contains("no ui"),
                    "{:?}",
                    table["err"]
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// `register_message_renderer` keeps the renderer in the VM and records
    /// the custom type; the host call hands it the payload and options.
    #[test]
    fn message_renderer_registration_and_call() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "renderers.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_message_renderer("my-card", function(message, options)
                    return {
                        lines = {
                            { { text = "card: ", style = "dim" }, { text = message.details.title } },
                            "expanded=" .. tostring(options.expanded),
                        },
                    }
                end)
                pillar.register_entry_renderer("my-entry", function(entry, options)
                    return "entry " .. entry.customType .. " " .. tostring(entry.data.value)
                end)
                return nil
                "#,
            )
            .unwrap();
        assert_eq!(runtime.registry().message_renderers, vec!["my-card"]);
        assert_eq!(runtime.registry().entry_renderers, vec!["my-entry"]);

        let rendered = runtime
            .render_custom_message(
                "my-card",
                &serde_json::json!({ "details": { "title": "hello" } }),
                &serde_json::json!({ "expanded": true, "outputPad": 2 }),
            )
            .unwrap()
            .expect("renderer answers a description");
        assert_eq!(
            rendered,
            serde_json::json!({
                "lines": [
                    [{ "text": "card: ", "style": "dim" }, { "text": "hello" }],
                    "expanded=true",
                ],
            })
        );

        let entry = runtime
            .render_custom_entry(
                "my-entry",
                &serde_json::json!({ "customType": "my-entry", "data": { "value": 7 } }),
                &serde_json::json!({ "expanded": false }),
            )
            .unwrap();
        assert_eq!(entry, Some(serde_json::json!("entry my-entry 7")));

        // An unregistered custom type answers nil (no renderer).
        assert_eq!(
            runtime
                .render_custom_message("other", &serde_json::json!({}), &serde_json::json!({}))
                .unwrap(),
            None
        );
    }

    /// A renderer that raises surfaces the error to the host (the bridge
    /// turns it into the failure notice).
    #[test]
    fn message_renderer_errors_surface() {
        let mut runtime = ExtensionRuntime::new();
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
        let error = runtime
            .render_custom_message(
                "broken",
                &serde_json::json!({}),
                &serde_json::json!({ "expanded": false }),
            )
            .unwrap_err();
        assert!(error.contains("renderer blew up"), "{error}");
    }

    /// `register_markdown_transformer` runs the transformer with the
    /// context, keeps `nil`, and rejects a non-string answer.
    #[test]
    fn markdown_transformer_registration_and_call() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "transform.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_markdown_transformer(function(markdown, context)
                    if context.messageType ~= "user" then return nil end
                    return markdown .. " [" .. tostring(context.availableWidth) .. "]"
                end)
                return nil
                "#,
            )
            .unwrap();
        assert_eq!(runtime.registry().markdown_transformers, vec!["@0"]);

        let user = serde_json::json!({
            "messageType": "user", "isStreaming": false, "availableWidth": 80,
        });
        assert_eq!(
            runtime.transform_markdown("@0", "hello", &user).unwrap(),
            Some("hello [80]".to_string())
        );
        let assistant = serde_json::json!({
            "messageType": "assistant", "isStreaming": false, "availableWidth": 80,
        });
        assert_eq!(
            runtime
                .transform_markdown("@0", "hello", &assistant)
                .unwrap(),
            None
        );
        // An out-of-range identity (no transformer) answers nil.
        assert_eq!(
            runtime.transform_markdown("@3", "hello", &user).unwrap(),
            None
        );
    }

    /// A transformer answering a non-string is an error.
    #[test]
    fn markdown_transformer_rejects_non_strings() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "bad-transform.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_markdown_transformer(function() return 42 end)
                return nil
                "#,
            )
            .unwrap();
        let error = runtime
            .transform_markdown("@0", "hello", &serde_json::json!({}))
            .unwrap_err();
        assert!(error.contains("expected a string"), "{error}");
    }

    /// `pillar.fs` forwards every operation to the host callback and
    /// answers nil / false for missing paths.
    #[test]
    fn fs_module_uses_the_host_callbacks() {
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ExtensionRuntime::new();
        let sink = Arc::clone(&calls);
        runtime.set_host_api(HostApi {
            fs: Some(Arc::new(move |op, path, _content| {
                sink.lock()
                    .unwrap()
                    .push((op.to_string(), path.to_string()));
                Ok(match op {
                    "read" => serde_json::json!("content"),
                    "write" => serde_json::json!(true),
                    "list" => serde_json::json!(["a.luau", "b.luau"]),
                    "stat" => serde_json::json!({ "type": "file", "size": 7 }),
                    "exists" => serde_json::json!(true),
                    _ => serde_json::Value::Null,
                })
            })),
            ..Default::default()
        });
        runtime
            .load_extension(
                "fs.luau",
                r#"
                local pillar = require("@pillar")
                local text = pillar.fs.read("notes.txt")
                local written = pillar.fs.write("out.txt", "hello")
                local names = pillar.fs.list(".")
                local info = pillar.fs.stat("notes.txt")
                local present = pillar.fs.exists("notes.txt")
                __fs_seen = {
                    text = text,
                    written = written,
                    first = names[1],
                    count = #names,
                    kind = info.type,
                    present = present,
                }
                return nil
                "#,
            )
            .unwrap();
        let seen: serde_json::Value = runtime
            .vm()
            .load("return __fs_seen")
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(seen["text"], serde_json::json!("content"));
        assert_eq!(seen["written"], serde_json::json!(true));
        assert_eq!(seen["first"], serde_json::json!("a.luau"));
        assert_eq!(seen["count"], serde_json::json!(2));
        assert_eq!(seen["kind"], serde_json::json!("file"));
        assert_eq!(seen["present"], serde_json::json!(true));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                ("read".to_string(), "notes.txt".to_string()),
                ("write".to_string(), "out.txt".to_string()),
                ("list".to_string(), ".".to_string()),
                ("stat".to_string(), "notes.txt".to_string()),
                ("exists".to_string(), "notes.txt".to_string()),
            ]
        );
    }

    /// Without an fs host the module raises (a catchable error) and a
    /// missing host reports nothing silently.
    #[test]
    fn fs_module_without_a_host_raises() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "nofs.luau",
                r#"
                local pillar = require("@pillar")
                local ok, err = pcall(function()
                    pillar.fs.read("x")
                end)
                __fs_error = tostring(err)
                return nil
                "#,
            )
            .unwrap();
        let error: serde_json::Value = runtime
            .vm()
            .load("return __fs_error")
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert!(
            error.as_str().unwrap_or_default().contains("fs host"),
            "{error:?}"
        );
    }

    /// `require("@ext/<name>")` resolves another loaded extension's export.
    #[test]
    fn extensions_can_require_each_other() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "shared.luau",
                r#"
                return { greet = function(name) return "hi " .. name end }
                "#,
            )
            .unwrap();
        runtime
            .load_extension(
                "uses.luau",
                r#"
                local shared = require("@ext/shared")
                __shared_greeting = shared.greet("there")
                return nil
                "#,
            )
            .unwrap();
        let greeting: serde_json::Value = runtime
            .vm()
            .load("return __shared_greeting")
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(greeting, serde_json::json!("hi there"));
    }

    /// The session-facing methods call their host callbacks with the
    /// upstream argument shapes (upstream the ExtensionAPI actions).
    #[test]
    fn session_methods_call_the_host_callbacks() {
        type Appended = Vec<(String, Option<serde_json::Value>)>;
        let appended: Arc<Mutex<Appended>> = Arc::new(Mutex::new(Vec::new()));
        let sent: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        type Labels = Vec<(String, Option<String>)>;
        let labels: Arc<Mutex<Labels>> = Arc::new(Mutex::new(Vec::new()));
        let levels: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let models: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let tools: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

        let mut runtime = ExtensionRuntime::new();
        let sink = Arc::clone(&appended);
        let sink_send = Arc::clone(&sent);
        let sink_names = Arc::clone(&names);
        let sink_labels = Arc::clone(&labels);
        let sink_levels = Arc::clone(&levels);
        let sink_models = Arc::clone(&models);
        let sink_tools = Arc::clone(&tools);
        runtime.set_host_api(HostApi {
            append_entry: Some(Arc::new(move |kind, data| {
                sink.lock().unwrap().push((kind.to_string(), data));
                Ok(())
            })),
            send_message: Some(Arc::new(move |json| {
                sink_send.lock().unwrap().push(json);
                Ok(())
            })),
            send_user_message: Some(Arc::new(|_json| Ok(()))),
            set_session_name: Some(Arc::new(move |name| {
                sink_names.lock().unwrap().push(name.to_string());
                Ok(())
            })),
            get_session_name: Some(Arc::new(|| Some("session-name".to_string()))),
            set_label: Some(Arc::new(move |entry_id, label| {
                sink_labels
                    .lock()
                    .unwrap()
                    .push((entry_id.to_string(), label.map(str::to_string)));
                Ok(())
            })),
            get_commands: Some(Arc::new(|| serde_json::json!([{ "name": "hello" }]))),
            get_active_tools: Some(Arc::new(|| serde_json::json!(["read", "write"]))),
            get_all_tools: Some(Arc::new(|| serde_json::json!([{ "name": "read" }]))),
            get_thinking_level: Some(Arc::new(|| Some("high".to_string()))),
            set_thinking_level: Some(Arc::new(move |level| {
                sink_levels.lock().unwrap().push(level.to_string());
                Ok(())
            })),
            set_model: Some(Arc::new(move |json| {
                sink_models.lock().unwrap().push(json);
                Ok(())
            })),
            set_active_tools: Some(Arc::new(move |json| {
                sink_tools.lock().unwrap().push(json);
                Ok(())
            })),
            ..Default::default()
        });
        runtime
            .load_extension(
                "session.luau",
                r#"
                local pillar = require("@pillar")
                pillar.append_entry("note", { text = "hi" })
                pillar.send_message({ customType = "ext", content = "msg" })
                pillar.send_user_message("hello")
                pillar.set_session_name("named")
                pillar.set_label("entry-1", "bookmark")
                pillar.set_label("entry-2", nil)
                pillar.set_thinking_level("low")
                pillar.set_model({ provider = "p", id = "a" })
                pillar.set_active_tools({ "read", "write" })
                local level = pillar.get_thinking_level()
                local name = pillar.get_session_name()
                local commands = pillar.get_commands()
                local active = pillar.get_active_tools()
                local all = pillar.get_all_tools()
                __api_seen = {
                    level = level,
                    name = name,
                    commands = commands,
                    active = active,
                    all = all,
                }
                return nil
                "#,
            )
            .unwrap();

        assert_eq!(
            appended.lock().unwrap().as_slice(),
            &[(
                "note".to_string(),
                Some(serde_json::json!({ "text": "hi" }))
            )]
        );
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert_eq!(names.lock().unwrap().as_slice(), &["named".to_string()]);
        assert_eq!(
            labels.lock().unwrap().as_slice(),
            &[
                ("entry-1".to_string(), Some("bookmark".to_string())),
                ("entry-2".to_string(), None)
            ]
        );
        assert_eq!(levels.lock().unwrap().as_slice(), &["low".to_string()]);
        assert_eq!(
            models.lock().unwrap().as_slice(),
            &[serde_json::json!({ "provider": "p", "id": "a" })]
        );
        assert_eq!(
            tools.lock().unwrap().as_slice(),
            &[serde_json::json!(["read", "write"])]
        );

        let seen: serde_json::Value = runtime
            .vm()
            .load("return __api_seen")
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(seen["level"], serde_json::json!("high"));
        assert_eq!(seen["name"], serde_json::json!("session-name"));
        assert_eq!(seen["commands"], serde_json::json!([{ "name": "hello" }]));
        assert_eq!(seen["active"], serde_json::json!(["read", "write"]));
        assert_eq!(seen["all"], serde_json::json!([{ "name": "read" }]));
    }

    /// A failing host callback surfaces as a catchable Lua error.
    #[test]
    fn host_callback_errors_are_catchable() {
        let mut runtime = ExtensionRuntime::new();
        runtime.set_host_api(HostApi {
            set_session_name: Some(Arc::new(|_| Err("nope".to_string()))),
            ..Default::default()
        });
        let error = runtime
            .load_extension(
                "failing.luau",
                r#"
                local pillar = require("@pillar")
                local ok, err = pcall(function()
                    pillar.set_session_name("x")
                end)
                __caught = tostring(err)
                return nil
                "#,
            )
            .unwrap_or_else(|error| panic!("load failed: {error:?}"));
        assert!(error.path.ends_with("failing.luau"));
        let caught: serde_json::Value = runtime
            .vm()
            .load("return __caught")
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert!(
            caught.as_str().unwrap_or_default().contains("nope"),
            "{caught:?}"
        );
    }

    /// The API surface installed by the runtime type-checks with the
    /// shipped definitions.
    #[test]
    fn api_surface_type_checks() {
        let runtime = ExtensionRuntime::new();
        runtime
            .type_check(
                "typed.luau",
                r#"
                --!strict
                local pillar = require("@pillar")
                local flag = pillar.get_flag("level")
                local params = pillar.schema.object({
                    name = pillar.schema.string({ description = "Name" }),
                }, { required = { "name" } })
                pillar.register_tool({ name = "t", parameters = params })
                pillar.on("tool_call", function(event)
                    return nil
                end)
                return flag
                "#,
            )
            .unwrap_or_else(|diagnostics| panic!("type check failed: {diagnostics:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module returning a setup function loads with has_setup.
    #[test]
    fn loads_setup_function_export() {
        let mut runtime = ExtensionRuntime::new();
        let loaded = runtime
            .load_extension("test.luau", "return function(pillar) end")
            .unwrap();
        assert!(loaded.has_setup);
        assert_eq!(loaded.path, "test.luau");
    }

    /// A module returning nil is a valid side-effect-only extension.
    #[test]
    fn loads_nil_export_as_side_effect_only() {
        let mut runtime = ExtensionRuntime::new();
        let loaded = runtime.load_extension("side.luau", "return nil").unwrap();
        assert!(!loaded.has_setup);
    }

    /// A module returning a non-function export is rejected (upstream
    /// the loader's export validation).
    #[test]
    fn non_function_exports_are_skipped_as_extensions() {
        // Upstream imports the module (side effects run) and skips it when the
        // default export is not a function; the port keeps it requireable as
        // `@ext/<name>`.
        let mut runtime = ExtensionRuntime::new();
        let loaded = runtime
            .load_extension("helper.luau", "return { helper = function() end }")
            .unwrap();
        assert!(!loaded.has_setup);
        let value: serde_json::Value = runtime
            .vm()
            .load(
                r#"
                local helper = require("@ext/helper")
                return type(helper.helper)
                "#,
            )
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(value, serde_json::json!("function"));
    }

    /// A compile error surfaces as ExtensionLoadError::Compile with
    /// the file path (upstream per-file load failure).
    #[test]
    fn compile_errors_carry_the_path() {
        let mut runtime = ExtensionRuntime::new();
        let error = runtime
            .load_extension("broken.luau", "this is not luau )(")
            .unwrap_err();
        assert!(matches!(error, ExtensionLoadError::Compile(_)));
        assert!(error.to_string().contains("broken.luau"));
    }

    /// `require("@pillar")` resolves inside a loaded extension.
    #[test]
    fn pillar_module_resolves_via_require() {
        let runtime = ExtensionRuntime::new();
        let chunk = runtime.lua.load(
            r#"
            local pillar = require("@pillar")
            return type(pillar) == "table"
        "#,
        );
        let ok: bool = chunk.call(()).expect("require @pillar");
        assert!(ok);
    }

    /// Unknown require roots fail (sandboxed by capability).
    #[test]
    fn unknown_require_roots_fail() {
        let runtime = ExtensionRuntime::new();
        let chunk = runtime.lua.load("return require('@other')");
        let result: Result<bool, _> = chunk.call(());
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;

    /// `pillar.on` records handlers in registration order with a
    /// stable per-function identity (upstream the runner's per-event
    /// handler lists).
    #[test]
    fn on_records_handlers_in_registration_order() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "test.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("tool_call", function() end)
                pillar.on("tool_call", function() end)
                pillar.on("session_start", function() end)
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        let tool_call = registry.handlers_for("tool_call");
        assert_eq!(tool_call.len(), 2);
        assert_ne!(tool_call[0], tool_call[1], "distinct handlers");
        assert_eq!(registry.handlers_for("session_start").len(), 1);
        assert!(registry.handlers_for("unknown").is_empty());
    }

    /// `pillar.register_tool` captures the definition table.
    #[test]
    fn register_tool_captures_definition() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "greet.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_tool({
                    name = "greet",
                    label = "Greet",
                    description = "Greet someone by name",
                })
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(registry.tools.len(), 1);
        assert_eq!(registry.tools[0]["name"], "greet");
        assert_eq!(registry.tools[0]["label"], "Greet");
    }

    /// `pillar.register_command(name, opts)` keeps the name and opts.
    #[test]
    fn register_command_keeps_name_and_opts() {
        let mut runtime = ExtensionRuntime::new();
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
        let registry = runtime.registry();
        assert_eq!(registry.commands.len(), 1);
        assert_eq!(registry.commands[0].0, "hello");
        assert_eq!(registry.commands[0].1["description"], "Say hello");
    }

    /// `pillar.append_entry(type, data?)` distinguishes nil data.
    #[test]
    fn append_entry_handles_nil_data() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "entries.luau",
                r#"
                local pillar = require("@pillar")
                pillar.append_entry("note", { text = "hi" })
                pillar.append_entry("bare", nil)
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(registry.appended_entries.len(), 2);
        assert_eq!(registry.appended_entries[0].0, "note");
        assert_eq!(
            registry.appended_entries[0].1.as_ref().unwrap()["text"],
            "hi"
        );
        assert_eq!(registry.appended_entries[1].0, "bare");
        assert_eq!(registry.appended_entries[1].1, None);
    }

    /// `pillar.set_session_name` records names in call order (upstream
    /// the session_info_changed flow).
    #[test]
    fn set_session_name_records_calls() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "name.luau",
                r#"
                local pillar = require("@pillar")
                pillar.set_session_name("first")
                pillar.set_session_name("second")
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(
            registry.session_names,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    /// send_message and send_user_message share the message sink
    /// (upstream both deliver to the session's message queue).
    #[test]
    fn send_methods_share_the_sink() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "send.luau",
                r#"
                local pillar = require("@pillar")
                pillar.send_message({ role = "user", content = "a" })
                pillar.send_user_message({ role = "user", content = "b" })
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(registry.messages.len(), 2);
        assert_eq!(registry.messages[0]["content"], "a");
        assert_eq!(registry.messages[1]["content"], "b");
    }

    /// Shortcut and flag registration record their keys.
    #[test]
    fn shortcut_and_flag_registration() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "keys.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_shortcut("ctrl+g", { description = "Go" })
                pillar.register_flag("verbose", { description = "Verbose" })
                return nil
            "#,
            )
            .unwrap();
        let registry = runtime.registry();
        assert_eq!(registry.shortcuts.len(), 1);
        assert_eq!(registry.shortcuts[0].0, "ctrl+g");
        assert_eq!(registry.flags.len(), 1);
        assert_eq!(registry.flags[0].0, "verbose");
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::runtime::HandlerOutcome;

    /// A tool_call handler returning `{ block = true, reason }` blocks
    /// the chain with the reason (upstream ToolCallEventResult).
    #[test]
    fn block_result_stops_the_chain() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "blocker.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("tool_call", function(event)
                    if event.tool_name == "bash" then
                        return { block = true, reason = "Blocked by user" }
                    end
                end)
                pillar.on("tool_call", function(event)
                    error("must not run after block")
                end)
                return nil
            "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch(
                "tool_call",
                serde_json::json!({ "tool_name": "bash", "input": { "command": "rm -rf /" } }),
            )
            .unwrap();
        assert_eq!(
            outcome,
            HandlerOutcome::Block {
                reason: Some("Blocked by user".to_string())
            }
        );
    }

    /// Handlers run in registration order; a non-block return lets the
    /// next handler run.
    #[test]
    fn handlers_run_in_registration_order() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "chain.luau",
                r#"
                local pillar = require("@pillar")
                local seen = {}
                pillar.on("tool_call", function(event)
                    table.insert(seen, "first:" .. event.tool_name)
                    _G.__seen = seen
                    return nil
                end)
                pillar.on("tool_call", function(event)
                    table.insert(seen, "second")
                    _G.__seen = seen
                    return nil
                end)
                return nil
            "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("tool_call", serde_json::json!({ "tool_name": "read" }))
            .unwrap();
        assert_eq!(outcome, HandlerOutcome::None);
        let seen: Value = runtime.vm().load("return _G.__seen").call(()).unwrap();
        let json = runtime.vm().from_value::<serde_json::Value>(seen).unwrap();
        assert_eq!(json, serde_json::json!(["first:read", "second"]));
    }

    /// A non-block table return surfaces as Table (modifications feed
    /// the next handler upstream).
    #[test]
    fn non_block_table_returns_surface() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .load_extension(
                "modify.luau",
                r#"
                local pillar = require("@pillar")
                pillar.on("tool_call", function(event)
                    return { tool_name = event.tool_name, renamed = true }
                end)
                return nil
            "#,
            )
            .unwrap();
        let outcome = runtime
            .dispatch("tool_call", serde_json::json!({ "tool_name": "read" }))
            .unwrap();
        assert_eq!(
            outcome,
            HandlerOutcome::Table(serde_json::json!({ "tool_name": "read", "renamed": true }))
        );
    }

    /// Events with no registered handlers resolve to None.
    #[test]
    fn unregistered_events_resolve_to_none() {
        let mut runtime = ExtensionRuntime::new();
        let outcome = runtime
            .dispatch("session_start", serde_json::json!({ "reason": "startup" }))
            .unwrap();
        assert_eq!(outcome, HandlerOutcome::None);
    }

    /// A handler error surfaces as Setup (upstream the runtime catches
    /// handler errors and reports them).
    #[test]
    fn handler_errors_surface() {
        let mut runtime = ExtensionRuntime::new();
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
        let outcome = runtime.dispatch("agent_start", serde_json::json!({}));
        assert!(matches!(outcome, Err(ExtensionLoadError::Setup(_))));
    }
}

#[cfg(test)]
mod typecheck_tests {
    use super::*;

    /// A clean extension passes the type check.
    #[test]
    fn clean_extension_passes() {
        let runtime = ExtensionRuntime::new();
        assert!(
            runtime
                .type_check(
                    "good.luau",
                    r#"
                --!strict
                local pillar = require("@pillar")
                pillar.on("tool_call", function(event)
                    return nil
                end)
                return nil
            "#,
                )
                .is_ok()
        );
    }

    /// Global `declare pillar` definitions type-check direct global
    /// access (the require bridge types as any, so member calls on it
    /// are unchecked — a checker limitation, documented).
    #[test]
    fn global_declare_gates_direct_access() {
        let runtime = ExtensionRuntime::new();
        let result = runtime.type_check(
            "probe.luau",
            r#"
                --!strict
                pillar.set_session_name(42)
                return nil
            "#,
        );
        assert!(result.is_err(), "typed misuse must be caught: {result:?}");
        let result = runtime.type_check(
            "probe-ok.luau",
            r#"
                --!strict
                pillar.set_session_name("ok")
                pillar.append_entry("note", nil)
                return nil
            "#,
        );
        assert!(result.is_ok(), "valid use must pass: {result:?}");
    }

    /// A type error is reported with its location (upstream the
    /// skip-with-warning path). Global `declare` definitions gate the
    /// surface; a checker limitation types require("@pillar") as any,
    /// so member calls through the require bridge are unchecked
    /// (documented divergence until the definitions can bind the
    /// module alias).
    #[test]
    fn type_errors_are_reported_with_locations() {
        let runtime = ExtensionRuntime::new();
        let diagnostics = runtime
            .type_check(
                "bad.luau",
                r#"
                --!strict
                pillar.set_session_name(42)
                return nil
            "#,
            )
            .unwrap_err();
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.in_definitions),
            "script diagnostics only: {diagnostics:?}"
        );
        assert!(diagnostics[0].line >= 1);
    }

    /// The `@pillar` definitions type-check: calling an undefined API
    /// method fails (the definitions gate the surface).
    #[test]
    fn undefined_api_method_is_rejected() {
        let runtime = ExtensionRuntime::new();
        let diagnostics = runtime
            .type_check(
                "unknown.luau",
                r#"
                --!strict
                pillar.nonexistent_method()
                return nil
            "#,
            )
            .unwrap_err();
        assert!(!diagnostics.is_empty());
    }

    /// A syntax error surfaces as a diagnostic (upstream the file is
    /// skipped before it can fail the startup).
    #[test]
    fn syntax_errors_surface_as_diagnostics() {
        let runtime = ExtensionRuntime::new();
        let diagnostics = runtime
            .type_check("broken.luau", "this is not ) luau")
            .unwrap_err();
        assert!(!diagnostics.is_empty());
    }
}

#[cfg(test)]
mod load_and_run_tests {
    use super::*;
    use crate::discovery::{ExtensionOrigin, discover_extension_files};
    use std::path::Path;

    fn write_extension(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// One good extension loads and registers; a type-check failure
    /// and a setup error land in errors without stopping the others.
    #[test]
    fn per_file_failures_do_not_stop_the_flow() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_extension(
            &global,
            "good.luau",
            r#"
            local pillar = require("@pillar")
            pillar.set_session_name("registered")
            return nil
        "#,
        );
        write_extension(
            &global,
            "typo.luau",
            r#"
            --!strict
            pillar.set_session_name(42)
            return nil
        "#,
        );
        write_extension(
            &global,
            "setup_error.luau",
            r#"
            local pillar = require("@pillar")
            error("setup exploded")
        "#,
        );
        let discovered = discover_extension_files(Some(&global), None);
        let mut runtime = ExtensionRuntime::new();
        let (loaded, errors) = runtime.load_and_run(&discovered);
        assert_eq!(loaded.len(), 1, "loaded={loaded:?} errors={errors:?}");
        assert_eq!(errors.len(), 2, "loaded={loaded:?} errors={errors:?}");
        // The good extension's registrations landed.
        assert_eq!(
            runtime.registry().session_names,
            vec!["registered".to_string()]
        );
        // The type-check failure message lists diagnostics.
        let typo = errors
            .iter()
            .find(|(path, _)| path.ends_with("typo.luau"))
            .expect("typo error recorded");
        assert!(typo.1.starts_with("type-check failed"));
        // The setup error uses the upstream prefix.
        let setup = errors
            .iter()
            .find(|(path, _)| path.ends_with("setup_error.luau"))
            .expect("setup error recorded");
        assert!(setup.1.starts_with("Failed to load extension: "));
    }

    /// Global-before-project ordering flows through load_and_run.
    #[test]
    fn discovery_order_flows_through() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        let project = temp.path().join("project");
        write_extension(&global, "a.luau", "return nil");
        write_extension(&project, "b.luau", "return nil");
        let discovered = discover_extension_files(Some(&global), Some(&project));
        assert_eq!(discovered[0].origin, ExtensionOrigin::Global);
        assert_eq!(discovered[1].origin, ExtensionOrigin::Project);
        let mut runtime = ExtensionRuntime::new();
        let (loaded, errors) = runtime.load_and_run(&discovered);
        assert!(errors.is_empty());
        assert_eq!(loaded.len(), 2);
    }

    /// An empty scope list yields an empty result.
    #[test]
    fn empty_discovery_yields_empty() {
        let mut runtime = ExtensionRuntime::new();
        let (loaded, errors) = runtime.load_and_run(&[]);
        assert!(loaded.is_empty());
        assert!(errors.is_empty());
    }

    /// Missing files on disk surface as Io-path errors.
    #[test]
    fn missing_files_surface_as_errors() {
        let mut runtime = ExtensionRuntime::new();
        let missing = vec![crate::discovery::DiscoveredExtension {
            path: std::path::PathBuf::from("/nonexistent/ext.luau"),
            origin: ExtensionOrigin::Global,
        }];
        let (loaded, errors) = runtime.load_and_run(&missing);
        assert!(loaded.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.starts_with("Failed to load extension: "));
    }
}

#[cfg(test)]
mod exec_tests {
    use super::*;

    /// pillar.exec returns the host executor's result shape
    /// (upstream { stdout, stderr, code, killed }).
    #[test]
    fn exec_returns_host_result_shape() {
        let runtime = ExtensionRuntime::new();
        runtime.set_exec_host(Arc::new(|command: &str, args: &[String]| {
            assert_eq!(command, "git");
            serde_json::json!({
                "stdout": format!("args={args:?}"),
                "stderr": "",
                "code": 0,
                "killed": false,
            })
        }));
        let result: serde_json::Value = runtime
            .vm()
            .load(
                r#"
                local pillar = require("@pillar")
                return pillar.exec("git", { "status", "--short" })
            "#,
            )
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(result["stdout"], r#"args=["status", "--short"]"#);
        assert_eq!(result["code"], 0);
        assert_eq!(result["killed"], false);
    }

    /// Calling exec before the host installs one returns the
    /// not-installed failure result (code -1, stderr explanation).
    #[test]
    fn exec_without_host_returns_failure() {
        let runtime = ExtensionRuntime::new();
        let result: serde_json::Value = runtime
            .vm()
            .load(
                r#"
                local pillar = require("@pillar")
                return pillar.exec("anything")
            "#,
            )
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(result["code"], -1);
        assert_eq!(result["stderr"], "exec host not installed");
        assert_eq!(result["stdout"], "");
    }

    /// exec accepts a nil args table (upstream optional args).
    #[test]
    fn exec_accepts_nil_args() {
        let runtime = ExtensionRuntime::new();
        runtime.set_exec_host(Arc::new(|command: &str, args: &[String]| {
            serde_json::json!({ "stdout": command, "stderr": "", "code": args.len(), "killed": false })
        }));
        let result: serde_json::Value = runtime
            .vm()
            .load(
                r#"
                local pillar = require("@pillar")
                return pillar.exec("ls")
            "#,
            )
            .call(())
            .and_then(|value| runtime.vm().from_value(value))
            .unwrap();
        assert_eq!(result["stdout"], "ls");
        assert_eq!(result["code"], 0);
    }
}
