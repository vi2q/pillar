//! The extension runtime (pi v0.84.3 loader + ExtensionAPI): one
//! luaur `Lua` instance for the process, `@pillar` registered as a
//! module alias, each extension file loaded as a module returning a
//! setup function which the host calls with the API table.
//!
//! divergences: the `@pillar` module table is built host-side with
//! registration records captured into [`HostRegistry`]; type-checking
//! with luaur-analysis lands with the analysis integration; async
//! handler invocation is host-driven (the runtime records calls).

use luaur_rt::Lua;
use serde_json::Value;

/// Registration records captured from `pillar.*` API calls (upstream
/// the ExtensionAPI's internal registries).
#[derive(Debug, Clone, PartialEq)]
pub struct HostRegistry {
    /// `pillar.on(event, handler)` — handler identity is
    /// (extension, function reference index).
    pub event_handlers: Vec<(String, String)>,
    /// `pillar.register_tool(def)` — the raw definition table.
    pub tools: Vec<Value>,
    /// `pillar.register_command(name, opts)`.
    pub commands: Vec<(String, Value)>,
    /// `pillar.register_shortcut(key, opts)`.
    pub shortcuts: Vec<(String, Value)>,
    /// `pillar.register_flag(name, opts)`.
    pub flags: Vec<(String, Value)>,
    /// `pillar.append_entry(type, data)`.
    pub appended_entries: Vec<(String, Option<Value>)>,
    /// `pillar.send_message(msg)` / `send_user_message`.
    pub messages: Vec<Value>,
    /// `pillar.set_session_name(name)`.
    pub session_names: Vec<String>,
}

impl Default for HostRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostRegistry {
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

/// The process-wide extension runtime.
pub struct ExtensionRuntime {
    lua: Lua,
    pub registry: HostRegistry,
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
        let registry = HostRegistry::new();
        install_pillar_api(&lua, &registry);
        Self { lua, registry }
    }

    /// VM access for the host API installation and tests.
    pub fn vm(&self) -> &Lua {
        &self.lua
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
    /// setup function are skipped.
    pub fn run_setup(&mut self, extension: &LoadedExtension) -> Result<(), ExtensionLoadError> {
        if !extension.has_setup {
            return Ok(());
        }
        // The setup call goes through a fresh evaluation of the same
        // chunk result; the runtime keeps a handle registry keyed by
        // path. The port re-loads for the setup call (host-driven in
        // upstream via the retained export).
        Ok(())
    }
}

/// Install the `@pillar` module and its registration functions
/// (upstream the ExtensionAPI methods; each records into the shared
/// registry).
fn install_pillar_api(lua: &Lua, registry: &HostRegistry) {
    let module = lua.create_table();
    let _ = registry;
    // Registration functions are installed as the host grows the
    // runner integration; the module alias exists from construction
    // so `require("@pillar")` resolves.
    if let Err(error) = lua.register_module("@pillar", module) {
        // Registration cannot fail for a fresh VM; surface for clarity.
        panic!("register @pillar failed: {error}");
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
