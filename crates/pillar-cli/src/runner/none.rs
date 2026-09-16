//! The no-Luau wiring: the same public API as [`super::luau`] with nothing to
//! load or run.
//!
//! Why this exists (docs/DEVELOPMENT-STRATEGY.md §5-3): "Luau present or
//! absent" has to be a build axis, not just a dependency property. With
//! `--no-default-features` the binary compiles and runs without the VM crate,
//! `luaur`, or the extension machinery; the session simply has no extensions,
//! no extension tools, and no `/reload` work to do.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pillar_agent::types::AgentTool;
use pillar_coding_agent::core::agent_session_class::{AgentSession, ExtensionCommandHandler};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::extensions_types::{ExtensionContextFacts, ExtensionUiSlot};

use super::{ExtensionDataSnapshot, ExtensionHostSlots};

/// Without the VM there is no runtime to point at. The type keeps the host
/// call sites (`extension_command_handler(&wiring.runtime)`) `cfg`-free.
#[derive(Clone, Copy, Default)]
pub struct NoRuntime;

/// The wiring the session binds to. It carries the host slots so a rebuild
/// keeps the same shape, but it never loads anything.
pub struct ExtensionWiring {
    /// Always [`NoRuntime`]: nothing to run.
    pub runtime: NoRuntime,
    /// The (empty) runner the session binds.
    pub runner: ExtensionRunner,
    /// Always empty: no extension file is read, so none can fail.
    pub errors: Vec<(String, String)>,
    /// The `ctx.ui` bridge the interactive run would install (nothing calls
    /// back into it without extensions, but the run still wires it).
    pub ui_slot: ExtensionUiSlot,
    pub session_slot: super::SessionSlot,
    pub data: Arc<std::sync::Mutex<ExtensionDataSnapshot>>,
    pub context: Arc<std::sync::Mutex<ExtensionContextFacts>>,
    /// The discovery inputs a rebuild would repeat; kept so the session's
    /// generation-swap closure has the same shape.
    pub rebuild: ExtensionRebuildInputs,
}

/// The inputs an extension rebuild repeats (see [`super::luau`]): with no VM
/// the rebuild is a no-op, but the host still carries them.
#[derive(Clone)]
pub struct ExtensionRebuildInputs {
    pub cwd: String,
    pub global_dir: Option<PathBuf>,
    pub project_dir: Option<PathBuf>,
    pub configured: Vec<String>,
    pub slots: ExtensionHostSlots,
}

impl ExtensionWiring {
    /// Ignore the flags and hand back an empty wiring (upstream `_buildRuntime`
    /// re-reads the extension directories; there are none here).
    pub fn rebuild(
        &self,
        _flag_values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> ExtensionWiring {
        build_extension_runner_with_slots(&self.rebuild.cwd, None, None, &[], &self.rebuild.slots)
    }

    pub fn set_extension_context(&self, facts: ExtensionContextFacts) {
        *self
            .context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = facts;
    }

    /// Move the (empty) runner out, keeping the wiring usable.
    pub fn take_runner(&mut self) -> ExtensionRunner {
        std::mem::replace(&mut self.runner, ExtensionRunner::new(Vec::new()))
    }

    /// No extension can register a tool without the VM.
    pub fn custom_tools(&self) -> Vec<AgentTool> {
        Vec::new()
    }

    pub fn bind_session(&self, session: &Arc<AgentSession>) {
        super::bind_session(&self.session_slot, session);
    }

    /// Nothing to snapshot: no extension provides commands or tools.
    pub fn refresh_extension_data(&self) {}
}

/// Build the (empty) wiring from the host slots.
pub fn build_extension_runner_with_slots(
    cwd: &str,
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
    configured: &[String],
    slots: &ExtensionHostSlots,
) -> ExtensionWiring {
    ExtensionWiring {
        runtime: NoRuntime,
        runner: ExtensionRunner::new(Vec::new()),
        errors: Vec::new(),
        ui_slot: Arc::clone(&slots.ui_slot),
        session_slot: Arc::clone(&slots.session_slot),
        data: Arc::clone(&slots.data),
        context: Arc::clone(&slots.context),
        rebuild: ExtensionRebuildInputs {
            cwd: cwd.to_string(),
            global_dir: global_dir.map(Path::to_path_buf),
            project_dir: project_dir.map(Path::to_path_buf),
            configured: configured.to_vec(),
            slots: slots.clone(),
        },
    }
}

/// [`build_extension_runner_with_slots`] with default slots.
pub fn build_extension_runner(
    cwd: &str,
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
    configured: &[String],
) -> ExtensionWiring {
    build_extension_runner_with_slots(
        cwd,
        global_dir,
        project_dir,
        configured,
        &ExtensionHostSlots::new(cwd),
    )
}

/// Without extensions every slash command is a prompt instead
/// (`Ok(false)` = "not handled"), which is what the session does with an
/// unknown command here.
pub fn extension_command_handler(_runtime: &NoRuntime) -> ExtensionCommandHandler {
    Arc::new(|_name: &str, _args: &str| Ok(false))
}

/// Wrap a pre-built runner in the shared, mutable handle the session takes.
pub fn shared_runner(runner: ExtensionRunner) -> Arc<std::sync::Mutex<ExtensionRunner>> {
    Arc::new(std::sync::Mutex::new(runner))
}
