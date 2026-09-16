//! Host wiring between the coding agent and the Luau extension runtime.
//!
//! `pillar-coding-agent` deliberately does not depend on
//! `pillar-extensions` (the latter depends on the former for the runner
//! types, so the edge would cycle). This crate is the top layer that joins
//! them: it owns the Luau VM / loader and produces the [`ExtensionRunner`]
//! the session binds to (upstream the extension construction inside
//! `_buildRuntime`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use pillar_agent::types::AgentTool;
use pillar_coding_agent::core::agent_session::CustomDelivery;
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, ExtensionCommandHandler, SendCustomMessageOptions, SendUserMessageOptions,
    StreamingBehavior,
};
use pillar_coding_agent::core::extensions_luau::{build_luau_runner, discover_luau_paths};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::extensions_types::{
    ExtensionContextFacts, ExtensionUiRequest, ExtensionUiSlot,
};
use pillar_extensions::loader::{LuauLoader, SharedRuntime, create_luau_loader};

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

/// The Luau runtime plus the runner built from the discovered extension
/// files. The runtime must outlive the runner's extension bridges.
pub struct ExtensionWiring {
    pub runtime: SharedRuntime,
    pub loader: LuauLoader,
    pub runner: ExtensionRunner,
    /// `(path, error)` for extensions that failed to load.
    pub errors: Vec<(String, String)>,
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
    /// The discovery inputs a rebuild repeats (upstream `_buildRuntime`
    /// re-runs discovery).
    pub rebuild: ExtensionRebuildInputs,
}

/// Everything a rebuild needs to re-discover and re-load the extensions.
#[derive(Clone)]
pub struct ExtensionRebuildInputs {
    pub cwd: String,
    pub global_dir: Option<PathBuf>,
    pub project_dir: Option<PathBuf>,
    pub configured: Vec<String>,
    pub slots: ExtensionHostSlots,
}

impl ExtensionWiring {
    /// Re-run discovery and loading into a fresh VM, sharing the host slots.
    /// This is the `/reload` path (upstream `_buildRuntime`): the new runner
    /// replaces the session's in place.
    pub fn rebuild(
        &self,
        flag_values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> ExtensionWiring {
        let inputs = self.rebuild.clone();
        let mut wiring = build_extension_runner_with_slots(
            &inputs.cwd,
            inputs.global_dir.as_deref(),
            inputs.project_dir.as_deref(),
            &inputs.configured,
            &inputs.slots,
        );
        for (name, value) in flag_values {
            wiring.runner.set_flag_value(name, value.clone());
        }
        wiring
    }

    /// Publish the `ctx` facts (upstream `bindCore` setting cwd / mode and
    /// `setUIContext` tracking the UI). Called next to
    /// `AgentSession::bind_extensions`.
    pub fn set_extension_context(&self, facts: ExtensionContextFacts) {
        *self
            .context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = facts;
    }
}

/// Discover and load Luau extensions for `cwd` and build the runner.
///
/// `global_dir` is the user extension directory (e.g. `~/.pillar/agent/...`),
/// `project_dir` the project-local one; `configured` are explicit
/// `--extension` paths (files or directories).
///
/// The caller must resolve `project_dir` through
/// [`crate::trust::project_extension_dir`]: loading an extension evaluates
/// arbitrary Luau with `pi.exec` / `pillar.fs` available, so an untrusted
/// checkout must not reach this function.
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

/// [`build_extension_runner`] with caller-owned host slots (a rebuild reuses
/// the running session's slots).
pub fn build_extension_runner_with_slots(
    cwd: &str,
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
    configured: &[String],
    slots: &ExtensionHostSlots,
) -> ExtensionWiring {
    let paths = discover_luau_paths(global_dir, project_dir, configured, cwd);
    let (runtime, loader) = create_luau_loader(None);
    let session_slot = Arc::clone(&slots.session_slot);
    let data = Arc::clone(&slots.data);
    let ui_slot = Arc::clone(&slots.ui_slot);
    let context = Arc::clone(&slots.context);
    // The host callbacks must exist before the extension factories run: a
    // factory may already call `pillar.fs` / `pillar.get_flag` / `ctx.ui`.
    install_exec_host(&runtime, &slots.broker, cwd);
    // `global_dir` is `<agent dir>/extensions`, so its parent is the agent
    // directory the custom-UI keybinding lookup reads `keybindings.json` from.
    install_host_api(&runtime, slots, cwd, global_dir.and_then(Path::parent));
    let (runner, errors) = build_luau_runner(&paths, cwd, &loader, true);
    ExtensionWiring {
        runtime,
        loader,
        runner,
        errors,
        session_slot,
        data,
        ui_slot,
        context,
        rebuild: ExtensionRebuildInputs {
            cwd: cwd.to_string(),
            global_dir: global_dir.map(Path::to_path_buf),
            project_dir: project_dir.map(Path::to_path_buf),
            configured: configured.to_vec(),
            slots: slots.clone(),
        },
    }
}

/// The host command handler (upstream the runner invoking a registered
/// command's handler): the session resolves the command and calls this with
/// the name and the argument text. `Ok(true)` means "handled, don't prompt".
pub fn extension_command_handler(runtime: &SharedRuntime) -> ExtensionCommandHandler {
    let runtime = Arc::clone(runtime);
    Arc::new(move |name: &str, args: &str| {
        let mut runtime = runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `Ok(false)` for an unknown command: the session treats the text as a
        // normal prompt then. The runner disambiguates same-named commands as
        // `/name:1`, `/name:2`, ... — the suffix picks the extension that
        // registered it instead of the last one winning (a command whose own
        // name ends in `:<digits>` is shadowed by that convention, as upstream).
        if let Some((base, occurrence)) = split_command_occurrence(name) {
            return runtime.call_command_at(base, occurrence, args);
        }
        runtime.call_command(name, args)
    })
}

/// `"hello:2"` → `Some(("hello", 1))`: the occurrence reference the runner
/// builds for a duplicated command name. A name without a numeric suffix is
/// not one.
fn split_command_occurrence(name: &str) -> Option<(&str, usize)> {
    let (base, suffix) = name.rsplit_once(':')?;
    let occurrence: usize = suffix.parse().ok()?;
    (!base.is_empty() && occurrence >= 1).then(|| (base, occurrence - 1))
}

/// Install the `pi.exec` host (upstream the process layer behind `exec`):
/// the command runs with the session's cwd, and the extension receives
/// `{ stdout, stderr, code, killed }`. The broker authorizes and executes
/// (never call the process API from here).
fn install_exec_host(runtime: &SharedRuntime, broker: &Arc<EffectBroker>, cwd: &str) {
    let cwd = cwd.to_string();
    let broker = Arc::clone(broker);
    let exec: pillar_extensions::runtime::ExecHost =
        Arc::new(move |command, args, options| broker.exec(&cwd, command, args, options));
    runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_exec_host(exec);
}

/// Install the `@pillar` host callbacks (upstream the runtime binding the
/// ExtensionAPI to the session): every callback resolves the live session
/// through `slot` at call time, so extensions loaded before the session exists
/// still work once [`ExtensionWiring::bind_session`] runs.
fn install_host_api(
    runtime: &SharedRuntime,
    slots: &ExtensionHostSlots,
    cwd: &str,
    agent_dir: Option<&Path>,
) {
    let slot = &slots.session_slot;
    let data = &slots.data;
    let ui_slot = &slots.ui_slot;
    let context = &slots.context;
    let broker = &slots.broker;
    use pillar_extensions::runtime::HostApi;

    let session = |slot: &SessionSlot| -> Result<Arc<AgentSession>, String> {
        resolve_session(slot)
            .ok_or_else(|| "extension API: the session is not ready yet".to_string())
    };
    let tokio_handle = tokio::runtime::Handle::try_current().ok();

    let api = HostApi {
        get_flag: Some(Arc::new(|_name: &str| None)),
        append_entry: {
            let slot = Arc::clone(slot);
            Some(Arc::new(
                move |custom_type: &str, data: Option<serde_json::Value>| {
                    let session = session(&slot)?;
                    session
                        .session_manager()
                        .lock()
                        .expect("session lock")
                        .append_custom_entry(custom_type, data)
                        .map(|_| ())
                },
            ))
        },
        send_message: {
            let slot = Arc::clone(slot);
            let handle = tokio_handle.clone();
            Some(Arc::new(move |json: serde_json::Value| {
                let session = session(&slot)?;
                let Some(handle) = handle.clone() else {
                    return Err("extension API: no async runtime for send_message".to_string());
                };
                let (message, options) = custom_message_from_json(json);
                handle.spawn(async move {
                    if let Err(error) = session.send_custom_message(message, options.as_ref()).await
                    {
                        eprintln!("extension send_message failed: {error}");
                    }
                });
                Ok(())
            }))
        },
        send_user_message: {
            let slot = Arc::clone(slot);
            let handle = tokio_handle.clone();
            Some(Arc::new(move |json: serde_json::Value| {
                let session = session(&slot)?;
                let Some(handle) = handle.clone() else {
                    return Err("extension API: no async runtime for send_user_message".to_string());
                };
                let (content, options) = user_message_from_json(json);
                handle.spawn(async move {
                    if let Err(error) = session.send_user_message(content, options.as_ref()).await {
                        eprintln!("extension send_user_message failed: {error}");
                    }
                });
                Ok(())
            }))
        },
        set_session_name: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move |name: &str| {
                session(&slot)?.set_session_name(name)
            }))
        },
        get_session_name: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || {
                session(&slot).ok().and_then(|session| {
                    session
                        .session_manager()
                        .lock()
                        .expect("session lock")
                        .session_name()
                })
            }))
        },
        set_label: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move |entry_id: &str, label: Option<&str>| {
                session(&slot)?
                    .session_manager()
                    .lock()
                    .expect("session lock")
                    .append_label_change(entry_id, label)
                    .map(|_| ())
            }))
        },
        get_commands: {
            let data = Arc::clone(data);
            Some(Arc::new(move || {
                data.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .commands
                    .clone()
            }))
        },
        get_active_tools: {
            let data = Arc::clone(data);
            Some(Arc::new(move || {
                data.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .active_tools
                    .clone()
            }))
        },
        get_all_tools: {
            let data = Arc::clone(data);
            Some(Arc::new(move || {
                data.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .all_tools
                    .clone()
            }))
        },
        get_thinking_level: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || {
                session(&slot).ok().map(|s| s.thinking_level())
            }))
        },
        set_thinking_level: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move |level: &str| {
                session(&slot)?.set_thinking_level(level, false);
                Ok(())
            }))
        },
        set_model: {
            let slot = Arc::clone(slot);
            let handle = tokio_handle.clone();
            Some(Arc::new(move |json: serde_json::Value| {
                let session = session(&slot)?;
                let provider = json
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = json
                    .get("id")
                    .or_else(|| json.get("model"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if provider.is_empty() || id.is_empty() {
                    return Err(
                        "extension set_model: expected { provider = ..., id = ... }".to_string()
                    );
                }
                let Some(handle) = handle.clone() else {
                    return Err("extension API: no async runtime for set_model".to_string());
                };
                handle.spawn(async move {
                    match session.model_runtime().get_model(&provider, &id) {
                        Some(model) => {
                            if let Err(error) = session.set_model(model, false).await {
                                eprintln!("extension set_model failed: {error}");
                            }
                        }
                        None => eprintln!("extension set_model: unknown model {provider}/{id}"),
                    }
                });
                Ok(())
            }))
        },
        // `ctx.ui.*` (upstream the mode's ExtensionUIContext): the request
        // goes to whatever bridge the interactive run installed; without one
        // it is the upstream no-op.
        ui: {
            let slot = Arc::clone(ui_slot);
            Some(Arc::new(move |request: ExtensionUiRequest| {
                let mut state = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // No bridge yet (before the run loop starts): the request is
                // queued and replayed when it is installed.
                state.dispatch(request);
                Ok(())
            }))
        },
        // `ctx.ui.confirm(...)` and friends: the interactive run's dialog
        // bridge; without it the answer is the upstream no-op default.
        ui_ask: {
            let slot = Arc::clone(ui_slot);
            Some(Arc::new(move |request: ExtensionUiRequest| {
                let ask = {
                    let guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.ask.clone()
                };
                match ask {
                    Some(ask) => ask(request),
                    None => Ok(serde_json::Value::Bool(false)),
                }
            }))
        },
        // `ctx.ui.custom(...)`: the interactive run's installer; without one
        // the surface is queued until the run loop starts.
        ui_custom: {
            let slot = Arc::clone(ui_slot);
            Some(Arc::new(
                move |surface: pillar_coding_agent::core::extensions_types::ExtensionCustomSurface| {
                    let mut state = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.install_custom(surface);
                    Ok(())
                },
            ))
        },
        // `keybindings.matches(data, name)` for a `ctx.ui.custom` factory:
        // the manager is built once from the agent directory's
        // `keybindings.json` (upstream passes the live KeybindingsManager).
        keybindings_match: {
            let manager: Arc<
                Mutex<Option<Arc<pillar_coding_agent::core::keybindings::KeybindingsManager>>>,
            > = Arc::new(Mutex::new(None));
            let agent_dir = agent_dir.map(Path::to_path_buf);
            Some(Arc::new(move |data: &str, name: &str| {
                let mut cached = manager
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let manager = cached.get_or_insert_with(|| {
                    Arc::new(pillar_coding_agent::core::keybindings::KeybindingsManager::create(
                        agent_dir.as_deref().unwrap_or(Path::new(".")),
                    ))
                });
                manager.matches(data, name)
            }))
        },
        // `ctx.ui.theme`: the presentation adapter over the live coding-agent
        // theme (the VM reads the snapshot through the contract provider).
        theme: Some(pillar_coding_agent::modes::interactive::theme::contract_provider()),
        // `ctx.isIdle()` (upstream the session's `isIdle`).
        is_idle: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || {
                resolve_session(&slot)
                    .map(|session| session.is_idle())
                    .unwrap_or(true)
            }))
        },
        // `ctx.sessionManager.getSessionId()` (upstream the session id).
        session_id: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || {
                resolve_session(&slot).map(|session| {
                    session
                        .session_manager()
                        .lock()
                        .expect("session")
                        .session_id()
                        .to_string()
                })
            }))
        },
        // `ctx.sessionManager.getEntries()` (upstream the read-only session
        // manager): the JSONL shapes, which is what extensions scan.
        session_entries: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || match resolve_session(&slot) {
                Some(session) => {
                    let entries = session
                        .session_manager()
                        .lock()
                        .expect("session")
                        .get_entries_owned();
                    serde_json::Value::Array(
                        entries
                            .iter()
                            .map(pillar_coding_agent::core::session_manager::entry_to_json)
                            .collect(),
                    )
                }
                None => serde_json::Value::Array(Vec::new()),
            }))
        },
        // The `ctx` facts (upstream the live ExtensionContext fields): the
        // host's own cell, published next to `bind_extensions`.
        context: {
            let facts = Arc::clone(context);
            Some(Arc::new(move || {
                facts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            }))
        },
        fs: {
            let cwd = cwd.to_string();
            let broker = Arc::clone(broker);
            // The broker resolves the path, authorizes it and only then
            // touches the filesystem (never call the fs API from here).
            Some(Arc::new(
                move |op: &str, path: &str, content: Option<&str>| {
                    broker.fs(&cwd, op, path, content)
                },
            ))
        },
        set_active_tools: {
            let slot = Arc::clone(slot);
            let data = Arc::clone(data);
            Some(Arc::new(move |names: serde_json::Value| {
                let session = session(&slot)?;
                let names: Vec<String> = serde_json::from_value(names)
                    .map_err(|error| format!("extension set_active_tools: {error}"))?;
                // Reuse the tools already built for the session (builtin and
                // extension) and fall back to the builtin factory for names
                // that are not currently active.
                let current = session.state().tools;
                let mut tools = Vec::new();
                for name in &names {
                    if let Some(tool) = current.iter().find(|tool| tool.name() == name).cloned() {
                        tools.push(tool);
                    } else if let Some(tool) =
                        pillar_coding_agent::core::tools::index::create_tool(name, session.cwd())
                    {
                        tools.push(tool);
                    }
                }
                session.agent().set_tools(tools);
                // The active-tool getter answers the snapshot; keep it in
                // sync without re-entering the runner.
                data.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .active_tools = serde_json::Value::Array(
                    names
                        .iter()
                        .map(|name| serde_json::Value::String(name.clone()))
                        .collect(),
                );
                Ok(())
            }))
        },
    };
    runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_host_api(api);
}

/// Build the agent custom message from the extension's table (upstream the
/// `sendMessage` payload).
fn custom_message_from_json(
    json: serde_json::Value,
) -> (
    pillar_agent::types::CustomMessage,
    Option<SendCustomMessageOptions>,
) {
    let custom_type = json
        .get("customType")
        .or_else(|| json.get("custom_type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let content = json
        .get("content")
        .and_then(|value| {
            serde_json::from_value::<pillar_ai::types::UserContent>(value.clone()).ok()
        })
        .unwrap_or_else(|| pillar_ai::types::UserContent::Text(String::new()));
    let display = json
        .get("display")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let details = json.get("details").cloned();
    let options = json.get("options").and_then(|options| {
        let deliver_as = options
            .get("deliverAs")
            .or_else(|| options.get("deliver_as"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| match value {
                "steer" => Some(CustomDelivery::Steer),
                "followUp" => Some(CustomDelivery::FollowUp),
                "nextTurn" => Some(CustomDelivery::NextTurn),
                _ => None,
            });
        let trigger_turn = options
            .get("triggerTurn")
            .or_else(|| options.get("trigger_turn"))
            .and_then(serde_json::Value::as_bool);
        (deliver_as.is_some() || trigger_turn.is_some()).then_some(SendCustomMessageOptions {
            trigger_turn,
            deliver_as,
        })
    });
    (
        pillar_agent::types::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp: pillar_ai::models::now_ms(),
        },
        options,
    )
}

/// Build the user message from the extension's table (upstream the
/// `sendUserMessage` payload: a string or content blocks, plus options).
fn user_message_from_json(
    json: serde_json::Value,
) -> (
    pillar_ai::types::UserContent,
    Option<SendUserMessageOptions>,
) {
    let content = match &json {
        serde_json::Value::String(text) => pillar_ai::types::UserContent::Text(text.clone()),
        serde_json::Value::Array(blocks) => {
            serde_json::from_value::<pillar_ai::types::UserContent>(serde_json::Value::Array(
                blocks.clone(),
            ))
            .unwrap_or_else(|_| pillar_ai::types::UserContent::Text(String::new()))
        }
        other => other
            .get("content")
            .and_then(|value| {
                serde_json::from_value::<pillar_ai::types::UserContent>(value.clone()).ok()
            })
            .unwrap_or_else(|| pillar_ai::types::UserContent::Text(String::new())),
    };
    let options = json.get("options").and_then(|options| {
        let deliver_as = options
            .get("deliverAs")
            .or_else(|| options.get("deliver_as"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| match value {
                "steer" => Some(StreamingBehavior::Steer),
                "followUp" => Some(StreamingBehavior::FollowUp),
                _ => None,
            });
        let expand_prompt_templates = options
            .get("expandPromptTemplates")
            .or_else(|| options.get("expand_prompt_templates"))
            .and_then(serde_json::Value::as_bool);
        (deliver_as.is_some() || expand_prompt_templates.is_some()).then_some(
            SendUserMessageOptions {
                deliver_as,
                expand_prompt_templates,
            },
        )
    });
    (content, options)
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

    /// Bind the live session the `@pillar` host callbacks resolve (upstream
    /// the runtime constructing the ExtensionAPI with the session).
    pub fn bind_session(&self, session: &Arc<AgentSession>) {
        bind_session(&self.session_slot, session);
    }

    /// Refresh the command / tool snapshot the `@pillar` getters answer.
    /// Call it after `bind_extensions` and after a reload — never from inside
    /// an extension handler (the runner lock is held there).
    pub fn refresh_extension_data(&self) {
        refresh_extension_data_for(&self.session_slot, &self.data);
    }
}

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
