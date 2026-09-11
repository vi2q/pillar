//! Port of packages/coding-agent/src/modes/rpc/rpc-mode.ts (pi v0.84.3):
//! headless JSON-lines RPC over stdin/stdout.
//!
//! divergence: the port covers the command subset whose session/model APIs
//! exist today; session-tree commands (new_session/switch_session/fork/
//! clone/get_entries/get_tree/get_fork_messages/export_html) and bash/cycle
//! commands return a `success: false` response with "not supported yet".
//! Extension UI requests are host-rendered (no UI bridge here), so
//! `extension_ui_response` lines are accepted and ignored.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::core::agent_session_class::{AgentSession, PromptOptions, StreamingBehavior};
use crate::core::messages::CodingAgentMessage;
use crate::core::model_mutation::CycleDirection;
use crate::core::session_entries::SessionEntry;
use crate::core::usage_totals::UsageTotals;
use crate::modes::json_event::{compaction_result_to_json, to_json_event};
use crate::modes::rpc::rpc_types::{
    RpcCommand, RpcCommandEnvelope, RpcExtensionUiResponse, RpcResponse, RpcSessionState,
};

/// The RPC session host: the session plus the stdout sink.
pub struct RpcMode {
    session: Arc<AgentSession>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
    unsubscribe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl RpcMode {
    /// Create the mode and subscribe to session events (written as JSON
    /// lines, upstream `session.subscribe` in `runRpcMode`).
    pub fn new(session: Arc<AgentSession>, out: Arc<Mutex<Box<dyn Write + Send>>>) -> Self {
        let sink = Arc::clone(&out);
        let unsubscribe = session.subscribe(Arc::new(move |event| {
            if let Ok(value) = to_json_event(event) {
                if let Ok(mut writer) = sink.lock() {
                    let _ = writeln!(writer, "{value}");
                }
            }
        }));
        Self {
            session,
            out,
            unsubscribe: Mutex::new(Some(unsubscribe)),
        }
    }

    /// Write one response line (upstream `writeResponse`).
    pub fn write_response(&self, response: &RpcResponse) {
        if let Ok(value) = serde_json::to_value(response) {
            if let Ok(mut writer) = self.out.lock() {
                let _ = writeln!(writer, "{value}");
            }
        }
    }

    /// Dispatch one command and return its response (upstream the command
    /// switch in `runRpcMode`).
    pub async fn handle_command(&self, envelope: RpcCommandEnvelope) -> RpcResponse {
        let id = envelope.id.clone();
        let command = command_name(&envelope.command);
        match envelope.command {
            RpcCommand::Prompt {
                message,
                streaming_behavior,
                ..
            } => {
                let behavior = streaming_behavior.as_deref().and_then(parse_behavior);
                match self
                    .session
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
            RpcCommand::Steer { message, .. } => match self.session.steer(&message, None).await {
                Ok(()) => RpcResponse::success(id, command, None),
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::FollowUp { message, .. } => {
                match self.session.follow_up(&message, None).await {
                    Ok(()) => RpcResponse::success(id, command, None),
                    Err(error) => RpcResponse::failure(id, command, error),
                }
            }
            RpcCommand::Abort => {
                self.session.abort().await;
                RpcResponse::success(id, command, None)
            }
            RpcCommand::ClearQueue => {
                let (steering, follow_up) = self.session.clear_queue();
                RpcResponse::success(
                    id,
                    command,
                    Some(json!({ "steering": steering, "followUp": follow_up })),
                )
            }
            RpcCommand::GetState => RpcResponse::success(id, command, Some(self.session_state())),
            RpcCommand::SetModel { provider, model_id } => {
                match self.session.model_runtime().get_model(&provider, &model_id) {
                    Some(model) => match self.session.set_model(model.clone(), false).await {
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
                let models: Vec<Value> = self
                    .session
                    .model_runtime()
                    .get_available_snapshot()
                    .iter()
                    .map(|model| serde_json::to_value(model).unwrap_or(Value::Null))
                    .collect();
                RpcResponse::success(id, command, Some(json!({ "models": models })))
            }
            RpcCommand::SetThinkingLevel { level } => {
                self.session.set_thinking_level(&level, false);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::GetAvailableThinkingLevels => RpcResponse::success(
                id,
                command,
                Some(json!({ "levels": self.session.available_thinking_levels() })),
            ),
            RpcCommand::CycleModel => match self.session.cycle_model(CycleDirection::Forward).await
            {
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
            RpcCommand::CycleThinkingLevel => match self.session.cycle_thinking_level() {
                Some(level) => RpcResponse::success(id, command, Some(json!({ "level": level }))),
                None => RpcResponse::success(id, command, Some(Value::Null)),
            },
            RpcCommand::SetSteeringMode { mode } => {
                self.session.set_steering_mode(parse_queue_mode(&mode));
                self.sync_queue_modes();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetFollowUpMode { mode } => {
                self.session.set_follow_up_mode(parse_queue_mode(&mode));
                self.sync_queue_modes();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetAutoCompaction { enabled } => {
                self.session.set_auto_compaction_enabled(enabled);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::SetAutoRetry { enabled } => {
                self.session.set_auto_retry_enabled(enabled);
                RpcResponse::success(id, command, None)
            }
            RpcCommand::AbortRetry => {
                self.session.abort_retry();
                RpcResponse::success(id, command, None)
            }
            RpcCommand::Compact {
                custom_instructions,
            } => match self.session.compact(custom_instructions.as_deref()).await {
                Ok(result) => {
                    RpcResponse::success(id, command, Some(compaction_result_to_json(&result)))
                }
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::GetSessionStats => {
                RpcResponse::success(id, command, Some(self.session_stats()))
            }
            RpcCommand::GetEntries { since } => {
                let (entries, leaf_id) = {
                    let session_manager =
                        self.session.session_manager().lock().expect("session lock");
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
                    let session_manager =
                        self.session.session_manager().lock().expect("session lock");
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
                Some(json!({ "text": self.session.last_assistant_text() })),
            ),
            RpcCommand::SetSessionName { name } => match self.session.set_session_name(&name) {
                Ok(()) => RpcResponse::success(id, command, None),
                Err(error) => RpcResponse::failure(id, command, error),
            },
            RpcCommand::GetMessages => {
                let messages: Vec<Value> = self
                    .session
                    .state()
                    .messages
                    .iter()
                    .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
                    .collect();
                RpcResponse::success(id, command, Some(json!({ "messages": messages })))
            }
            unsupported => RpcResponse::failure(
                id,
                command,
                format!("{}: not supported yet", command_name(&unsupported)),
            ),
        }
    }

    fn sync_queue_modes(&self) {
        self.session.sync_queue_modes_from_settings();
    }

    /// Upstream `session.getSessionStats()`: entry counts, tool-call count,
    /// and usage totals summed over message/toolResult/summary entries.
    ///
    /// divergence: `contextUsage` is omitted (the port has no
    /// `getContextUsage` equivalent yet).
    fn session_stats(&self) -> Value {
        let (session_file, session_id, entries) = {
            let session_manager = self.session.session_manager().lock().expect("session lock");
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
        let (steering_mode, follow_up_mode) = {
            let settings = self
                .session
                .settings_manager()
                .lock()
                .expect("settings lock");
            (
                settings.steering_mode().to_string(),
                settings.follow_up_mode().to_string(),
            )
        };
        // Read the session-manager fields first: `session_id()` locks the
        // session manager itself, so holding the guard across it would
        // deadlock.
        let (session_file, session_name) = {
            let session_manager = self.session.session_manager().lock().expect("session lock");
            (
                session_manager
                    .session_file()
                    .map(|path| path.to_string_lossy().to_string()),
                session_manager.session_name(),
            )
        };
        let state = RpcSessionState {
            model: self
                .session
                .model()
                .map(|model| serde_json::to_value(model.to_model()).unwrap_or(Value::Null)),
            thinking_level: self.session.thinking_level(),
            is_streaming: self.session.is_streaming(),
            is_compacting: self.session.is_compacting(),
            steering_mode,
            follow_up_mode,
            session_file,
            session_id: self.session.session_id(),
            session_name,
            auto_compaction_enabled: self.session.auto_compaction_enabled(),
            message_count: self.session.state().messages.len() as u64,
            pending_message_count: self.session.pending_message_count() as u64,
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
    let mode = RpcMode::new(session, out);
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
