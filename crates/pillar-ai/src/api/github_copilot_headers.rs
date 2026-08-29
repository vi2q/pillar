//! Port of packages/ai/src/api/github-copilot-headers.ts (pi v0.84.3).

use crate::types::{Message, UserContent};

/// Copilot expects X-Initiator to indicate whether the request is
/// user-initiated or agent-initiated (e.g. follow-up after assistant/tool
/// messages).
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    let last_is_user = matches!(messages.last(), Some(Message::User { .. }));
    if last_is_user { "user" } else { "agent" }
}

fn blocks_have_image(blocks: &[crate::types::Content]) -> bool {
    blocks
        .iter()
        .any(|block| matches!(block, crate::types::Content::Image { .. }))
}

/// Copilot requires Copilot-Vision-Request header when sending images.
pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User { content, .. } => match content {
            UserContent::Text(_) => false,
            UserContent::Blocks(blocks) => blocks_have_image(blocks),
        },
        Message::ToolResult(tool_result) => blocks_have_image(&tool_result.content),
        _ => false,
    })
}

/// Dynamic per-request headers Copilot requires.
pub fn build_copilot_dynamic_headers(
    messages: &[Message],
    has_images: bool,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "X-Initiator".to_string(),
            infer_copilot_initiator(messages).to_string(),
        ),
        (
            "Openai-Intent".to_string(),
            "conversation-edits".to_string(),
        ),
    ];
    if has_images {
        headers.push(("Copilot-Vision-Request".to_string(), "true".to_string()));
    }
    headers
}
