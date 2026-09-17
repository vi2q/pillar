//! The Luau extension loader bridge (docs/rules/04): adapts
//! pillar-extensions' runtime into pillar-coding-agent's
//! `ModuleLoader` contract — type-check → load → setup per file, with
//! the runner-facing HostExtension built from the registration
//! records (upstream the loader's per-file import + factory
//! invocation).

use std::sync::{Arc, Mutex};

use pillar_extensions_contract::{LoadOutcome, LuauExtensionLoader};

use crate::bridge::bridge_to_runner;
use crate::runtime::ExtensionRuntime;

/// A shared Luau runtime the host owns across loads (the runner
/// bridge holds the same Arc).
pub type SharedRuntime = Arc<Mutex<ExtensionRuntime>>;

/// Create a shared runtime and install the host exec callback.
pub fn create_shared_runtime(exec_host: Option<crate::runtime::ExecHost>) -> SharedRuntime {
    let runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
    if let Some(exec_host) = exec_host {
        runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_exec_host(exec_host);
    }
    runtime
}

/// Reads an extension's source by path.
///
/// The host owns where sources come from — the native filesystem, a project
/// tree the host already walked, an embedded bundle, or a Wasm host's VFS — so
/// the VM crate never touches a disk itself
/// (docs/DEVELOPMENT-STRATEGY.md §4/§5-6).
pub type SourceReader = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// Build a `ModuleLoader` closure over a shared runtime and a source reader:
/// a path load type-checks, compiles, runs the setup body, and returns the
/// bridged `HostExtension`. Per the loader contract, a `Ok(None)`
/// means "not an extension" and errors are per-path strings.
pub fn luau_module_loader(
    runtime: &SharedRuntime,
    source_reader: SourceReader,
) -> impl FnMut(&str) -> LoadOutcome + '_ {
    move |path: &str| {
        let source = source_reader(path)
            .map_err(|error| format!("Failed to load extension: {error}"))?;
        let mut guard = runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.type_check(path, &source).map_err(|diagnostics| {
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
            format!("type-check failed: {summary}")
        })?;
        let loaded = guard
            .load_extension(path, &source)
            .map_err(|error| format!("Failed to load extension: {error}"))?;
        guard
            .run_setup(&loaded)
            .map_err(|error| format!("Failed to load extension: {error}"))?;
        // Drop the guard before bridging: `bridge_to_runner` locks the
        // same non-reentrant mutex (docs/INSTRUCTIONS.md #1 — don't
        // hold a guard across a call that re-locks).
        drop(guard);
        let extension = bridge_to_runner(path, runtime)
            .map_err(|error| format!("Failed to load extension: {error}"))?;
        Ok(Some(extension))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_source(files: Vec<(&str, &str)>) -> SourceReader {
        let files: Vec<(String, String)> = files
            .into_iter()
            .map(|(name, body)| (name.to_string(), body.to_string()))
            .collect();
        Arc::new(move |path: &str| {
            files
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, body)| body.clone())
                .ok_or_else(|| format!("no such extension source: {path}"))
        })
    }

    /// The exec host is visible to loaded extensions (the VM calls out through
    /// the injected callback, never the process itself).
    #[test]
    fn exec_host_flows_through() {
        let source = memory_source(vec![(
            "exec.luau",
            r#"
            local pillar = require("@pillar")
            __result = pillar.exec("probe")
            return nil
            "#,
        )]);
        let runtime = create_shared_runtime(Some(Arc::new(
            |command: &str, _: &[String], _: &pillar_extensions_contract::ExecOptions| {
                pillar_extensions_contract::ExecResult {
                    stdout: command.to_string(),
                    stderr: String::new(),
                    code: 0,
                    killed: false,
                    truncated: false,
                }
            },
        )));
        let mut loader = luau_module_loader(&runtime, source);
        let extension = loader("exec.luau").expect("no error").expect("extension");
        assert_eq!(extension.commands.len(), 0);
        let vm = runtime.lock().unwrap().vm().clone();
        let result: serde_json::Value = {
            use luaur_rt::LuaSerdeExt;
            let value: luaur_rt::Value = vm.load("return __result").call(()).unwrap();
            vm.from_value(value).unwrap()
        };
        assert_eq!(result["stdout"], "probe");
    }

    /// The module loader answers the loader contract: a source the host can
    /// serve loads, a missing one is a per-path error.
    #[test]
    fn module_loader_contract() {
        let runtime = create_shared_runtime(None);
        let source = memory_source(vec![("good.luau", "return nil")]);
        let mut loader = luau_module_loader(&runtime, source);
        assert!(matches!(loader("good.luau"), Ok(Some(_))));
        let outcome = loader("missing.luau");
        assert!(
            matches!(outcome, Err(message) if message.starts_with("Failed to load extension: "))
        );
    }
}

/// A coding-agent-facing loader that wraps a shared Luau runtime
/// (implements `LuauExtensionLoader`). The host owns the runtime Arc
/// across loads; the runner bridge holds the same Arc.
pub struct LuauLoader {
    runtime: SharedRuntime,
    source_reader: SourceReader,
}

impl LuauLoader {
    /// A loader that reads sources through `source_reader` (the host decides
    /// where they come from; see [`SourceReader`]).
    pub fn with_source(runtime: SharedRuntime, source_reader: SourceReader) -> Self {
        Self {
            runtime,
            source_reader,
        }
    }
}

impl LuauExtensionLoader for LuauLoader {
    fn load_extension(
        &self,
        path: &str,
    ) -> LoadOutcome {
        luau_module_loader(&self.runtime, Arc::clone(&self.source_reader))(path)
    }
}

/// Convenience: create the shared runtime plus its coding-agent loader in one
/// call, reading sources through `source_reader`.
pub fn create_luau_loader(
    exec_host: Option<crate::runtime::ExecHost>,
    source_reader: SourceReader,
) -> (SharedRuntime, LuauLoader) {
    let runtime = create_shared_runtime(exec_host);
    let loader = LuauLoader::with_source(Arc::clone(&runtime), source_reader);
    (runtime, loader)
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Serve sources from memory: the loader takes them from the host, so the
    /// tests need no filesystem either.
    fn memory_source(files: Vec<(&str, &str)>) -> SourceReader {
        let files: Vec<(String, String)> = files
            .into_iter()
            .map(|(name, body)| (name.to_string(), body.to_string()))
            .collect();
        Arc::new(move |path: &str| {
            files
                .iter()
                .find(|(name, _)| name == path || path.ends_with(name.as_str()))
                .map(|(_, body)| body.clone())
                .ok_or_else(|| format!("no such extension source: {path}"))
        })
    }

    /// The trait loader runs a real Luau extension: type-check -> setup ->
    /// bridge, with registrations surfacing in the contract's extension
    /// (path discovery and the runner build belong to the host; the
    /// coding-agent parity test covers that wiring).
    #[test]
    fn luau_loader_trait_loads_and_bridges_real_extension() {
        let source = memory_source(vec![(
            "greet.luau",
            r#"
            local pillar = require("@pillar")
            pillar.register_command("hello", { description = "Say hello" })
            pillar.on("session_start", function(event)
                __seen_reason = event.reason
                return nil
            end)
            return nil
            "#,
        )]);
        let (runtime, loader) = create_luau_loader(None, source);

        let extension = loader
            .load_extension("greet.luau")
            .expect("no load error")
            .expect("an extension");
        assert!(
            extension.commands.iter().any(|command| command.name == "hello"),
            "command registered"
        );

        // The extension drives the contract's runner without the host.
        let runner = pillar_extensions_contract::ExtensionRunner::new(vec![extension]);
        assert!(runner.has_handlers("session_start"));
        runner.emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }));
        let vm = runtime.lock().unwrap();
        use luaur_rt::LuaSerdeExt;
        let seen: serde_json::Value = vm
            .vm()
            .load("return __seen_reason")
            .call(())
            .and_then(|value| vm.vm().from_value(value))
            .unwrap_or(serde_json::Value::Null);
        assert_eq!(seen, "startup");
    }

    /// A type-check failure is a per-path error; other files still load.
    #[test]
    fn type_check_failure_is_collected_without_aborting() {
        let source = memory_source(vec![
            ("bad.luau", "local n: number = \"text\""),
            ("good.luau", "return nil"),
        ]);
        let (_runtime, loader) = create_luau_loader(None, source);

        let error = match loader.load_extension("bad.luau") {
            Err(error) => error,
            Ok(_) => panic!("expected a type-check error"),
        };
        assert!(error.contains("type-check failed"), "{error}");
        assert!(loader
            .load_extension("good.luau")
            .expect("good loads")
            .is_some());
    }

    /// A source the host cannot answer is a per-path error, and the runtime
    /// stays usable afterwards.
    #[test]
    fn a_missing_source_is_a_per_path_error() {
        let source = memory_source(Vec::new());
        let (runtime, loader) = create_luau_loader(None, source);

        let error = match loader.load_extension("absent.luau") {
            Err(error) => error,
            Ok(_) => panic!("expected a read error"),
        };
        assert!(error.contains("Failed to load extension"), "{error}");
        let mut guard = runtime.lock().unwrap();
        guard
            .load_extension("later.luau", "return nil")
            .expect("runtime alive");
    }

    /// One loader can load several extensions in turn and the shared runtime
    /// stays usable (the host's discovery / cache wiring sits above this).
    #[test]
    fn one_loader_loads_several_extensions() {
        let source = memory_source(vec![
            (
                "one.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_command("one", { description = "First" })
                return nil
                "#,
            ),
            (
                "two.luau",
                r#"
                local pillar = require("@pillar")
                pillar.register_command("two", { description = "Second" })
                return nil
                "#,
            ),
        ]);
        let (_runtime, loader) = create_luau_loader(None, source);

        let first = loader.load_extension("one.luau").expect("ok").expect("extension");
        let second = loader.load_extension("two.luau").expect("ok").expect("extension");
        assert!(first.commands.iter().any(|command| command.name == "one"));
        assert!(second.commands.iter().any(|command| command.name == "two"));
    }
}
