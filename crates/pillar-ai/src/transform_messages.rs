//! Port of packages/ai/src/api/transform-messages.ts (pi v0.84.3).
//!
//! Cross-provider message transformation before replay: unsupported image
//! downgrade, thinking-block / tool-call-signature handling across models,
//! tool-call ID normalization, and synthetic tool results for orphaned tool
//! calls.
//!
//! divergence: upstream normalizes `null` content from untyped callers; the
//! Rust message types cannot represent null content, so that pass is
//! unnecessary. Timestamps on synthetic tool results use the current wall
//! clock (upstream: `Date.now()`).

use std::collections::{HashMap, HashSet};

use crate::types::{AssistantMessage, Content, Message, Model, StopReason, ToolResultMessage};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn replace_images_with_placeholder(content: &[Content], placeholder: &str) -> Vec<Content> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;

    for block in content {
        if matches!(block, Content::Image { .. }) {
            if !previous_was_placeholder {
                result.push(Content::text(placeholder));
            }
            previous_was_placeholder = true;
            continue;
        }

        result.push(block.clone());
        previous_was_placeholder = block.as_text() == Some(placeholder);
    }

    result
}

fn downgrade_unsupported_images(messages: &[Message], model: &Model) -> Vec<Message> {
    if model.input.iter().any(|input| input == "image") {
        return messages.to_vec();
    }

    messages
        .iter()
        .map(|message| match message {
            Message::User { content, timestamp } => {
                let content = match content {
                    crate::types::UserContent::Text(text) => {
                        crate::types::UserContent::Text(text.clone())
                    }
                    crate::types::UserContent::Blocks(blocks) => crate::types::UserContent::Blocks(
                        replace_images_with_placeholder(blocks, NON_VISION_USER_IMAGE_PLACEHOLDER),
                    ),
                };
                Message::User {
                    content,
                    timestamp: *timestamp,
                }
            }
            Message::ToolResult(tool_result) => Message::ToolResult(Box::new(ToolResultMessage {
                content: replace_images_with_placeholder(
                    &tool_result.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                ),
                ..(**tool_result).clone()
            })),
            other => other.clone(),
        })
        .collect()
}

/// Normalize tool call ID for cross-provider compatibility. OpenAI Responses
/// API generates IDs that are 450+ chars with special characters like `|`;
/// Anthropic APIs require IDs matching `^[a-zA-Z0-9_-]+$` (max 64 chars).
/// Tool-call ID normalizer (upstream: `normalizeToolCallId?: (id, model,
/// source) => string`).
pub type NormalizeToolCallId<'a> = &'a dyn Fn(&str, &Model, &AssistantMessage) -> String;

pub fn transform_messages(
    messages: Vec<Message>,
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId<'_>>,
) -> Vec<Message> {
    // Original tool call IDs to normalized IDs.
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let image_aware_messages = downgrade_unsupported_images(&messages, model);

    // First pass: transform messages (unsupported image downgrade, thinking
    // blocks, tool call ID normalization).
    let transformed: Vec<Message> = image_aware_messages
        .into_iter()
        .map(|message| match message {
            Message::User { .. } => message,
            Message::ToolResult(mut tool_result) => {
                if let Some(normalized_id) = tool_call_id_map.get(&tool_result.tool_call_id)
                    && normalized_id != &tool_result.tool_call_id
                {
                    tool_result.tool_call_id = normalized_id.clone();
                }
                Message::ToolResult(tool_result)
            }
            Message::Assistant(assistant) => {
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let source = AssistantMessage {
                    content: assistant.content.clone(),
                    ..(*assistant).clone()
                };

                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        Content::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            // Redacted thinking is opaque encrypted content,
                            // only valid for the same model. Drop it for
                            // cross-model to avoid API errors.
                            if redacted == &Some(true) {
                                if is_same_model {
                                    content.push(block.clone());
                                }
                                continue;
                            }
                            // For same model: keep thinking blocks with
                            // signatures (needed for replay) even if the
                            // thinking text is empty (OpenAI encrypted
                            // reasoning).
                            if is_same_model && thinking_signature.is_some() {
                                content.push(block.clone());
                                continue;
                            }
                            // Skip empty thinking blocks, convert others to
                            // plain text.
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            if is_same_model {
                                content.push(block.clone());
                            } else {
                                content.push(Content::text(thinking.clone()));
                            }
                        }
                        Content::Text { text, .. } => {
                            if is_same_model {
                                content.push(block.clone());
                            } else {
                                content.push(Content::text(text.clone()));
                            }
                        }
                        Content::Image { .. } => {
                            content.push(block.clone());
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature,
                            namespace,
                        } => {
                            let mut normalized_call = Content::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                                thought_signature: thought_signature.clone(),
                                namespace: namespace.clone(),
                            };
                            let Content::ToolCall {
                                id: call_id,
                                thought_signature: call_thought_signature,
                                ..
                            } = &mut normalized_call
                            else {
                                unreachable!("just constructed a tool call");
                            };

                            if !is_same_model && call_thought_signature.is_some() {
                                *call_thought_signature = None;
                            }

                            if !is_same_model && let Some(normalize) = normalize_tool_call_id {
                                let normalized_id = normalize(call_id, model, &source);
                                if normalized_id != *call_id {
                                    tool_call_id_map.insert(call_id.clone(), normalized_id.clone());
                                    *call_id = normalized_id;
                                }
                            }

                            content.push(normalized_call);
                        }
                    }
                }

                Message::Assistant(Box::new(AssistantMessage {
                    content,
                    ..(*assistant).clone()
                }))
            }
        })
        .collect();

    // Second pass: insert synthetic empty tool results for orphaned tool
    // calls. This preserves thinking signatures and satisfies API
    // requirements.
    let mut result: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new(); // (id, name)
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();

    let insert_synthetic_tool_results =
        |result: &mut Vec<Message>,
         pending_tool_calls: &mut Vec<(String, String)>,
         existing_tool_result_ids: &mut HashSet<String>| {
            for (id, name) in pending_tool_calls.drain(..) {
                if !existing_tool_result_ids.contains(&id) {
                    result.push(Message::ToolResult(Box::new(ToolResultMessage {
                        tool_call_id: id,
                        tool_name: name,
                        content: vec![Content::text("No result provided")],
                        details: None,
                        usage: None,
                        added_tool_names: None,
                        is_error: true,
                        timestamp: now_ms(),
                    })));
                }
            }
            existing_tool_result_ids.clear();
        };

    for message in transformed {
        match message {
            Message::Assistant(assistant) => {
                // If we have pending orphaned tool calls from a previous
                // assistant, insert synthetic results now.
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );

                // Skip errored/aborted assistant messages entirely. These are
                // incomplete turns that shouldn't be replayed: they may have
                // partial content (reasoning without message, incomplete tool
                // calls), replaying them can cause API errors, and the model
                // should retry from the last valid state.
                if assistant.stop_reason == StopReason::Error
                    || assistant.stop_reason == StopReason::Aborted
                {
                    continue;
                }

                let tool_calls: Vec<(String, String)> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    pending_tool_calls = tool_calls;
                    existing_tool_result_ids = HashSet::new();
                }

                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            other => {
                // User message interrupts tool flow - insert synthetic
                // results for orphaned calls.
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(other);
            }
        }
    }

    // If the conversation ends with unresolved tool calls, synthesize
    // results now.
    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );

    result
}
