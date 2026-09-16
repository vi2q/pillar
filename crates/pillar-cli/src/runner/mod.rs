//! Host wiring between the coding agent and the Luau extension runtime.
//!
//! The Luau side lives behind the `luau` feature (default on): without it the
//! crate builds and runs with no extension runtime at all
//! (docs/DEVELOPMENT-STRATEGY.md §5-3), so a host that does not need
//! extensions does not link the VM. The public API is the same either way.
//!
//! `pillar-coding-agent` deliberately does not depend on `pillar-extensions`;
//! the shared runner types live in `pillar-extensions-contract`, and this crate
//! is the top layer that joins the agent with the VM.

use std::sync::{Arc, Mutex, Weak};

use pillar_coding_agent::core::agent_session_class::AgentSession;
use pillar_coding_agent::core::extensions_types::{ExtensionContextFacts, ExtensionUiSlot};

use crate::effects::EffectBroker;

/// The session handle the `@pillar` host callbacks resolve at call time.
///
/// divergence: the runtime is built before the session exists (the runner
/// must be ready for `createAgentSession`), so the callbacks read the session
/// through this slot instead of capturing it directly. [`bind_session`] fills
/// it once the caller has the `Arc`.
///
/// The slot holds a [`Weak`] reference: the session owns the runner whose
/// callbacks reach this slot, so a strong handle would be a cycle that keeps
/// the session alive for the process lifetime
/// (docs/ARCHITECTURE-REVIEW-s05c0.md D).
pub type SessionSlot = Arc<Mutex<Option<Weak<AgentSession>>>>;

/// Point the slot at the live session (upstream the runtime constructing the
/// ExtensionAPI with the session).
pub fn bind_session(slot: &SessionSlot, session: &Arc<AgentSession>) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(session));
}

/// The session the slot points at: `None` before binding, after the last
/// strong handle was dropped, and after [`AgentSession::dispose`] (the host
/// then reports "not ready" instead of touching a dead session).
pub fn resolve_session(slot: &SessionSlot) -> Option<Arc<AgentSession>> {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(Weak::upgrade)
        .filter(|session| !session.is_disposed())
}

/// The command / tool data `get_commands` and the tool getters answer.
///
/// divergence: upstream reads these straight from the runner, but the port's
/// runner is behind a mutex that is *held* while an extension handler runs, so
/// a handler calling back into it would deadlock. The host refreshes this
/// snapshot (outside dispatch) and the callbacks read only it.
#[derive(Clone, Debug, Default)]
pub struct ExtensionDataSnapshot {
    pub commands: serde_json::Value,
    pub all_tools: serde_json::Value,
    pub active_tools: serde_json::Value,
}

/// The host-side slots a wiring's callbacks resolve through. A `/reload`
/// builds a fresh VM and runner but keeps these, so the session binding, the
/// `ctx.ui` bridge, the facts, the command snapshot and the effect broker
/// survive the rebuild.
#[derive(Clone)]
pub struct ExtensionHostSlots {
    /// The live session the host callbacks resolve (see [`SessionSlot`]).
    pub session_slot: SessionSlot,
    /// Command / tool data for the `@pillar` getters.
    pub data: Arc<Mutex<ExtensionDataSnapshot>>,
    /// The `ctx.ui` bridge: the interactive run installs its pump-backed
    /// sender (upstream the mode owns the extension UI context).
    pub ui_slot: ExtensionUiSlot,
    /// The `ctx` facts the extensions see. The host keeps its own cell
    /// instead of asking the session's runner: a handler runs while the
    /// runner mutex is held, so re-locking it there would deadlock.
    pub context: Arc<Mutex<ExtensionContextFacts>>,
    /// The gate every extension effect passes (see [`crate::effects`]).
    pub broker: Arc<EffectBroker>,
}

impl ExtensionHostSlots {
    pub fn new(cwd: &str) -> Self {
        Self::with_broker(cwd, EffectBroker::permissive())
    }

    pub fn with_broker(cwd: &str, broker: Arc<EffectBroker>) -> Self {
        Self {
            session_slot: Arc::new(Mutex::new(None)),
            data: Arc::new(Mutex::new(ExtensionDataSnapshot::default())),
            ui_slot: Arc::new(Mutex::new(Default::default())),
            context: Arc::new(Mutex::new(ExtensionContextFacts {
                cwd: cwd.to_string(),
                ..Default::default()
            })),
            broker,
        }
    }
}

// The Luau side is optional: both implementations expose the same public API,
// so the rest of the crate (and the binary) needs no `cfg`.
#[cfg(feature = "luau")]
#[path = "luau.rs"]
mod imp;
#[cfg(not(feature = "luau"))]
#[path = "none.rs"]
mod imp;

pub use imp::*;

/// [`ExtensionWiring::refresh_extension_data`] over bare slots, so the reload
/// hook can refresh the host snapshot after the session installed the rebuilt
/// runner (before that the old runner answers and the snapshot would be a
/// generation behind).
pub fn refresh_extension_data_for(
    session_slot: &SessionSlot,
    data: &Arc<Mutex<ExtensionDataSnapshot>>,
) {
    let Some(session) = resolve_session(session_slot) else {
        return;
    };
    let tools = session.state().tools;
    let (commands, owners) = {
        let runner = session.extension_runner_arc();
        let mut runner = runner.lock().expect("runner lock");
        let commands = serde_json::Value::Array(
            runner
                .registered_commands()
                .iter()
                .map(|command| {
                    serde_json::json!({
                        "name": command.invocation_name,
                        "description": command.description,
                        "source": command.source_path,
                    })
                })
                .collect(),
        );
        let owners: Vec<Option<String>> = tools
            .iter()
            .map(|tool| runner.tool_owner(tool.name()))
            .collect();
        (commands, owners)
    };
    let all_tools = serde_json::Value::Array(
        tools
            .iter()
            .zip(owners)
            .map(|(tool, owner)| {
                serde_json::json!({
                    "name": tool.name(),
                    "description": tool.tool.description,
                    "parameters": tool.tool.parameters,
                    "source": owner.unwrap_or_else(|| "builtin".to_string()),
                })
            })
            .collect(),
    );
    let active_tools = serde_json::Value::Array(
        tools
            .iter()
            .map(|tool| serde_json::Value::String(tool.name().to_string()))
            .collect(),
    );
    *data.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = ExtensionDataSnapshot {
        commands,
        all_tools,
        active_tools,
    };
}
