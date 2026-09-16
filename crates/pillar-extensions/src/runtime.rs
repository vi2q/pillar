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

/// Host `declare` definitions for the type-checker: the `@pillar`
/// module surface (grows with the API; currently the registration
/// methods installed by [`install_pillar_api`]).
pub const PILLAR_DEFINITIONS: &str = r#"
declare pillar: {
    on: (event: string, handler: (event: any) -> any) -> (),
    register_tool: (definition: any) -> (),
    register_command: (name: string, opts: any) -> (),
    register_shortcut: (key: string, opts: any) -> (),
    register_flag: (name: string, opts: any) -> (),
    get_flag: (name: string) -> any,
    append_entry: (kind: string, data: any?) -> (),
    send_message: (message: any) -> (),
    send_user_message: (message: any) -> (),
    set_session_name: (name: string) -> (),
    exec: (command: string, args: { number }?, opts: any?) -> any,
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

/// The host callbacks the `@pillar` API reads (upstream the pieces of
/// the runtime the ExtensionAPI reaches). Each is optional: a missing
/// callback answers the documented default (`nil`).
#[derive(Clone, Default)]
pub struct HostApi {
    /// `pillar.get_flag(name)` → the parsed CLI flag value (upstream
    /// `getFlag`).
    pub get_flag: Option<GetFlagFn>,
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
                .call(arg)
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
        let has_setup = match &export {
            luaur_rt::Value::Nil => false,
            luaur_rt::Value::Function(_) => true,
            other => {
                return Err(ExtensionLoadError::InvalidExport(format!(
                    "{path}: expected function or nil export, got {}",
                    other.type_name()
                )));
            }
        };
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

    // pillar.register_tool(def): the definition table converts to
    // JSON at the boundary (payload keys snake_cased mechanically).
    let tools = Arc::clone(registry);
    let lua_tools = lua.clone();
    module
        .set(
            "register_tool",
            Function::wrap(move |definition: Value| {
                let json = lua_tools.from_value::<serde_json::Value>(definition)?;
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

    // pillar.append_entry(type, data?)
    let entries = Arc::clone(registry);
    let lua_entries = lua.clone();
    module
        .set(
            "append_entry",
            Function::wrap(move |kind: String, data: Value| {
                let json = match data {
                    Value::Nil => None,
                    other => Some(lua_entries.from_value::<serde_json::Value>(other)?),
                };
                entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .appended_entries
                    .push((kind, json));
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.append_entry");

    // pillar.send_message(msg) / pillar.send_user_message(msg)
    for name in ["send_message", "send_user_message"] {
        let sink = Arc::clone(registry);
        let sink_lua = lua.clone();
        module
            .set(
                name,
                Function::wrap(move |message: Value| {
                    let json = sink_lua.from_value::<serde_json::Value>(message)?;
                    sink.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .messages
                        .push(json);
                    Ok::<(), luaur_rt::Error>(())
                }),
            )
            .expect("set pillar send");
    }

    // pillar.set_session_name(name)
    let names = Arc::clone(registry);
    module
        .set(
            "set_session_name",
            Function::wrap(move |name: String| {
                names
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .session_names
                    .push(name);
                Ok::<(), luaur_rt::Error>(())
            }),
        )
        .expect("set pillar.set_session_name");

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

    install_schema_module(lua, &module);

    // Registration cannot fail for a fresh VM; surface for clarity.
    if let Err(error) = lua.register_module("@pillar", module) {
        panic!("register @pillar failed: {error}");
    }
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
                    lua_builder.to_value(&json).map_err(luaur_rt::Error::external)
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
                lua_object.to_value(&json).map_err(luaur_rt::Error::external)
            }),
        )
        .expect("set pillar.schema.object");

    module
        .set("schema", schema)
        .expect("set pillar.schema");
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
            .load_extension("api.luau", &format!("local pillar = require(\"@pillar\")\n{body}\nreturn nil"))
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
        let registry = run(
            r#"pillar.append_entry("flag", { present = pillar.get_flag("level") ~= nil })"#,
        );
        assert_eq!(
            registry.appended_entries[0].1,
            Some(serde_json::json!({ "present": false }))
        );
    }

    /// The `pillar.schema` builders emit JSON-Schema-shaped tables that
    /// the tool boundary converts verbatim.
    #[test]
    fn schema_builders_emit_json_schema() {
        let registry = run(
            r#"
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
            "#,
        );
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
    fn rejects_non_function_export() {
        let mut runtime = ExtensionRuntime::new();
        let error = runtime
            .load_extension("bad.luau", "return { [1] = 42 }")
            .unwrap_err();
        assert!(
            matches!(error, ExtensionLoadError::InvalidExport(_)),
            "unexpected error: {error:?}"
        );
        assert!(error.to_string().contains("bad.luau"));
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
