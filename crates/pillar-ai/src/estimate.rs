//! Port of packages/ai/src/utils/estimate.ts (pi v0.84.3).
//!
//! Context token estimation: text is approximated at 4 chars/token, images
//! at a fixed 4800 chars. When the context contains an applicable assistant
//! usage block (the newest, not followed by an inserted newer prefix
//! message, not aborted/errored), the estimate anchors on reported usage
//! and only counts trailing messages.

use crate::types::{Content, Context, Message, StopReason, Tool, Usage};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent applicable assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent applicable assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the message that provided usage, or `None`.
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: f64 = 4.0;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn safe_json_stringify(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as u64
}

fn estimate_text_and_image_content_tokens(content: &[Content]) -> u64 {
    let mut chars: usize = 0;
    for block in content {
        match block {
            Content::Text { text, .. } => chars += text.chars().count(),
            Content::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
            _ => {}
        }
    }
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

fn user_content_chars(content: &crate::types::UserContent) -> usize {
    match content {
        crate::types::UserContent::Text(text) => text.chars().count(),
        crate::types::UserContent::Blocks(blocks) => {
            let mut chars = 0;
            for block in blocks {
                match block {
                    Content::Text { text, .. } => chars += text.chars().count(),
                    Content::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                    _ => {}
                }
            }
            chars
        }
    }
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User { content, .. } => {
            (user_content_chars(content) as f64 / CHARS_PER_TOKEN).ceil() as u64
        }
        Message::ToolResult(tool_result) => {
            estimate_text_and_image_content_tokens(&tool_result.content)
        }
        Message::Assistant(assistant) => {
            let mut chars: usize = 0;
            for block in &assistant.content {
                match block {
                    Content::Text { text, .. } => chars += text.chars().count(),
                    Content::Thinking { thinking, .. } => chars += thinking.chars().count(),
                    Content::ToolCall {
                        name, arguments, ..
                    } => {
                        chars +=
                            name.chars().count() + safe_json_stringify(arguments).chars().count();
                    }
                    Content::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                }
            }
            (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
        }
    }
}

struct LastAssistantUsageInfo {
    usage: Usage,
    index: usize,
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Option<LastAssistantUsageInfo> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info: Option<LastAssistantUsageInfo> = None;

    for (i, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::User { timestamp, .. } => *timestamp as i64,
            Message::Assistant(assistant) => assistant.timestamp as i64,
            Message::ToolResult(tool_result) => tool_result.timestamp as i64,
        };
        if let Message::Assistant(assistant) = message {
            // A newer prefix message inserted after this response (e.g. a
            // compaction summary) invalidates its usage for the current prefix.
            let usage_applies_to_prefix = assistant.timestamp as i64 >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && assistant.stop_reason != StopReason::Aborted
                && assistant.stop_reason != StopReason::Error
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some(LastAssistantUsageInfo {
                    usage: assistant.usage.clone(),
                    index: i,
                });
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
    }

    usage_info
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some(usage_info) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage_info.usage);
        let mut trailing_tokens: u64 = 0;
        for message in &messages[usage_info.index + 1..] {
            trailing_tokens += estimate_message_tokens(message);
        }
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(usage_info.index),
        };
    }

    let mut tokens: u64 = 0;
    for message in messages {
        tokens += estimate_message_tokens(message);
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: &[Tool]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    let serialized =
        safe_json_stringify(&serde_json::to_value(tools).unwrap_or(serde_json::Value::Null));
    estimate_text_tokens(&serialized)
}

/// Estimate the token cost of a full context or a bare message list.
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages(&context.messages);

    if let Some(last_usage_index) = estimate.last_usage_index {
        // Tools added after the last usage block need adding to the estimate.
        let mut added_names = std::collections::HashSet::new();
        for message in &context.messages[last_usage_index + 1..] {
            if let Message::ToolResult(tool_result) = message {
                if let Some(names) = &tool_result.added_tool_names {
                    added_names.extend(names.iter().cloned());
                }
            }
        }
        let added_tools: Vec<Tool> = context
            .tools
            .iter()
            .filter(|tool| added_names.contains(&tool.name))
            .cloned()
            .collect();
        let added_tool_tokens = estimate_tools_tokens(&added_tools);
        return ContextUsageEstimate {
            tokens: estimate.tokens + added_tool_tokens,
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
            last_usage_index: estimate.last_usage_index,
        };
    }

    let prefix_tokens = context
        .system_prompt
        .as_deref()
        .map(estimate_text_tokens)
        .unwrap_or(0)
        + estimate_tools_tokens(&context.tools);

    ContextUsageEstimate {
        tokens: estimate.tokens + prefix_tokens,
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens + prefix_tokens,
        last_usage_index: estimate.last_usage_index,
    }
}
