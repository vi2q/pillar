//! Port of packages/agent/src/harness/messages.ts (pi v0.84.3).
//!
//! Harness custom message types (folded into
//! [`AgentMessage`](crate::types::AgentMessage)), their LLM conversion
//! helpers, and the harness-level `convertToLlm` used by the AgentHarness.

use pillar_ai::types::Message;

use crate::types::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage,
};

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";

pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Render a bash execution message as user-visible text (upstream
/// `bashExecutionToText`).
pub fn bash_execution_to_text(msg: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", msg.command);
    if !msg.output.is_empty() {
        text.push_str(&format!("```\n{}\n```", msg.output));
    } else {
        text.push_str("(no output)");
    }
    if msg.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = msg.exit_code.filter(|code| *code != 0) {
        text.push_str(&format!("\n\nCommand exited with code {exit_code}"));
    }
    if msg.truncated
        && let Some(full_output_path) = &msg.full_output_path
    {
        text.push_str(&format!(
            "\n\n[Output truncated. Full output: {full_output_path}]"
        ));
    }
    text
}

/// Build a [`AgentMessage::BranchSummary`].
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::BranchSummary(Box::new(BranchSummaryMessage {
        summary: summary.into(),
        from_id: from_id.into(),
        timestamp,
    }))
}

/// Build a [`AgentMessage::CompactionSummary`].
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: u64,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::CompactionSummary(Box::new(CompactionSummaryMessage {
        summary: summary.into(),
        tokens_before,
        timestamp,
    }))
}

/// Build a [`AgentMessage::Custom`].
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: pillar_ai::types::UserContent,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Custom(Box::new(CustomMessage {
        custom_type: custom_type.into(),
        content,
        display,
        details,
        timestamp,
    }))
}

/// Harness-level conversion to LLM messages (upstream harness
/// `convertToLlm`): custom entries map to user messages, excluded bash
/// executions are dropped, base messages pass through.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::BashExecution(msg) => {
                if msg.exclude_from_context {
                    None
                } else {
                    Some(Message::User {
                        content: pillar_ai::types::UserContent::Blocks(vec![
                            pillar_ai::types::Content::text(bash_execution_to_text(msg)),
                        ]),
                        timestamp: msg.timestamp,
                    })
                }
            }
            AgentMessage::Custom(msg) => Some(Message::User {
                content: msg.content.clone(),
                timestamp: msg.timestamp,
            }),
            AgentMessage::BranchSummary(msg) => Some(Message::User {
                content: pillar_ai::types::UserContent::Blocks(vec![
                    pillar_ai::types::Content::text(format!(
                        "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                        msg.summary
                    )),
                ]),
                timestamp: msg.timestamp,
            }),
            AgentMessage::CompactionSummary(msg) => Some(Message::User {
                content: pillar_ai::types::UserContent::Blocks(vec![
                    pillar_ai::types::Content::text(format!(
                        "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                        msg.summary
                    )),
                ]),
                timestamp: msg.timestamp,
            }),
            AgentMessage::Message(message) => Some(message.clone()),
        })
        .collect()
}
