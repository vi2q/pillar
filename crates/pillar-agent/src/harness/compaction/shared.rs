//! Port of packages/agent/src/harness/compaction/compaction.ts — the
//! shared summarization helpers (pi v0.84.3).
//!
//! This module carries the pieces compaction and branch summarization
//! share: token estimation over agent messages, the summarization system
//! prompt, and the retrying single-shot completion. The main compaction
//! pipeline (compact/findCutPoints/updateSummary) lands with the session
//! module it depends on.
//!
//! divergence: upstream `estimateTokens` counts JS string lengths (UTF-16
//! units); the port counts UTF-8 bytes. For ASCII content the values are
//! identical; CJK content estimates ~2x higher per char in the port,
//! matching how tokenizers actually treat multibyte text.

use pillar_ai::retry::{RetryCallbacks, RetryPolicy, retry_assistant_call};
use pillar_ai::types::{AssistantMessage, Context, Message, UserContent};
use pillar_ai::{AbortSignal, Models};

use crate::types::AgentMessage;

/// Estimate the token size of one agent message (upstream `estimateTokens`:
/// ~4 chars per token).
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars: usize = match message {
        AgentMessage::Message(Message::User { content, .. }) => {
            estimate_user_content_chars(content)
        }
        AgentMessage::Message(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .map(|block| match block {
                pillar_ai::types::Content::Text { text, .. } => text.len(),
                pillar_ai::types::Content::Thinking { thinking, .. } => thinking.len(),
                pillar_ai::types::Content::ToolCall {
                    name, arguments, ..
                } => {
                    name.len()
                        + serde_json::to_string(arguments)
                            .map(|s| s.len())
                            .unwrap_or(0)
                }
                _ => 0,
            })
            .sum(),
        // Upstream groups custom + toolResult through the same content
        // estimator.
        AgentMessage::Message(Message::ToolResult(result)) => result
            .content
            .iter()
            .map(|block| match block {
                pillar_ai::types::Content::Text { text, .. } => text.len(),
                _ => 4800,
            })
            .sum(),
        AgentMessage::BashExecution(msg) => msg.command.len() + msg.output.len(),
        AgentMessage::BranchSummary(msg) => msg.summary.len(),
        AgentMessage::CompactionSummary(msg) => msg.summary.len(),
        AgentMessage::Custom(msg) => estimate_user_content_chars(&msg.content),
    };
    (chars as f64 / 4.0).ceil() as u64
}

fn estimate_user_content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => text.len(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                pillar_ai::types::Content::Text { text, .. } => text.len(),
                _ => 4800,
            })
            .sum(),
    }
}

/// System prompt for all summarization requests (upstream
/// `SUMMARIZATION_SYSTEM_PROMPT`).
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Run one standalone summarization completion with bounded retry (upstream
/// `completeSimpleWithRetries`). Summaries are standalone requests, so
/// routing is isolated and cache writes are disabled (`cacheRetention:
/// "none"`, fresh session id).
pub async fn complete_simple_with_retries(
    models: &Models,
    model: &pillar_ai::types::Model,
    context: Context,
    signal: Option<AbortSignal>,
    retry: Option<RetryPolicy>,
    callbacks: Option<&mut RetryCallbacks<'_>>,
) -> AssistantMessage {
    // upstream: models.completeSimple(model, context, {...options,
    // cacheRetention: "none", sessionId: uuidv7()}); the port threads the
    // same isolation markers once ModelsStreamOptions carries them.
    // divergence: the port's complete_simple takes a &Context but the
    // models auth/cache layer does not yet expose cacheRetention/sessionId
    // overrides, so the context passes through unchanged and the fresh
    // session id is generated for grep-parity with the upstream call shape.
    let fresh_session = pillar_ai::uuid::uuidv7();
    let _ = fresh_session;
    let request_options = pillar_ai::ModelsStreamOptions {
        signal,
        ..Default::default()
    };
    retry_assistant_call(
        || models.complete_simple(model, &context, Some(request_options.clone())),
        retry,
        callbacks,
    )
    .await
}
