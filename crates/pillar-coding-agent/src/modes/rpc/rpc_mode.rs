//! Port of packages/coding-agent/src/modes/rpc/rpc-mode.ts (pi v0.84.3):
//! headless JSON-lines RPC over stdin/stdout. Every `RpcCommand` is handled;
//! session replacement (`new_session` / `switch_session` / `fork` / `clone`)
//! needs a [`RpcRuntimeHost`] and reports an explicit error when the host
//! configured none. Extension UI requests are host-rendered (no UI bridge
//! here), so `extension_ui_response` lines are accepted and ignored.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::core::agent_session_class::{
    AgentSession, ExtensionBindings, PromptOptions, StreamingBehavior,
};
use crate::core::messages::CodingAgentMessage;
use crate::core::model_mutation::CycleDirection;
use crate::core::session_entries::SessionEntry;
use crate::core::source_info::{
    SyntheticSourceOptions, create_synthetic_source_info, source_info_to_json,
};
use crate::core::usage_totals::UsageTotals;
use crate::modes::json_event::{compaction_result_to_json, to_json_event};
use crate::modes::rpc::rpc_types::{
    RpcCommand, RpcCommandEnvelope, RpcExtensionUiResponse, RpcResponse, RpcSessionState,
};

/// A replacement request outcome reported by the host (upstream
/// `{ cancelled, selectedText? }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReplacement {
    pub cancelled: bool,
    pub selected_text: Option<String>,
}

/// The runtime host the RPC mode drives for session replacement (upstream
/// `runtimeHost`): `newSession` / `switchSession` / `fork` / `dispose`.
///
/// The host owns session construction and teardown; the mode only swaps the
/// session it is bound to and re-subscribes its event sink. Returning `Ok`
/// with no session means the host produced nothing (treat like cancelled).
#[async_trait::async_trait]
pub trait RpcRuntimeHost: Send + Sync {
    /// Start a new session (upstream `newSession`).
    async fn new_session(
        &self,
        parent_session: Option<&str>,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String>;

    /// Switch to an existing session file (upstream `switchSession`).
    async fn switch_session(
        &self,
        session_path: &str,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String>;

    /// Fork from an entry (upstream `fork`). `position` is `"before"` or
    /// `"at"` (clone).
    async fn fork(
        &self,
        entry_id: &str,
        position: &str,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String>;
}

/// The RPC session host: the current session plus the stdout sink.
pub struct RpcMode {
    session: Mutex<Arc<AgentSession>>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    unsubscribe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    host: Option<Arc<dyn RpcRuntimeHost>>,
}

impl RpcMode {
    /// Create the mode and subscribe to session events (written as JSON
    /// lines, upstream `session.subscribe` in `runRpcMode`).
    pub fn new(session: Arc<AgentSession>, out: Arc<Mutex<Box<dyn Write + Send>>>) -> Self {
        Self::new_with_host(session, out, None)
    }

    /// Create the mode with a runtime host able to replace the session.
    pub fn new_with_host(
        session: Arc<AgentSession>,
        out: Arc<Mutex<Box<dyn Write + Send>>>,
        host: Option<Arc<dyn RpcRuntimeHost>>,
    ) -> Self {
        let mode = Self {
            session: Mutex::new(Arc::clone(&session)),
            out,
            unsubscribe: Mutex::new(None),
            host,
        };
        mode.subscribe_session(&session);
        mode
    }

    /// The session commands currently run against.
    fn current_session(&self) -> Arc<AgentSession> {
        Arc::clone(&self.session.lock().expect("session lock"))
    }

    /// The event sink written as JSON lines for every session event.
    fn event_listener(&self) -> crate::core::agent_session_class::AgentSessionEventListener {
        let sink = Arc::clone(&self.out);
        Arc::new(move |event| {
            if let Ok(value) = to_json_event(event)
                && let Ok(mut writer) = sink.lock()
            {
                let _ = writeln!(writer, "{value}");
            }
        })
    }

    fn subscribe_session(&self, session: &Arc<AgentSession>) {
        let unsubscribe = session.subscribe(self.event_listener());
        let previous = self
            .unsubscribe
            .lock()
            .expect("unsubscribe lock")
            .replace(unsubscribe);
        if let Some(previous) = previous {
            previous();
        }
    }

    /// Bind to a replacement session (upstream `rebindSession`): drop the
    /// old listeners, follow the new session's events, and re-bind the
    /// extension UI mode.
    async fn rebind_session(&self, session: Arc<AgentSession>) {
        self.subscribe_session(&session);
        *self.session.lock().expect("session lock") = Arc::clone(&session);
        session
            .bind_extensions(ExtensionBindings {
                ui_context: Some(false),
                mode: Some("rpc".to_string()),
                on_error: None,
            })
            .await;
    }

    /// Write one response line (upstream `writeResponse`).
    pub fn write_response(&self, response: &RpcResponse) {
        if let Ok(value) = serde_json::to_value(response)
            && let Ok(mut writer) = self.out.lock()
        {
            let _ = writeln!(writer, "{value}");
        }
    }

    /// Dispatch one command and return its response (upstream the command
    /// switch in `runRpcMode`).
    pub async fn handle_command(&self, envelope: RpcCommandEnvelope) -> RpcResponse {
        let session = self.current_session();
        let id = envelope.id.clone();
        let command = command_name(&envelope.command);
        match envelope.command {
            RpcCommand::Prompt {
                message,
                streaming_behavior,
                ..
            } => {
                let behavior = streaming_behavior.as_deref().and_then(parse_behavior);
                match session
                    .prompt(
                        &message,
                        Some(&PromptOptions {
                            streaming_behavior: behavior,
                            ..Default::default()
                        }),
                    )
                    .await
                {
                    Ok(()) => RpcResponse::success(id, command, None),
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::Steer { message, .. } => match session.steer(&message, None).await {
                Ok(()) => RpcResponse::success(id, command, None),
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::FollowUp { message, .. } => match session.follow_up(&message, None).await {
                Ok(()) => RpcResponse::success(id, command, None),
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::Abort => {
                session.abort().await;
                RpcResponse::success(id, command, None)
            }
            RpcCommand::ClearQueue => {
                let (steering, follow_up) = session.clear_queue();
                RpcResponse::success(
                    id,
                    command,
                    Some(json!({ "steering": steering, "followUp": follow_up })),
                )
            }
            RpcCommand::GetState => RpcResponse::success(id, command, Some(self.session_state())),
            RpcCommand::SetModel { provider, model_id } => {
                match session.model_runtime().get_model(&provider, &model_id) {
                    Some(model) => match session.set_model(model.clone(), false).await {
                        Ok(()) => RpcResponse::success(
                            id,
                            command,
                            Some(serde_json::to_value(model).unwrap_or(Value::Null)),
                        ),
                        Err(error) => RpcResponse::failure(id, command, error),
                    },
                    None => RpcResponse::failure(
                        id,
                        command,
                        format!("Unknown model: {provider}/{model_id}"),
                    ),
                }
            }
            RpcCommand::GetAvailableModels => {
                let models: Vec<Value> = session
                    .model_runtime()
                    .get_available_snapshot()
                    .iter()
                    .map(|model| serde_json::to_value(model).unwrap_or(Value::Null))
                    .collect();
                RpcResponse::success(id, command, Some(json!({ "models": models })))
            }
            RpcCommand::SetThinkingLevel { level } => {
                session.set_thinking_level(&level, false);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::GetAvailableThinkingLevels => RpcResponse::success(
                id,
                command,
                Some(json!({ "levels": session.available_thinking_levels() })),
            ),
            RpcCommand::CycleModel => match session.cycle_model(CycleDirection::Forward).await {
                Ok(Some(outcome)) => RpcResponse::success(
                    id,
                    command,
                    Some(json!({
                        "model": serde_json::to_value(&outcome.model).unwrap_or(Value::Null),
                        "thinkingLevel": outcome.thinking_level,
                        "isScoped": outcome.is_scoped,
                    })),
                ),
                Ok(None) => RpcResponse::success(id, command, Some(Value::Null)),
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::CycleThinkingLevel => match session.cycle_thinking_level() {
                Some(level) => RpcResponse::success(id, command, Some(json!({ "level": level }))),
                None => RpcResponse::success(id, command, Some(Value::Null)),
            },
            RpcCommand::SetSteeringMode { mode } => {
                session.set_steering_mode(parse_queue_mode(&mode));
                self.sync_queue_modes();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetFollowUpMode { mode } => {
                session.set_follow_up_mode(parse_queue_mode(&mode));
                self.sync_queue_modes();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetAutoCompaction { enabled } => {
                session.set_auto_compaction_enabled(enabled);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetAutoRetry { enabled } => {
                session.set_auto_retry_enabled(enabled);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::AbortRetry => {
                session.abort_retry();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::Compact {
                custom_instructions,
            } => match session.compact(custom_instructions.as_deref()).await {
                Ok(result) => {
                    RpcResponse::success(id, command, Some(compaction_result_to_json(&result)))
                }
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::Bash {
                command: bash_command,
                exclude_from_context,
            } => {
                let exclude = exclude_from_context.unwrap_or(false);
                let event = json!({
                    "type": "user_bash",
                    "command": bash_command,
                    "excludeFromContext": exclude,
                    "cwd": session.cwd(),
                });
                // Extensions may handle the command themselves.
                let event_result = session.emit_user_bash(&event);
                if let Some(result) = event_result.as_ref().and_then(|value| value.get("result")) {
                    let bash_result = bash_result_from_json(result);
                    session.record_bash_result(&bash_command, &bash_result, exclude);
                    return RpcResponse::success(
                        id,
                        command,
                        Some(bash_result_to_json(&bash_result)),
                    );
                }
                match session
                    .execute_bash(&bash_command, exclude, id.as_deref())
                    .await
                {
                    Ok(result) => {
                        RpcResponse::success(id, command, Some(bash_result_to_json(&result)))
                    }
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::AbortBash => {
                session.abort_bash();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::NewSession { parent_session } => {
                let Some(host) = self.host.clone() else {
                    return RpcResponse::failure(
                        id,
                        command,
                        "new_session: no runtime host configured",
                    );
                };
                match host.new_session(parent_session.as_deref()).await {
                    Ok((outcome, replacement)) => {
                        if let Some(replacement) = replacement {
                            self.rebind_session(replacement).await;
                        }
                        RpcResponse::success(
                            id,
                            command,
                            Some(json!({ "cancelled": outcome.cancelled })),
                        )
                    }
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::SwitchSession { session_path } => {
                let Some(host) = self.host.clone() else {
                    return RpcResponse::failure(
                        id,
                        command,
                        "switch_session: no runtime host configured",
                    );
                };
                match host.switch_session(&session_path).await {
                    Ok((outcome, replacement)) => {
                        if let Some(replacement) = replacement {
                            self.rebind_session(replacement).await;
                        }
                        RpcResponse::success(
                            id,
                            command,
                            Some(json!({ "cancelled": outcome.cancelled })),
                        )
                    }
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::Fork { entry_id } => {
                self.fork_response(id, command, &entry_id, "before", false)
                    .await
            }
            RpcCommand::Clone => {
                let leaf_id = session
                    .session_manager()
                    .lock()
                    .expect("session lock")
                    .get_leaf_id()
                    .map(str::to_string);
                let Some(leaf_id) = leaf_id else {
                    return RpcResponse::failure(
                        id,
                        command,
                        "Cannot clone session: no current entry selected",
                    );
                };
                self.fork_response(id, command, &leaf_id, "at", true).await
            }
            RpcCommand::GetSessionStats => {
                RpcResponse::success(id, command, Some(self.session_stats()))
            }
            RpcCommand::GetEntries { since } => {
                let (entries, leaf_id) = {
                    let session_manager = session.session_manager().lock().expect("session lock");
                    (
                        session_manager.get_entries_owned(),
                        session_manager.get_leaf_id().map(str::to_string),
                    )
                };
                let entries = match &since {
                    Some(since) => match entries.iter().position(|entry| entry.id() == since) {
                        Some(index) => entries.into_iter().skip(index + 1).collect(),
                        None => {
                            return RpcResponse::failure(
                                id,
                                command,
                                format!("Entry not found: {since}"),
                            );
                        }
                    },
                    None => entries,
                };
                let entries: Vec<Value> = entries
                    .iter()
                    .map(crate::core::session_manager::entry_to_json)
                    .collect();
                RpcResponse::success(
                    id,
                    command,
                    Some(json!({ "entries": entries, "leafId": leaf_id })),
                )
            }
            RpcCommand::GetTree => {
                let (tree, leaf_id) = {
                    let session_manager = session.session_manager().lock().expect("session lock");
                    (
                        session_manager.get_tree(),
                        session_manager.get_leaf_id().map(str::to_string),
                    )
                };
                let tree: Vec<Value> = tree
                    .iter()
                    .map(crate::core::session_manager::tree_node_to_json)
                    .collect();
                RpcResponse::success(
                    id,
                    command,
                    Some(json!({ "tree": tree, "leafId": leaf_id })),
                )
            }
            RpcCommand::GetLastAssistantText => RpcResponse::success(
                id,
                command,
                Some(json!({ "text": session.last_assistant_text() })),
            ),
            RpcCommand::SetSessionName { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return RpcResponse::failure(id, command, "Session name cannot be empty");
                }
                match session.set_session_name(name) {
                    Ok(()) => RpcResponse::success(id, command, None),
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::GetForkMessages => {
                let messages: Vec<Value> = session
                    .user_messages_for_forking()
                    .into_iter()
                    .map(|(entry_id, text)| json!({ "entryId": entry_id, "text": text }))
                    .collect();
                RpcResponse::success(id, command, Some(json!({ "messages": messages })))
            }
            RpcCommand::GetCommands => RpcResponse::success(id, command, Some(self.commands())),
            RpcCommand::ExportHtml { output_path } => {
                let resolved = output_path.as_ref().map(Path::new);
                match session.export_to_html(resolved) {
                    Ok(path) => RpcResponse::success(
                        id,
                        command,
                        Some(json!({ "path": path.to_string_lossy() })),
                    ),
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::GetMessages => {
                let messages: Vec<Value> = session
                    .state()
                    .messages
                    .iter()
                    .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
                    .collect();
                RpcResponse::success(id, command, Some(json!({ "messages": messages })))
            }
        }
    }

    fn sync_queue_modes(&self) {
        let session = self.current_session();
        session.sync_queue_modes_from_settings();
    }

    /// Drive a fork/clone through the host and shape the response
    /// (upstream the `fork` / `clone` arms).
    async fn fork_response(
        &self,
        id: Option<String>,
        command: String,
        entry_id: &str,
        position: &str,
        clone: bool,
    ) -> RpcResponse {
        let Some(host) = self.host.clone() else {
            return RpcResponse::failure(
                id,
                command,
                format!(
                    "{}: no runtime host configured",
                    if clone { "clone" } else { "fork" }
                ),
            );
        };
        match host.fork(entry_id, position).await {
            Ok((outcome, replacement)) => {
                if let Some(replacement) = replacement {
                    self.rebind_session(replacement).await;
                }
                let data = if clone {
                    json!({ "cancelled": outcome.cancelled })
                } else {
                    json!({ "text": outcome.selected_text, "cancelled": outcome.cancelled })
                };
                RpcResponse::success(id, command, Some(data))
            }
            Err(error) => RpcResponse::failure(id, command, error),
        }
    }

    /// Upstream `get_commands`: extension commands, prompt templates, and
    /// skills available for invocation via prompt.
    ///
    /// divergence: extension commands carry a synthetic `sourceInfo` built
    /// from the extension command's source path (the port's
    /// `RegisteredCommand` does not record the loader metadata).
    fn commands(&self) -> Value {
        let session = self.current_session();
        let mut commands: Vec<Value> = Vec::new();
        for command in session.registered_commands() {
            let source_info = create_synthetic_source_info(
                &command.source_path,
                SyntheticSourceOptions {
                    source: "extension".to_string(),
                    scope: None,
                    origin: None,
                    base_dir: None,
                },
            );
            commands.push(json!({
                "name": command.invocation_name,
                "description": command.description,
                "source": "extension",
                "sourceInfo": source_info_to_json(&source_info),
            }));
        }
        for template in session.prompt_templates() {
            commands.push(json!({
                "name": template.name,
                "description": template.description,
                "source": "prompt",
                "sourceInfo": source_info_to_json(&template.source_info),
            }));
        }
        for skill in session.skills() {
            commands.push(json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": source_info_to_json(&skill.source_info),
            }));
        }
        json!({ "commands": commands })
    }

    /// Upstream `session.getSessionStats()`: entry counts, tool-call count,
    /// and usage totals summed over message/toolResult/summary entries.
    ///
    /// divergence: `contextUsage` is omitted (the port has no
    /// `getContextUsage` equivalent yet).
    fn session_stats(&self) -> Value {
        let session = self.current_session();
        let (session_file, session_id, entries) = {
            let session_manager = session.session_manager().lock().expect("session lock");
            (
                session_manager
                    .session_file()
                    .map(|path| path.to_string_lossy().to_string()),
                session_manager.session_id().to_string(),
                session_manager.get_entries_owned(),
            )
        };

        let mut totals = UsageTotals::new();
        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut tool_calls = 0u64;
        let mut tool_results = 0u64;
        let mut total_messages = 0u64;
        for entry in &entries {
            match entry {
                SessionEntry::Compaction(compaction) => {
                    if let Some(usage) = &compaction.usage {
                        totals.add(usage);
                    }
                }
                SessionEntry::BranchSummary(summary) => {
                    if let Some(usage) = &summary.usage {
                        totals.add(usage);
                    }
                }
                SessionEntry::Message(message) => {
                    total_messages += 1;
                    match &message.message {
                        CodingAgentMessage::Base(pillar_ai::types::Message::User { .. }) => {
                            user_messages += 1;
                        }
                        CodingAgentMessage::Base(pillar_ai::types::Message::ToolResult(result)) => {
                            tool_results += 1;
                            if let Some(usage) = &result.usage {
                                totals.add(usage);
                            }
                        }
                        CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(
                            assistant,
                        )) => {
                            assistant_messages += 1;
                            tool_calls += assistant
                                .content
                                .iter()
                                .filter(|content| {
                                    matches!(content, pillar_ai::types::Content::ToolCall { .. })
                                })
                                .count() as u64;
                            totals.add(&assistant.usage);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        json!({
            "sessionFile": session_file,
            "sessionId": session_id,
            "userMessages": user_messages,
            "assistantMessages": assistant_messages,
            "toolCalls": tool_calls,
            "toolResults": tool_results,
            "totalMessages": total_messages,
            "tokens": {
                "input": totals.input,
                "output": totals.output,
                "cacheRead": totals.cache_read,
                "cacheWrite": totals.cache_write,
                "total": totals.input + totals.output + totals.cache_read + totals.cache_write,
            },
            "cost": totals.cost,
        })
    }

    fn session_state(&self) -> Value {
        let session = self.current_session();
        let (steering_mode, follow_up_mode) = {
            let settings = session.settings_manager().lock().expect("settings lock");
            (
                settings.steering_mode().to_string(),
                settings.follow_up_mode().to_string(),
            )
        };
        // Read the session-manager fields first: `session_id()` locks the
        // session manager itself, so holding the guard across it would
        // deadlock.
        let (session_file, session_name) = {
            let session_manager = session.session_manager().lock().expect("session lock");
            (
                session_manager
                    .session_file()
                    .map(|path| path.to_string_lossy().to_string()),
                session_manager.session_name(),
            )
        };
        let state = RpcSessionState {
            model: session
                .model()
                .map(|model| serde_json::to_value(model.to_model()).unwrap_or(Value::Null)),
            thinking_level: session.thinking_level(),
            is_streaming: session.is_streaming(),
            is_compacting: session.is_compacting(),
            steering_mode,
            follow_up_mode,
            session_file,
            session_id: session.session_id(),
            session_name,
            auto_compaction_enabled: session.auto_compaction_enabled(),
            message_count: session.state().messages.len() as u64,
            pending_message_count: session.pending_message_count() as u64,
        };
        serde_json::to_value(state).unwrap_or(Value::Null)
    }
}

impl Drop for RpcMode {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().expect("unsubscribe lock").take() {
            unsubscribe();
        }
    }
}

/// Read JSON-lines commands and dispatch them until the input ends
/// (upstream `runRpcMode`).
pub async fn run_rpc_mode(
    session: Arc<AgentSession>,
    input: impl BufRead,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
) -> Result<(), String> {
    run_rpc_mode_with_host(session, input, out, None).await
}

/// Read JSON-lines commands and dispatch them until the input ends
/// (upstream `runRpcMode`), with a runtime host for session replacement.
pub async fn run_rpc_mode_with_host(
    session: Arc<AgentSession>,
    input: impl BufRead,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    host: Option<Arc<dyn RpcRuntimeHost>>,
) -> Result<(), String> {
    let mode = RpcMode::new_with_host(session, out, host);
    for line in input.lines() {
        let line = line.map_err(|error| error.to_string())?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(envelope) = serde_json::from_str::<RpcCommandEnvelope>(trimmed) {
            let response = mode.handle_command(envelope).await;
            mode.write_response(&response);
        } else if serde_json::from_str::<RpcExtensionUiResponse>(trimmed).is_ok() {
            // Extension UI responses are host-rendered; ignore without a bridge.
        } else {
            mode.write_response(&RpcResponse::failure(
                None,
                "unknown",
                "invalid RPC command JSON",
            ));
        }
    }
    Ok(())
}

/// The wire shape of a bash result (upstream `BashResult`).
fn bash_result_to_json(result: &crate::core::bash_executor::BashResult) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("output".to_string(), Value::String(result.output.clone()));
    obj.insert(
        "exitCode".to_string(),
        result
            .exit_code
            .map(|code| json!(code))
            .unwrap_or(Value::Null),
    );
    obj.insert("cancelled".to_string(), Value::Bool(result.cancelled));
    obj.insert("truncated".to_string(), Value::Bool(result.truncated));
    if let Some(path) = &result.full_output_path {
        obj.insert(
            "fullOutputPath".to_string(),
            Value::String(path.to_string_lossy().to_string()),
        );
    }
    Value::Object(obj)
}

/// Parse an extension-provided bash result (upstream extensions may answer
/// `user_bash` with a `BashResult`).
fn bash_result_from_json(value: &Value) -> crate::core::bash_executor::BashResult {
    crate::core::bash_executor::BashResult {
        output: value
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        exit_code: value
            .get("exitCode")
            .and_then(Value::as_i64)
            .map(|code| code as i32),
        cancelled: value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        full_output_path: value
            .get("fullOutputPath")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from),
        truncation: None,
    }
}

fn parse_behavior(value: &str) -> Option<StreamingBehavior> {
    match value {
        "steer" => Some(StreamingBehavior::Steer),
        "followUp" => Some(StreamingBehavior::FollowUp),
        _ => None,
    }
}

fn parse_queue_mode(value: &str) -> pillar_agent::types::QueueMode {
    match value {
        "one-at-a-time" => pillar_agent::types::QueueMode::OneAtATime,
        _ => pillar_agent::types::QueueMode::All,
    }
}

/// The wire command tag for a command (upstream `command.type`).
pub fn command_name(command: &RpcCommand) -> String {
    let value = serde_json::to_value(command).unwrap_or(Value::Null);
    value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}
