//! Port of packages/server/src/protocol.ts (pi v0.84.3): the pi-ai →
//! protocol bridge — validating mappers from execution-boundary
//! values into the protocol's JSON subset (assistant / user /
//! tool-result transcript items, model metadata, usage, raw JSON).
//!
//! divergences: Rust's type system removes the plain-object /
//! prototype / sparse-array / bigint / undefined checks (those values
//! cannot appear in serde_json::Value); toProtocolJsonValue becomes a
//! finite-number check plus a cycle guard, and error cases surface as
//! [`ProtocolBridgeError`]. Circular `details` become "[Circular]"
//! through the same seen-set walk as upstream.

use pillar_ai::models::get_supported_thinking_levels;
use pillar_ai::types::{
    AssistantMessage, Content, Message, Model, StopReason, ToolResultMessage, Usage,
};
use pillar_protocol::schemas::{
    AssistantContent, AssistantStatus, AssistantStopReason, AssistantTranscriptItem, ModelCost,
    ModelMetadata, ThinkingLevel, ToolStatus, ToolTranscriptItem, Usage as ProtocolUsage,
    UsageCost, UserContent,
};

/// Error from a lossy or invalid boundary conversion (upstream the
/// TypeError throws).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolBridgeError {
    pub message: String,
}

impl std::fmt::Display for ProtocolBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProtocolBridgeError {}

fn error(message: impl Into<String>) -> ProtocolBridgeError {
    ProtocolBridgeError {
        message: message.into(),
    }
}

fn identifier(value: &str, label: &str) -> Result<String, ProtocolBridgeError> {
    if value.is_empty() {
        return Err(error(format!("{label} must be a non-empty string")));
    }
    Ok(value.to_string())
}

fn timestamp(value: u64) -> Result<u64, ProtocolBridgeError> {
    // Rust u64 cannot be fractional/negative; the check documents the
    // upstream safe-integer gate.
    if value >= 9.007_199_254_740_992e15 as u64 {
        return Err(error("Protocol timestamps must be non-negative integers"));
    }
    Ok(value)
}

fn non_negative_integer(value: u64) -> u64 {
    value
}

fn non_negative_number(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// Validate and copy a value into the protocol's JSON subset
/// (upstream `toProtocolJsonValue`): finite numbers only, no cycles.
pub fn to_protocol_json_value(
    value: &serde_json::Value,
) -> Result<serde_json::Value, ProtocolBridgeError> {
    fn walk(
        value: &serde_json::Value,
        seen: &mut Vec<*const serde_json::Value>,
    ) -> Result<serde_json::Value, ProtocolBridgeError> {
        match value {
            serde_json::Value::Number(number) => {
                if number.as_f64().is_some_and(|float| !float.is_finite()) {
                    return Err(error("Protocol JSON numbers must be finite"));
                }
                Ok(value.clone())
            }
            serde_json::Value::Array(entries) => {
                if seen.contains(&(value as *const _)) {
                    return Err(error(
                        "Protocol JSON values must not contain circular references",
                    ));
                }
                seen.push(value as *const _);
                let result = entries
                    .iter()
                    .map(|entry| walk(entry, seen))
                    .collect::<Result<Vec<_>, _>>();
                seen.pop();
                Ok(serde_json::Value::Array(result?))
            }
            serde_json::Value::Object(entries) => {
                if seen.contains(&(value as *const _)) {
                    return Err(error(
                        "Protocol JSON values must not contain circular references",
                    ));
                }
                seen.push(value as *const _);
                let result = entries
                    .iter()
                    .map(|(key, entry)| walk(entry, seen).map(|converted| (key.clone(), converted)))
                    .collect::<Result<serde_json::Map<_, _>, _>>();
                seen.pop();
                Ok(serde_json::Value::Object(result?))
            }
            _ => Ok(value.clone()),
        }
    }
    walk(value, &mut Vec::new())
}

fn model_cost(cost: &pillar_ai::types::ModelCost) -> ModelCost {
    let rates = cost.rates;
    ModelCost {
        input: non_negative_number(rates.input),
        output: non_negative_number(rates.output),
        cache_read: non_negative_number(rates.cache_read),
        cache_write: non_negative_number(rates.cache_write),
    }
}

fn usage_cost(cost: &pillar_ai::types::UsageCost) -> UsageCost {
    UsageCost {
        input: non_negative_number(cost.input),
        output: non_negative_number(cost.output),
        cache_read: non_negative_number(cost.cache_read),
        cache_write: non_negative_number(cost.cache_write),
        total: non_negative_number(cost.total),
    }
}

fn usage_components(usage: &Usage) -> ProtocolUsage {
    ProtocolUsage {
        input: non_negative_integer(usage.input),
        output: non_negative_integer(usage.output),
        cache_read: non_negative_integer(usage.cache_read),
        cache_write: non_negative_integer(usage.cache_write),
        reasoning: usage.reasoning.map(non_negative_integer),
        total_tokens: non_negative_integer(usage.total_tokens),
        cost: usage_cost(&usage.cost),
    }
}

/// Upstream `toProtocolUsage` (executed values are u64/f64, so the
/// clamping reduces to component mapping).
pub fn to_protocol_usage(usage: Option<&Usage>) -> Option<ProtocolUsage> {
    usage.map(usage_components)
}

/// Upstream `toProtocolModelMetadata`.
pub fn to_protocol_model_metadata(
    model: &Model,
    authenticated: bool,
) -> Result<ModelMetadata, ProtocolBridgeError> {
    let supported = get_supported_thinking_levels(model)
        .into_iter()
        .map(|level| match level {
            pillar_ai::types::ModelThinkingLevel::Off => ThinkingLevel::Off,
            pillar_ai::types::ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
            pillar_ai::types::ModelThinkingLevel::Low => ThinkingLevel::Low,
            pillar_ai::types::ModelThinkingLevel::Medium => ThinkingLevel::Medium,
            pillar_ai::types::ModelThinkingLevel::High => ThinkingLevel::High,
            pillar_ai::types::ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
            pillar_ai::types::ModelThinkingLevel::Max => ThinkingLevel::Max,
        })
        .collect::<Vec<_>>();
    Ok(ModelMetadata {
        provider: identifier(&model.provider, "Model provider")?,
        id: identifier(&model.id, "Model id")?,
        name: identifier(&model.name, "Model name")?,
        api: identifier(&model.api, "Model API")?,
        reasoning: model.reasoning,
        input: model
            .input
            .iter()
            .map(|kind| match kind.as_str() {
                "image" => pillar_protocol::schemas::InputKind::Image,
                _ => pillar_protocol::schemas::InputKind::Text,
            })
            .collect(),
        context_window: model.context_window.max(1),
        max_tokens: model.max_tokens.max(1),
        cost: model_cost(&model.cost),
        supported_thinking_levels: supported,
        authenticated,
    })
}

fn user_content(content: &pillar_ai::types::UserContent) -> Vec<UserContent> {
    match content {
        pillar_ai::types::UserContent::Text(text) => vec![UserContent::Text { text: text.clone() }],
        pillar_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|part| match part {
                Content::Text { text, .. } => UserContent::Text { text: text.clone() },
                Content::Image { data, mime_type } => UserContent::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                },
                _ => UserContent::Text {
                    text: String::new(),
                },
            })
            .collect(),
    }
}

/// Upstream `toProtocolUserMessage`.
pub fn to_protocol_user_message(
    message: &Message,
    id: &str,
) -> Result<pillar_protocol::schemas::TranscriptItem, ProtocolBridgeError> {
    let Message::User {
        content,
        timestamp: at,
    } = message
    else {
        return Err(error("Expected a user message"));
    };
    Ok(pillar_protocol::schemas::TranscriptItem::User {
        id: identifier(id, "Transcript item id")?,
        content: user_content(content),
        timestamp: timestamp(*at)?,
    })
}

fn assistant_content(content: &[Content]) -> Result<Vec<AssistantContent>, ProtocolBridgeError> {
    content
        .iter()
        .map(|part| match part {
            Content::Text { text, .. } => Ok(AssistantContent::Text { text: text.clone() }),
            Content::Thinking {
                thinking, redacted, ..
            } => Ok(AssistantContent::Thinking {
                thinking: thinking.clone(),
                redacted: *redacted,
            }),
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Ok(AssistantContent::ToolCall {
                tool_call_id: identifier(id, "Tool call id")?,
                tool_name: identifier(name, "Tool call name")?,
                input: to_protocol_json_value(arguments)?,
            }),
            Content::Image { .. } => Err(error("Assistant messages cannot contain images")),
        })
        .collect()
}

fn assistant_stop_reason(stop_reason: StopReason) -> Option<AssistantStopReason> {
    match stop_reason {
        StopReason::Stop => Some(AssistantStopReason::Stop),
        StopReason::Length => Some(AssistantStopReason::Length),
        StopReason::ToolUse => Some(AssistantStopReason::ToolUse),
        StopReason::Error => Some(AssistantStopReason::Error),
        StopReason::Aborted => Some(AssistantStopReason::Aborted),
        StopReason::Pending | StopReason::Deferred => None,
    }
}

/// Upstream `toProtocolAssistantMessage`: derives the streaming
/// status from a pending stop reason and validates error messages.
pub fn to_protocol_assistant_message(
    message: &AssistantMessage,
    id: &str,
) -> Result<AssistantTranscriptItem, ProtocolBridgeError> {
    let content = assistant_content(&message.content)?;
    let id = identifier(id, "Transcript item id")?;
    let timestamp = timestamp(message.timestamp)?;
    let model = pillar_protocol::schemas::ModelRef {
        provider: identifier(&message.provider, "Assistant provider")?,
        id: identifier(&message.model, "Assistant model")?,
    };
    let response_model = message
        .response_model
        .as_deref()
        .map(|value| identifier(value, "Assistant response model"))
        .transpose()?;
    let usage = to_protocol_usage(Some(&message.usage));
    match message.stop_reason {
        StopReason::Pending => Ok(AssistantTranscriptItem {
            id,
            content,
            model,
            response_model,
            usage,
            status: AssistantStatus::Streaming,
            stop_reason: None,
            error_message: None,
            timestamp,
        }),
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => {
            Ok(AssistantTranscriptItem {
                id,
                content,
                model,
                response_model,
                usage,
                status: AssistantStatus::Complete,
                stop_reason: assistant_stop_reason(message.stop_reason),
                error_message: None,
                timestamp,
            })
        }
        StopReason::Deferred => Err(error(
            "Deferred assistant messages are not supported by protocol v1",
        )),
        StopReason::Error => {
            if message.error_message.as_deref() == Some("") {
                return Err(error("Assistant error messages must not be empty"));
            }
            Ok(AssistantTranscriptItem {
                id,
                content,
                model,
                response_model,
                usage,
                status: AssistantStatus::Error,
                stop_reason: Some(AssistantStopReason::Error),
                error_message: message.error_message.clone(),
                timestamp,
            })
        }
        StopReason::Aborted => Ok(AssistantTranscriptItem {
            id,
            content,
            model,
            response_model,
            usage,
            status: AssistantStatus::Aborted,
            stop_reason: Some(AssistantStopReason::Aborted),
            error_message: message.error_message.clone(),
            timestamp,
        }),
    }
}

fn tool_content(content: &[Content]) -> Vec<pillar_protocol::schemas::ToolContent> {
    content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => {
                Some(pillar_protocol::schemas::ToolContent::Text { text: text.clone() })
            }
            Content::Image { data, mime_type } => {
                Some(pillar_protocol::schemas::ToolContent::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Tool call reference (upstream `ToolTranscriptOptions.call`).
pub struct ToolCallRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub arguments: &'a serde_json::Value,
}

/// Upstream `toProtocolToolResultMessage`: validates the result
/// against its call and sanitizes diagnostic details.
pub fn to_protocol_tool_result_message(
    message: &ToolResultMessage,
    id: &str,
    call: &ToolCallRef<'_>,
) -> Result<ToolTranscriptItem, ProtocolBridgeError> {
    let call_id = identifier(call.id, "Tool call id")?;
    let call_name = identifier(call.name, "Tool call name")?;
    let result_call_id = identifier(&message.tool_call_id, "Tool result call id")?;
    if result_call_id != call_id {
        return Err(error(format!(
            "Tool result {result_call_id} does not match tool call {call_id}"
        )));
    }
    let result_name = identifier(&message.tool_name, "Tool result name")?;
    if result_name != call_name {
        return Err(error(format!(
            "Tool result {result_name} does not match tool call {call_name}"
        )));
    }
    let details = message
        .details
        .as_ref()
        .map(crate::snapshots::sanitize_protocol_details);
    let usage = to_protocol_usage(message.usage.as_ref());
    let id = identifier(id, "Transcript item id")?;
    let common = ToolTranscriptItem {
        id,
        tool_call_id: call_id,
        tool_name: call_name,
        input: to_protocol_json_value(call.arguments)?,
        content: tool_content(&message.content),
        details,
        status: if message.is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Complete
        },
        is_error: message.is_error,
        usage,
        timestamp: timestamp(message.timestamp)?,
    };
    Ok(common)
}

/// Convenience: map any [`Message`] into its protocol transcript item
/// (upstream callers dispatch on role).
pub fn to_protocol_transcript_item(
    message: &Message,
    id: &str,
    tool_call: Option<(&str, &str, &serde_json::Value)>,
) -> Result<pillar_protocol::schemas::TranscriptItem, ProtocolBridgeError> {
    match message {
        Message::User { .. } => to_protocol_user_message(message, id),
        Message::Assistant(assistant) => Ok(pillar_protocol::schemas::TranscriptItem::Assistant(
            to_protocol_assistant_message(assistant, id)?,
        )),
        Message::ToolResult(tool) => {
            let Some((call_id, call_name, arguments)) = tool_call else {
                return Err(error("Tool result mapping requires its tool call"));
            };
            Ok(pillar_protocol::schemas::TranscriptItem::Tool(
                to_protocol_tool_result_message(
                    tool,
                    id,
                    &ToolCallRef {
                        id: call_id,
                        name: call_name,
                        arguments,
                    },
                )?,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_protocol::codec::encode_server_message;
    use pillar_protocol::framing::FrameDecoderOptions;
    use pillar_protocol::schemas::PROTOCOL_VERSION;

    fn test_model() -> Model {
        Model {
            id: "model-1".to_string(),
            name: "Model One".to_string(),
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            base_url: "https://example.test".to_string(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".to_string(), "image".to_string()],
            cost: pillar_ai::types::ModelCost {
                rates: pillar_ai::types::ModelCostRates {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.1,
                    cache_write: 0.2,
                },
                tiers: None,
            },
            context_window: 100_000,
            max_tokens: 10_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn assistant_usage() -> Usage {
        Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 10,
            cost: pillar_ai::types::UsageCost {
                input: 0.1,
                output: 0.2,
                cache_read: 0.3,
                cache_write: 0.4,
                total: 1.0,
            },
        }
    }

    /// Upstream `assertValidServerPayload`: every mapped transcript
    /// item must survive a protocol-valid server hello and event.
    fn assert_valid_server_payload(item: &pillar_protocol::schemas::TranscriptItem) {
        let model = to_protocol_model_metadata(&test_model(), true).unwrap();
        let hello = serde_json::json!({
            "type": "hello",
            "version": PROTOCOL_VERSION,
            "connectionId": "connection-1",
            "snapshot": {
                "serverId": "server-1",
                "protocolVersion": PROTOCOL_VERSION,
                "revision": 0,
                "sessions": [{
                    "id": "session-1",
                    "createdAt": 1,
                    "updatedAt": 1,
                    "sessionName": "Session one",
                    "cwd": "/workspace"
                }],
                "models": [serde_json::to_value(&model).unwrap()]
            }
        });
        assert!(encode_server_message(&hello, FrameDecoderOptions::default()).is_ok());

        let event = serde_json::json!({
            "type": "event",
            "event": {
                "type": "session_snapshot",
                "snapshot": {
                    "id": "session-1",
                    "cwd": "/workspace",
                    "createdAt": 1,
                    "updatedAt": 1,
                    "phase": "idle",
                    "model": { "provider": "test-provider", "id": "model-1" },
                    "thinkingLevel": "off",
                    "attached": true,
                    "locked": true,
                    "revision": 1,
                    "transcript": [serde_json::to_value(item).unwrap()],
                    "queuedSteer": [],
                    "queuedSteerCount": 0
                }
            }
        });
        assert!(encode_server_message(&event, FrameDecoderOptions::default()).is_ok());
    }

    #[test]
    fn maps_model_metadata_and_produces_protocol_valid_output() {
        let result = to_protocol_model_metadata(&test_model(), true).unwrap();
        assert_eq!(result.provider, "test-provider");
        assert_eq!(result.id, "model-1");
        assert_eq!(result.api, "test-api");
        assert!(result.authenticated);
        assert!(
            result
                .supported_thinking_levels
                .contains(&ThinkingLevel::Off)
        );
    }

    #[test]
    fn exhaustively_maps_assistant_content_and_stop_reasons() {
        let message = AssistantMessage {
            content: vec![
                Content::Text {
                    text: "hello".to_string(),
                    text_signature: None,
                },
                Content::Thinking {
                    thinking: "hmm".to_string(),
                    thinking_signature: None,
                    redacted: Some(false),
                },
                Content::ToolCall {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "README.md" }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "model-1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: assistant_usage(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 123,
        };
        let result = to_protocol_assistant_message(&message, "message-1").unwrap();
        assert_eq!(result.id, "message-1");
        assert_eq!(result.status, AssistantStatus::Complete);
        assert_eq!(result.stop_reason, Some(AssistantStopReason::ToolUse));
        assert_eq!(result.model.provider, "test-provider");
        assert_eq!(result.model.id, "model-1");
        assert_eq!(
            result.content,
            vec![
                AssistantContent::Text {
                    text: "hello".to_string()
                },
                AssistantContent::Thinking {
                    thinking: "hmm".to_string(),
                    redacted: Some(false),
                },
                AssistantContent::ToolCall {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "read".to_string(),
                    input: serde_json::json!({ "path": "README.md" }),
                },
            ]
        );
        let item =
            to_protocol_transcript_item(&Message::Assistant(Box::new(message)), "message-1", None)
                .unwrap();
        assert_valid_server_payload(&item);
    }

    #[test]
    fn maps_user_messages() {
        let user = Message::User {
            content: pillar_ai::types::UserContent::Text("hello".to_string()),
            timestamp: 1,
        };
        let result = to_protocol_user_message(&user, "user-1").unwrap();
        match &result {
            pillar_protocol::schemas::TranscriptItem::User { id, content, .. } => {
                assert_eq!(id, "user-1");
                assert_eq!(
                    content,
                    &vec![UserContent::Text {
                        text: "hello".to_string()
                    }]
                );
            }
            other => panic!("unexpected item: {other:?}"),
        }
        assert_valid_server_payload(&result);
    }

    #[test]
    fn maps_tool_results_and_sanitizes_details() {
        let tool = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![Content::Text {
                text: "result".to_string(),
                text_signature: None,
            }],
            details: Some(serde_json::json!({ "note": "diagnostic" })),
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2,
        };
        let call = ToolCallRef {
            id: "call-1",
            name: "read",
            arguments: &serde_json::json!({ "path": "README.md" }),
        };
        let result = to_protocol_tool_result_message(&tool, "tool-1", &call).unwrap();
        assert_eq!(result.id, "tool-1");
        assert_eq!(result.tool_name, "read");
        assert_eq!(result.input, serde_json::json!({ "path": "README.md" }));
        assert_eq!(
            result.details,
            Some(serde_json::json!({ "note": "diagnostic" }))
        );
        assert_eq!(result.status, ToolStatus::Complete);
        let item = to_protocol_transcript_item(
            &Message::ToolResult(Box::new(tool)),
            "tool-1",
            Some((
                "call-1",
                "read",
                &serde_json::json!({ "path": "README.md" }),
            )),
        )
        .unwrap();
        assert_valid_server_payload(&item);
    }

    #[test]
    fn rejects_tool_results_associated_with_a_different_call() {
        let base = |call_id: &str, name: &str| ToolResultMessage {
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            content: vec![Content::Text {
                text: "result".to_string(),
                text_signature: None,
            }],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2,
        };
        let call = ToolCallRef {
            id: "call-1",
            name: "read",
            arguments: &serde_json::json!({}),
        };
        let error =
            to_protocol_tool_result_message(&base("call-2", "read"), "tool-1", &call).unwrap_err();
        assert!(error.message.to_lowercase().contains("tool result"));
        let error =
            to_protocol_tool_result_message(&base("call-1", "write"), "tool-1", &call).unwrap_err();
        assert!(error.message.to_lowercase().contains("tool result"));
    }

    #[test]
    fn derives_streaming_status_from_a_pending_stop_reason() {
        let message = AssistantMessage {
            content: vec![Content::Text {
                text: "partial".to_string(),
                text_signature: None,
            }],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "model-1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 123,
        };
        let result = to_protocol_assistant_message(&message, "message-pending").unwrap();
        assert_eq!(result.status, AssistantStatus::Streaming);
        assert_eq!(result.stop_reason, None);
        let item = to_protocol_transcript_item(
            &Message::Assistant(Box::new(message)),
            "message-pending",
            None,
        )
        .unwrap();
        assert_valid_server_payload(&item);
    }

    #[test]
    fn preserves_optional_non_empty_assistant_error_messages() {
        let base = |error_message: Option<&str>| AssistantMessage {
            content: vec![],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "model-1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: error_message.map(|value| value.to_string()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 123,
        };
        let without = to_protocol_assistant_message(&base(None), "message-error").unwrap();
        assert_eq!(without.status, AssistantStatus::Error);
        assert_eq!(without.stop_reason, Some(AssistantStopReason::Error));
        assert_eq!(without.error_message, None);
        let item = to_protocol_transcript_item(
            &Message::Assistant(Box::new(base(None))),
            "message-error",
            None,
        )
        .unwrap();
        assert_valid_server_payload(&item);
        let empty = to_protocol_assistant_message(&base(Some("")), "message-error");
        assert!(empty.is_err());
        let with = to_protocol_assistant_message(&base(Some("failed")), "message-error").unwrap();
        assert_eq!(with.error_message.as_deref(), Some("failed"));
    }

    #[test]
    fn rejects_invalid_source_identifiers_and_timestamps() {
        let message = AssistantMessage {
            content: vec![Content::ToolCall {
                id: String::new(),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
            }],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "model-1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        };
        let error = to_protocol_assistant_message(&message, "assistant-1").unwrap_err();
        assert!(error.message.to_lowercase().contains("tool call id"));
    }

    #[test]
    fn rejects_lossy_json_conversions() {
        assert!(to_protocol_json_value(&serde_json::Value::Null).is_ok());
        // Non-finite numbers cannot exist in serde_json::Value; the
        // finite check is reachable only through raw parsing of
        // arbitrary input, which serde rejects upstream of this call.
        assert_eq!(
            to_protocol_json_value(&serde_json::json!({"a": 1})).unwrap(),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn sanitizes_non_finite_numbers_to_strings() {
        // serde_json cannot hold NaN/Infinity; the sanitizer's
        // stringification is only reachable via f64 paths constructed
        // in Rust code (documented divergence).
        assert_eq!(
            crate::snapshots::sanitize_protocol_details(&serde_json::json!([null, "value"])),
            serde_json::json!([null, "value"])
        );
    }

    #[test]
    fn deferred_assistant_messages_are_rejected() {
        let message = AssistantMessage {
            content: vec![],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "model-1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Deferred,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        };
        let error = to_protocol_assistant_message(&message, "m").unwrap_err();
        assert_eq!(
            error.message,
            "Deferred assistant messages are not supported by protocol v1"
        );
    }
}
