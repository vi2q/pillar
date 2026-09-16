//! Host wiring between the coding agent and the Luau extension runtime.
//!
//! `pillar-coding-agent` deliberately does not depend on
//! `pillar-extensions` (the latter depends on the former for the runner
//! types, so the edge would cycle). This crate is the top layer that joins
//! them: it owns the Luau VM / loader and produces the [`ExtensionRunner`]
//! the session binds to (upstream the extension construction inside
//! `_buildRuntime`).

use std::path::Path;
use std::sync::Arc;

use pillar_agent::types::AgentTool;
use pillar_coding_agent::core::extensions_luau::{build_luau_runner, discover_luau_paths};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_extensions::loader::{LuauLoader, SharedRuntime, create_luau_loader};

/// The Luau runtime plus the runner built from the discovered extension
/// files. The runtime must outlive the runner's extension bridges.
pub struct ExtensionWiring {
    pub runtime: SharedRuntime,
    pub loader: LuauLoader,
    pub runner: ExtensionRunner,
    /// `(path, error)` for extensions that failed to load.
    pub errors: Vec<(String, String)>,
}

/// Discover and load Luau extensions for `cwd` and build the runner.
///
/// `global_dir` is the user extension directory (e.g. `~/.pillar/agent/...`),
/// `project_dir` the project-local one; `configured` are explicit
/// `--extension` paths (files or directories).
pub fn build_extension_runner(
    cwd: &str,
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
    configured: &[String],
) -> ExtensionWiring {
    let paths = discover_luau_paths(global_dir, project_dir, configured, cwd);
    let (runtime, loader) = create_luau_loader(None);
    let (runner, errors) = build_luau_runner(&paths, cwd, &loader, true);
    ExtensionWiring {
        runtime,
        loader,
        runner,
        errors,
    }
}

/// Wrap a pre-built runner in the shared, mutable handle the session takes.
pub fn shared_runner(runner: ExtensionRunner) -> Arc<std::sync::Mutex<ExtensionRunner>> {
    Arc::new(std::sync::Mutex::new(runner))
}

impl ExtensionWiring {
    /// Move the built runner out while keeping the runtime/loader alive in
    /// the wiring value.
    pub fn take_runner(&mut self) -> ExtensionRunner {
        std::mem::replace(&mut self.runner, ExtensionRunner::new(Vec::new()))
    }

    /// The callable agent tools the loaded extensions registered (upstream
    /// the runner adding the extension tools to the session's tool set).
    /// Built from the runtime, so it must be called after the setup pass.
    pub fn custom_tools(&self) -> Vec<AgentTool> {
        pillar_extensions::bridge::bridge_to_agent_tools(&self.runtime)
    }
}
