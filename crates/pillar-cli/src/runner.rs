//! Host wiring between the coding agent and the Luau extension runtime.
//!
//! `pillar-coding-agent` deliberately does not depend on
//! `pillar-extensions` (the latter depends on the former for the runner
//! types, so the edge would cycle). This crate is the top layer that joins
//! them: it owns the Luau VM / loader and produces the [`ExtensionRunner`]
//! the session binds to (upstream the extension construction inside
//! `_buildRuntime`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::Mutex;

use pillar_agent::types::AgentTool;
use pillar_coding_agent::core::agent_session::CustomDelivery;
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, SendCustomMessageOptions, SendUserMessageOptions, StreamingBehavior,
};
use pillar_coding_agent::core::extensions_luau::{build_luau_runner, discover_luau_paths};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::extensions_types::{
    ExtensionContextFacts, ExtensionUiRequest, ExtensionUiSlot,
};
use pillar_extensions::loader::{LuauLoader, SharedRuntime, create_luau_loader};

/// The session handle the `@pillar` host callbacks resolve at call time.
///
/// divergence: the runtime is built before the session exists (the runner
/// must be ready for `createAgentSession`), so the callbacks read the session
/// through this slot instead of capturing it directly. `bind_session` fills it
/// once the caller has the `Arc`.
pub type SessionSlot = Arc<Mutex<Option<Arc<AgentSession>>>>;

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
/// `ctx.ui` bridge, the facts and the command snapshot survive the rebuild.
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
}

impl ExtensionHostSlots {
    pub fn new(cwd: &str) -> Self {
        Self {
            session_slot: Arc::new(Mutex::new(None)),
            data: Arc::new(Mutex::new(ExtensionDataSnapshot::default())),
            ui_slot: Arc::new(Mutex::new(Default::default())),
            context: Arc::new(Mutex::new(ExtensionContextFacts {
                cwd: cwd.to_string(),
                ..Default::default()
            })),
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
    pub fn rebuild(&self, flag_values: &std::collections::BTreeMap<String, serde_json::Value>) -> ExtensionWiring {
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
    install_exec_host(&runtime, cwd);
    install_host_api(&runtime, &session_slot, &data, &ui_slot, &context, cwd);
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

/// Install the `pi.exec` host (upstream the process layer behind `exec`):
/// the command runs with the session's cwd, and the extension receives
/// `{ stdout, stderr, code, killed }`.
fn install_exec_host(runtime: &SharedRuntime, cwd: &str) {
    let cwd = cwd.to_string();
    let exec: pillar_extensions::runtime::ExecHost = Arc::new(move |command, args| {
        let output = std::process::Command::new(command)
            .args(args)
            .current_dir(&cwd)
            .output();
        match output {
            Ok(output) => serde_json::json!({
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "code": output.status.code(),
                "killed": false,
            }),
            Err(error) => serde_json::json!({
                "stdout": "",
                "stderr": error.to_string(),
                "code": -1,
                "killed": false,
            }),
        }
    });
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
    slot: &SessionSlot,
    data: &Arc<Mutex<ExtensionDataSnapshot>>,
    ui_slot: &ExtensionUiSlot,
    context: &Arc<Mutex<ExtensionContextFacts>>,
    cwd: &str,
) {
    use pillar_extensions::runtime::HostApi;

    let session = |slot: &SessionSlot| -> Result<Arc<AgentSession>, String> {
        slot.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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
                    if let Err(error) = session.send_user_message(content, options.as_ref()).await
                    {
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
                session(&slot)
                    .ok()
                    .and_then(|session| {
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
            Some(Arc::new(move || session(&slot).ok().map(|s| s.thinking_level())))
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
                let mut state = slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // No bridge yet (before the run loop starts): the request is
                // queued and replayed when it is installed.
                state.dispatch(request);
                Ok(())
            }))
        },
        // `ctx.sessionManager.getSessionId()` (upstream the session id).
        session_id: {
            let slot = Arc::clone(slot);
            Some(Arc::new(move || {
                let session = slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                session.map(|session| {
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
            Some(Arc::new(move || {
                let session = slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                match session {
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
                }
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
            Some(Arc::new(
                move |op: &str, path: &str, content: Option<&str>| {
                    // Paths resolve against the session cwd, like tool calls.
                    let resolved = if std::path::Path::new(path).is_absolute() {
                        std::path::PathBuf::from(path)
                    } else {
                        std::path::Path::new(&cwd).join(path)
                    };
                    match op {
                        "read" => match std::fs::read_to_string(&resolved) {
                            Ok(text) => Ok(serde_json::Value::String(text)),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                Ok(serde_json::Value::Null)
                            }
                            Err(error) => Err(format!("pillar.fs.read: {error}")),
                        },
                        "write" => {
                            let Some(content) = content else {
                                return Err("pillar.fs.write: missing content".to_string());
                            };
                            if let Some(parent) = resolved.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            std::fs::write(&resolved, content)
                                .map(|_| serde_json::Value::Bool(true))
                                .map_err(|error| format!("pillar.fs.write: {error}"))
                        }
                        "list" => {
                            let entries = std::fs::read_dir(&resolved)
                                .map_err(|error| format!("pillar.fs.list: {error}"))?;
                            let mut names: Vec<String> = entries
                                .filter_map(|entry| entry.ok())
                                .map(|entry| entry.file_name().to_string_lossy().to_string())
                                .collect();
                            names.sort();
                            Ok(serde_json::Value::Array(
                                names.into_iter().map(serde_json::Value::String).collect(),
                            ))
                        }
                        "stat" => match std::fs::metadata(&resolved) {
                            Ok(metadata) => {
                                let modified_ms = metadata
                                    .modified()
                                    .ok()
                                    .and_then(|time| {
                                        time.duration_since(std::time::UNIX_EPOCH).ok()
                                    })
                                    .map(|duration| duration.as_millis() as u64)
                                    .unwrap_or(0);
                                Ok(serde_json::json!({
                                    "type": if metadata.is_dir() { "directory" } else { "file" },
                                    "size": metadata.len(),
                                    "modified_ms": modified_ms,
                                }))
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                Ok(serde_json::Value::Null)
                            }
                            Err(error) => Err(format!("pillar.fs.stat: {error}")),
                        },
                        "exists" => Ok(serde_json::Value::Bool(resolved.exists())),
                        other => Err(format!("pillar.fs: unknown operation {other}")),
                    }
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
    let _ = cwd;
    runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_host_api(api);
}

/// Build the agent custom message from the extension's table (upstream the
/// `sendMessage` payload).
fn custom_message_from_json(
    json: serde_json::Value,
) -> (pillar_agent::types::CustomMessage, Option<SendCustomMessageOptions>) {
    let custom_type = json
        .get("customType")
        .or_else(|| json.get("custom_type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let content = json
        .get("content")
        .and_then(|value| serde_json::from_value::<pillar_ai::types::UserContent>(value.clone()).ok())
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
) -> (pillar_ai::types::UserContent, Option<SendUserMessageOptions>) {
    let content = match &json {
        serde_json::Value::String(text) => pillar_ai::types::UserContent::Text(text.clone()),
        serde_json::Value::Array(blocks) => serde_json::from_value::<pillar_ai::types::UserContent>(
            serde_json::Value::Array(blocks.clone()),
        )
        .unwrap_or_else(|_| pillar_ai::types::UserContent::Text(String::new())),
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
        *self
            .session_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(session));
    }

    /// Refresh the command / tool snapshot the `@pillar` getters answer.
    /// Call it after `bind_extensions` and after a reload — never from inside
    /// an extension handler (the runner lock is held there).
    pub fn refresh_extension_data(&self) {
        let Some(session) = self
            .session_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
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
        *self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ExtensionDataSnapshot {
            commands,
            all_tools,
            active_tools,
        };
    }
}
