//! Port of packages/coding-agent/src/core/compaction/utils.ts (pi v0.84.3):
//! file-operation tracking, conversation serialization, and the
//! summarization system prompt shared by compaction and branch
//! summarization.

use std::collections::BTreeSet;

use pillar_ai::text::content_text;
use pillar_ai::types::{Content, Message};

use crate::core::messages::CodingAgentMessage;

// ============================================================================
// File Operation Tracking
// ============================================================================

/// File operation sets extracted from tool calls (upstream `FileOperations`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

impl FileOperations {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Extract file operations from tool calls in an assistant message (upstream
/// `extractFileOpsFromMessage`).
pub fn extract_file_ops_from_message(message: &CodingAgentMessage, file_ops: &mut FileOperations) {
    let CodingAgentMessage::Base(Message::Assistant(assistant)) = message else {
        return;
    };
    for block in &assistant.content {
        let Content::ToolCall {
            name, arguments, ..
        } = block
        else {
            continue;
        };
        let Some(path) = arguments.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// Compute final file lists: readFiles (read only, not modified, sorted) and
/// modifiedFiles (edited or written, sorted) (upstream `computeFileLists`).
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let mut modified: BTreeSet<String> = file_ops.edited.clone();
    modified.extend(file_ops.written.iter().cloned());
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_files, modified_files)
}

/// Format file operations as XML tags for the summary (upstream
/// `formatFileOperations`).
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

// ============================================================================
// Message Serialization
// ============================================================================

/// Maximum characters for a tool result in serialized summaries.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Truncate text to a maximum character length for summarization. Keeps the
/// beginning and appends a truncation marker (upstream `truncateForSummary`).
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let total = text.chars().count();
    let truncated_chars = total - max_chars;
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n\n[... {truncated_chars} more characters truncated]")
}

/// Serialize LLM messages to text for summarization (upstream
/// `serializeConversation`). This prevents the model from treating it as a
/// conversation to continue. Call `convert_to_llm` first to handle custom
/// message types. Tool results are truncated to keep the summarization
/// request within reasonable token budgets.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                let blocks = match content {
                    pillar_ai::types::UserContent::Text(text) => vec![Content::text(text.clone())],
                    pillar_ai::types::UserContent::Blocks(blocks) => blocks.clone(),
                };
                let text = content_text(&blocks, "");
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();

                for block in &assistant.content {
                    match block {
                        Content::Thinking { thinking, .. } => {
                            thinking_parts.push(thinking.clone());
                        }
                        Content::ToolCall {
                            name, arguments, ..
                        } => {
                            let args_str = arguments
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| {
                                            format!(
                                                "{k}={}",
                                                serde_json::to_string(v).unwrap_or_default()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            tool_calls.push(format!("{name}({args_str})"));
                        }
                        _ => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::Text { .. }))
                {
                    parts.push(format!(
                        "[Assistant]: {}",
                        content_text(&assistant.content, "")
                    ));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = content_text(&result.content, "");
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}

// ============================================================================
// Summarization System Prompt
// ============================================================================

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";
