//! Port of packages/coding-agent/src/core/messages.ts (pi v0.84.3): the
//! coding-agent custom message types and the transformer that converts them
//! (plus base agent messages) into LLM-compatible messages.
//!
//! divergence: upstream merges the custom types into `AgentMessage` via
//! declaration merging; the port defines a `CodingAgentMessage` enum that
//! wraps the pillar-ai `Message` variants plus the custom roles.

use pillar_ai::types::{Content, Message, UserContent};

/// Prefix wrapped around compaction summaries (upstream
/// `COMPACTION_SUMMARY_PREFIX`).
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Suffix wrapped around compaction summaries (upstream
/// `COMPACTION_SUMMARY_SUFFIX`).
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Prefix wrapped around branch summaries (upstream `BRANCH_SUMMARY_PREFIX`).
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";

/// Suffix wrapped around branch summaries (upstream `BRANCH_SUMMARY_SUFFIX`).
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// A bash execution via the `!` command (upstream `BashExecutionMessage`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
    /// If true, this message is excluded from LLM context (`!!` prefix).
    pub exclude_from_context: bool,
}

/// Content blocks usable in custom messages (upstream the
/// `TextContent | ImageContent` union).
#[derive(Debug, Clone, PartialEq)]
pub enum CustomContent {
    Text(String),
    Image { data: String, mime_type: String },
}

impl CustomContent {
    fn to_content(&self) -> Content {
        match self {
            CustomContent::Text(text) => Content::text(text.clone()),
            CustomContent::Image { data, mime_type } => Content::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            },
        }
    }
}

/// An extension-injected message (upstream `CustomMessage`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: Vec<CustomContent>,
    pub display: bool,
    pub details: Option<serde_json::Value>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// A branch summary marker (upstream `BranchSummaryMessage`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// A compaction summary marker (upstream `CompactionSummaryMessage`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// The coding-agent message union (upstream `AgentMessage` extended via
/// declaration merging with the custom roles).
#[derive(Debug, Clone, PartialEq)]
pub enum CodingAgentMessage {
    Base(Message),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}

/// Convert a bash execution to user message text for LLM context (upstream
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
    } else if let Some(exit_code) = msg.exit_code {
        if exit_code != 0 {
            text.push_str(&format!("\n\nCommand exited with code {exit_code}"));
        }
    }
    if msg.truncated {
        if let Some(path) = &msg.full_output_path {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
    }
    text
}

/// Build a branch summary message from an ISO timestamp (upstream
/// `createBranchSummaryMessage`).
///
/// divergence: upstream parses an ISO string; the port takes the resolved
/// millisecond timestamp directly.
pub fn create_branch_summary_message(
    summary: &str,
    from_id: &str,
    timestamp_ms: u64,
) -> BranchSummaryMessage {
    BranchSummaryMessage {
        summary: summary.to_string(),
        from_id: from_id.to_string(),
        timestamp: timestamp_ms,
    }
}

/// Build a compaction summary message (upstream
/// `createCompactionSummaryMessage`).
pub fn create_compaction_summary_message(
    summary: &str,
    tokens_before: u64,
    timestamp_ms: u64,
) -> CompactionSummaryMessage {
    CompactionSummaryMessage {
        summary: summary.to_string(),
        tokens_before,
        timestamp: timestamp_ms,
    }
}

/// Build a custom message (upstream `createCustomMessage`).
pub fn create_custom_message(
    custom_type: &str,
    content: Vec<CustomContent>,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp_ms: u64,
) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.to_string(),
        content,
        display,
        details,
        timestamp: timestamp_ms,
    }
}

/// Transform coding-agent messages (including the custom types) into
/// LLM-compatible messages (upstream `convertToLlm`).
///
/// Used by the agent's transform-to-LLM option, compaction summary
/// generation, and extensions/tools.
pub fn convert_to_llm(messages: &[CodingAgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| match m {
            CodingAgentMessage::BashExecution(msg) => {
                // Skip messages excluded from context (`!!` prefix).
                if msg.exclude_from_context {
                    return None;
                }
                Some(Message::User {
                    content: UserContent::Blocks(vec![Content::text(bash_execution_to_text(msg))]),
                    timestamp: msg.timestamp,
                })
            }
            CodingAgentMessage::Custom(msg) => {
                let content: Vec<Content> = if msg.content.is_empty() {
                    vec![Content::text(String::new())]
                } else {
                    msg.content.iter().map(|c| c.to_content()).collect()
                };
                Some(Message::User {
                    content: UserContent::Blocks(content),
                    timestamp: msg.timestamp,
                })
            }
            CodingAgentMessage::BranchSummary(msg) => Some(Message::User {
                content: UserContent::Blocks(vec![Content::text(format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    msg.summary
                ))]),
                timestamp: msg.timestamp,
            }),
            CodingAgentMessage::CompactionSummary(msg) => Some(Message::User {
                content: UserContent::Blocks(vec![Content::text(format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    msg.summary
                ))]),
                timestamp: msg.timestamp,
            }),
            CodingAgentMessage::Base(message) => Some(message.clone()),
        })
        .collect()
}
