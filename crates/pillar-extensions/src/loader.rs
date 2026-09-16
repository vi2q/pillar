//! The Luau extension loader bridge (docs/rules/04): adapts
//! pillar-extensions' runtime into pillar-coding-agent's
//! `ModuleLoader` contract — type-check → load → setup per file, with
//! the runner-facing HostExtension built from the registration
//! records (upstream the loader's per-file import + factory
//! invocation).

use std::sync::{Arc, Mutex};

use pillar_extensions_contract::{HostExtension, LoadOutcome, LuauExtensionLoader};

use crate::bridge::bridge_to_runner;
use crate::discovery::discover_extension_files;
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

/// Build a `ModuleLoader` closure over a shared runtime: a path load
/// type-checks, compiles, runs the setup body, and returns the
/// bridged `HostExtension`. Per the loader contract, a `Ok(None)`
/// means "not an extension" and errors are per-path strings.
pub fn luau_module_loader(runtime: &SharedRuntime) -> impl FnMut(&str) -> LoadOutcome + '_ {
    move |path: &str| {
        let source = std::fs::read_to_string(path)
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

/// Discover and load extensions for a project (upstream
/// `discoverAndLoadExtensions`): global scope first, then
/// project-local, using the shared runtime. Returns the bridged
/// extensions and per-path errors.
pub fn discover_and_load(
    runtime: &SharedRuntime,
    global_dir: Option<&std::path::Path>,
    project_dir: Option<&std::path::Path>,
) -> (Vec<HostExtension>, Vec<(String, String)>) {
    let discovered = discover_extension_files(global_dir, project_dir);
    let mut extensions = Vec::new();
    let mut errors = Vec::new();
    let mut loader = luau_module_loader(runtime);
    for entry in discovered {
        let path = entry.path.to_string_lossy().to_string();
        match loader(&path) {
            Ok(Some(extension)) => extensions.push(extension),
            Ok(None) => {}
            Err(error) => errors.push((path, error)),
        }
    }
    (extensions, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_extension(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// discover_and_load bridges a good extension into a runner-shaped
    /// HostExtension with its registrations.
    #[test]
    fn bridges_good_extensions_with_registrations() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_extension(
            &global,
            "good.luau",
            r#"
            local pillar = require("@pillar")
            pillar.register_command("hello", { description = "Say hello" })
            pillar.set_session_name("named")
            return nil
        "#,
        );
        let runtime = create_shared_runtime(None);
        let (extensions, errors) = discover_and_load(&runtime, Some(&global), None);
        assert!(errors.is_empty(), "errors={errors:?}");
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].commands.len(), 1);
        assert_eq!(extensions[0].commands[0].name, "hello");
        // The registration landed in the shared runtime too.
        assert_eq!(
            runtime.lock().unwrap().registry().session_names,
            vec!["named".to_string()]
        );
    }

    /// A type-check failure is a per-path error; other files load.
    #[test]
    fn type_check_failures_are_per_path_errors() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_extension(
            &global,
            "bad.luau",
            r#"
            --!strict
            pillar.set_session_name(42)
            return nil
        "#,
        );
        write_extension(&global, "fine.luau", "return nil");
        let runtime = create_shared_runtime(None);
        let (extensions, errors) = discover_and_load(&runtime, Some(&global), None);
        assert_eq!(extensions.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.starts_with("type-check failed"));
    }

    /// The exec host is visible to loaded extensions.
    #[test]
    fn exec_host_flows_through() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        write_extension(
            &global,
            "exec.luau",
            r#"
            local pillar = require("@pillar")
            __result = pillar.exec("probe")
            return nil
        "#,
        );
        let runtime = create_shared_runtime(Some(Arc::new(
            |command: &str, _: &[String], _: &pillar_extensions_contract::ExecOptions| {
                pillar_extensions_contract::ExecResult {
                    stdout: command.to_string(),
                    stderr: String::new(),
                    code: 0,
                    killed: false,
                }
            },
        )));
        let (extensions, errors) = discover_and_load(&runtime, Some(&global), None);
        assert!(errors.is_empty(), "errors={errors:?}");
        assert_eq!(extensions.len(), 1);
        let vm = runtime.lock().unwrap().vm().clone();
        let result: serde_json::Value = {
            use luaur_rt::LuaSerdeExt;
            let value: luaur_rt::Value = vm.load("return __result").call(()).unwrap();
            vm.from_value(value).unwrap()
        };
        assert_eq!(result["stdout"], "probe");
    }

    /// The luau_module_loader works standalone against the loader
    /// contract: a path load returns the extension, a missing file is
    /// a per-path error.
    #[test]
    fn module_loader_contract() {
        let runtime = create_shared_runtime(None);
        let mut loader = luau_module_loader(&runtime);
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.luau");
        std::fs::write(&good, "return nil").unwrap();
        let outcome = loader(&good.to_string_lossy());
        assert!(matches!(outcome, Ok(Some(_))));
        let outcome = loader("/nonexistent/x.luau");
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
}

impl LuauLoader {
    pub fn new(runtime: SharedRuntime) -> Self {
        Self { runtime }
    }
}

impl LuauExtensionLoader for LuauLoader {
    fn load_extension(
        &self,
        path: &str,
    ) -> LoadOutcome {
        luau_module_loader(&self.runtime)(path)
    }
}

/// Convenience: create the shared runtime plus its coding-agent loader
/// in one call.
pub fn create_luau_loader(
    exec_host: Option<crate::runtime::ExecHost>,
) -> (SharedRuntime, LuauLoader) {
    let runtime = create_shared_runtime(exec_host);
    let loader = LuauLoader::new(Arc::clone(&runtime));
    (runtime, loader)
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    fn write_extension(dir: &std::path::Path, name: &str, body: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().to_string()
    }

    /// The trait loader runs a real Luau extension: type-check -> setup ->
    /// bridge, with registrations surfacing in the contract's extension
    /// (path discovery and the runner build belong to the host; the
    /// coding-agent parity test covers that wiring).
    #[test]
    fn luau_loader_trait_loads_and_bridges_real_extension() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_extension(
            temp.path(),
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
        );

        let (_runtime, loader) = create_luau_loader(None);
        let extension = loader
            .load_extension(&path)
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
        let vm = _runtime.lock().unwrap();
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
        let temp = tempfile::tempdir().unwrap();
        let bad = write_extension(temp.path(), "bad.luau", "local n: number = \"text\"");
        let good = write_extension(temp.path(), "good.luau", "return nil");

        let (_runtime, loader) = create_luau_loader(None);
        let error = match loader.load_extension(&bad) {
            Err(error) => error,
            Ok(_) => panic!("expected a type-check error"),
        };
        assert!(error.contains("type-check failed"), "{error}");
        assert!(loader
            .load_extension(&good)
            .expect("good loads")
            .is_some());
    }

    /// One loader can load several extensions in turn and the shared runtime
    /// stays usable (the host's discovery / cache wiring sits above this).
    #[test]
    fn one_loader_loads_several_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let first = write_extension(
            temp.path(),
            "one.luau",
            r#"
            local pillar = require("@pillar")
            pillar.register_command("one", { description = "First" })
            return nil
            "#,
        );
        let second = write_extension(
            temp.path(),
            "two.luau",
            r#"
            local pillar = require("@pillar")
            pillar.register_command("two", { description = "Second" })
            return nil
            "#,
        );

        let (_runtime, loader) = create_luau_loader(None);
        let first = loader.load_extension(&first).expect("ok").expect("extension");
        let second = loader.load_extension(&second).expect("ok").expect("extension");
        assert!(first.commands.iter().any(|command| command.name == "one"));
        assert!(second.commands.iter().any(|command| command.name == "two"));
    }
}
