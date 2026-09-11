//! Port of packages/coding-agent/src/modes/json-event.ts (pi v0.84.3): the
//! session-event shape emitted by the JSON and RPC stdout protocols.
//!
//! `message_update` is the only transformed event: cumulative assistant
//! snapshots (`partial`) are dropped, while usage, tool-call ids, and tool
//! names are kept because their size is constant.
//!
//! divergence: upstream returns the event object itself for the non-update
//! events (JavaScript preserves its identity); the port serializes them to
//! `serde_json::Value` with the same camelCase wire keys. Session entries
//! (`entry_appended.entry`) and compaction details are serialized
//! best-effort from the port's non-serializable types.

use serde_json::{Value, json};

use pillar_agent::types::AgentMessage;
use pillar_ai::types::{AssistantMessageEvent, Content, Message};

use crate::core::agent_session_class::{AgentSessionEvent, SummarizationRetrySource};

/// Convert a session event to its JSON wire shape (upstream `toJsonEvent`).
/// Returns an error where upstream throws (a malformed `message_update`).
pub fn to_json_event(event: &AgentSessionEvent) -> Result<Value, String> {
    match event {
        AgentSessionEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            let AgentMessage::Message(Message::Assistant(assistant)) = message else {
                return Err("message_update message is not an assistant message".to_string());
            };
            Ok(json!({
                "type": "message_update",
                "usage": assistant.usage,
                "assistantMessageEvent": assistant_message_event_to_json(assistant_message_event)?,
            }))
        }
        AgentSessionEvent::AgentStart => Ok(json!({ "type": "agent_start" })),
        AgentSessionEvent::AgentEnd {
            messages,
            will_retry,
        } => Ok(json!({
            "type": "agent_end",
            "messages": messages.iter().map(agent_message_to_json).collect::<Vec<_>>(),
            "willRetry": will_retry,
        })),
        AgentSessionEvent::TurnStart => Ok(json!({ "type": "turn_start" })),
        AgentSessionEvent::TurnEnd {
            message,
            tool_results,
        } => Ok(json!({
            "type": "turn_end",
            "message": agent_message_to_json(message),
            "toolResults": tool_results.iter().map(|result| {
                serde_json::to_value(result).unwrap_or(Value::Null)
            }).collect::<Vec<_>>(),
        })),
        AgentSessionEvent::MessageStart { message } => Ok(json!({
            "type": "message_start",
            "message": agent_message_to_json(message),
        })),
        AgentSessionEvent::MessageEnd { message } => Ok(json!({
            "type": "message_end",
            "message": agent_message_to_json(message),
        })),
        AgentSessionEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Ok(json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
        })),
        AgentSessionEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => Ok(json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
            "partialResult": partial_result,
        })),
        AgentSessionEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Ok(json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "result": result,
            "isError": is_error,
        })),
        AgentSessionEvent::AgentSettled => Ok(json!({ "type": "agent_settled" })),
        AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        } => Ok(json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        })),
        AgentSessionEvent::ThinkingLevelChanged { level } => Ok(json!({
            "type": "thinking_level_changed",
            "level": level,
        })),
        AgentSessionEvent::CompactionStart { reason } => Ok(json!({
            "type": "compaction_start",
            "reason": reason,
        })),
        AgentSessionEvent::EntryAppended { entry } => Ok(json!({
            "type": "entry_appended",
            "entry": session_entry_to_json(entry),
        })),
        AgentSessionEvent::SessionInfoChanged { name } => Ok(json!({
            "type": "session_info_changed",
            "name": name,
        })),
        AgentSessionEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        } => {
            let mut value = json!({
                "type": "compaction_end",
                "reason": reason,
                "result": result.as_ref().map(compaction_result_to_json).unwrap_or(Value::Null),
                "aborted": aborted,
                "willRetry": will_retry,
            });
            if let Some(error_message) = error_message {
                value["errorMessage"] = json!(error_message);
            }
            Ok(value)
        }
        AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => Ok(json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        })),
        AgentSessionEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            let mut value = json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
            });
            if let Some(final_error) = final_error {
                value["finalError"] = json!(final_error);
            }
            Ok(value)
        }
        AgentSessionEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => Ok(json!({
            "type": "summarization_retry_scheduled",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        })),
        AgentSessionEvent::SummarizationRetryAttemptStart { source } => Ok(match source {
            SummarizationRetrySource::BranchSummary => json!({
                "type": "summarization_retry_attempt_start",
                "source": "branchSummary",
            }),
            SummarizationRetrySource::Compaction { reason } => json!({
                "type": "summarization_retry_attempt_start",
                "source": "compaction",
                "reason": reason,
            }),
        }),
        AgentSessionEvent::SummarizationRetryFinished => {
            Ok(json!({ "type": "summarization_retry_finished" }))
        }
        AgentSessionEvent::BashExecutionUpdate { id, delta } => {
            let mut value = json!({
                "type": "bash_execution_update",
                "delta": delta,
            });
            if let Some(id) = id {
                value["id"] = json!(id);
            }
            Ok(value)
        }
    }
}

/// Serialize an agent message in the upstream role-tagged union shape.
fn agent_message_to_json(message: &AgentMessage) -> Value {
    let mut value = match message {
        AgentMessage::Message(message) => serde_json::to_value(message).unwrap_or(Value::Null),
        AgentMessage::BashExecution(message) => {
            serde_json::to_value(message.as_ref()).unwrap_or(Value::Null)
        }
        AgentMessage::Custom(message) => {
            serde_json::to_value(message.as_ref()).unwrap_or(Value::Null)
        }
        AgentMessage::BranchSummary(message) => {
            serde_json::to_value(message.as_ref()).unwrap_or(Value::Null)
        }
        AgentMessage::CompactionSummary(message) => {
            serde_json::to_value(message.as_ref()).unwrap_or(Value::Null)
        }
    };
    if let Value::Object(map) = &mut value {
        if !map.contains_key("role") {
            let role = match message {
                AgentMessage::Message(_) => None,
                AgentMessage::BashExecution(_) => Some("bashExecution"),
                AgentMessage::Custom(_) => Some("custom"),
                AgentMessage::BranchSummary(_) => Some("branchSummary"),
                AgentMessage::CompactionSummary(_) => Some("compactionSummary"),
            };
            if let Some(role) = role {
                map.insert("role".to_string(), json!(role));
            }
        }
    }
    value
}

/// Convert one streaming assistant event, dropping the cumulative `partial`
/// snapshot and camelCasing the wire keys (upstream
/// `toJsonAssistantMessageEvent`).
fn assistant_message_event_to_json(event: &AssistantMessageEvent) -> Result<Value, String> {
    let raw = serde_json::to_value(event).unwrap_or(Value::Null);
    let Value::Object(raw) = raw else {
        return Ok(raw);
    };
    let mut map = serde_json::Map::new();
    for (key, value) in raw {
        if key == "partial" {
            continue;
        }
        let key = match key.as_str() {
            "content_index" => "contentIndex".to_string(),
            "tool_call" => "toolCall".to_string(),
            _ => key,
        };
        map.insert(key, value);
    }
    if let AssistantMessageEvent::ToolcallStart {
        content_index,
        partial,
    } = event
    {
        let (id, name) = match partial.content.get(*content_index) {
            Some(Content::ToolCall { id, name, .. }) => (id.clone(), name.clone()),
            _ => {
                return Err(format!(
                    "toolcall_start content at index {content_index} is not a tool call"
                ));
            }
        };
        map.insert("id".to_string(), json!(id));
        map.insert("toolName".to_string(), json!(name));
    }
    Ok(Value::Object(map))
}

/// Serialize a compaction result (upstream `CompactionResult`).
pub(crate) fn compaction_result_to_json(
    result: &crate::core::compaction::driver::CompactionResult,
) -> Value {
    json!({
        "summary": result.summary,
        "firstKeptEntryId": result.first_kept_entry_id,
        "tokensBefore": result.tokens_before,
        "estimatedTokensAfter": result.estimated_tokens_after,
        "usage": result.usage,
        "details": result.details.as_ref().map(|details| json!({
            "readFiles": details.read_files,
            "modifiedFiles": details.modified_files,
        })),
    })
}

/// Serialize a session entry with the canonical JSONL shape
/// (`session_manager::entry_to_json`, upstream emits the full entry).
fn session_entry_to_json(entry: &crate::core::session_entries::SessionEntry) -> Value {
    crate::core::session_manager::entry_to_json(entry)
}

/// Re-export used by tests (the port keeps the assistant message type here).
pub use pillar_ai::types::AssistantMessage as JsonAssistantMessage;
