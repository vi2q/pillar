//! Port of packages/agent/src/harness/compaction/utils.ts (pi v0.84.3).
//!
//! File-operation accumulation and conversation serialization helpers used
//! by compaction and branch summarization.

use std::collections::BTreeSet;

use pillar_ai::text::content_text;
use pillar_ai::types::{Content, Message};

use crate::types::AgentMessage;

/// File paths touched by a session branch or compaction range. Sets are
/// ordered (upstream `Set` iteration order is insertion order; the port
/// uses `BTreeSet` so the derived sorted output is stable).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOperations {
    /// Files read but not necessarily modified.
    pub read: BTreeSet<String>,
    /// Files written by full-file write operations.
    pub written: BTreeSet<String>,
    /// Files modified by edit operations.
    pub edited: BTreeSet<String>,
}

/// Create an empty file-operation accumulator.
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// Add file operations from assistant tool calls to an accumulator.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Message(Message::Assistant(assistant)) = message else {
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
                file_ops.read.insert(path.to_owned());
            }
            "write" => {
                file_ops.written.insert(path.to_owned());
            }
            "edit" => {
                file_ops.edited.insert(path.to_owned());
            }
            _ => {}
        }
    }
}

/// Compute sorted read-only and modified file lists from accumulated
/// operations.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .cloned()
        .collect();
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_files, modified_files)
}

/// Format file lists as summary metadata tags.
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

const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn safe_json_stringify(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_owned();
    }
    let truncated_chars = text.len() - max_chars;
    format!(
        "{}\n\n[... {truncated_chars} more characters truncated]",
        &text[..max_chars]
    )
}

/// Serialize LLM messages to plain text for summarization prompts.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                let text = match content {
                    pillar_ai::types::UserContent::Text(text) => text.clone(),
                    pillar_ai::types::UserContent::Blocks(blocks) => content_text(blocks, ""),
                };
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();
                let mut has_text = false;
                let mut text_parts: Vec<String> = Vec::new();

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
                                .map(|object| {
                                    object
                                        .iter()
                                        .map(|(k, v)| format!("{k}={}", safe_json_stringify(v)))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            tool_calls.push(format!("{name}({args_str})"));
                        }
                        Content::Text { text, .. } => {
                            has_text = true;
                            text_parts.push(text.clone());
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
                if has_text {
                    parts.push(format!("[Assistant]: {}", text_parts.join("")));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let content = content_text(&result.content, "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_ai::types::Usage;
    use std::collections::BTreeSet;

    fn usage() -> Usage {
        Usage::default()
    }

    /// Upstream behavior: read/write/edit tool calls accumulate into the
    /// right buckets; non-path calls are ignored.
    #[test]
    fn extracts_file_ops_from_assistant_tool_calls() {
        let assistant = pillar_ai::types::AssistantMessage {
            content: vec![
                Content::tool_call("t1", "read", serde_json::json!({"path": "/a.ts"})),
                Content::tool_call("t2", "write", serde_json::json!({"path": "/b.ts"})),
                Content::tool_call("t3", "edit", serde_json::json!({"path": "/c.ts"})),
                Content::tool_call("t4", "bash", serde_json::json!({"command": "ls"})),
            ],
            api: "openai-completions".into(),
            provider: "openai".into(),
            model: "m".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: usage(),
            stop_reason: pillar_ai::types::StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        };
        let message = AgentMessage::Message(Message::Assistant(Box::new(assistant)));

        let mut file_ops = create_file_ops();
        extract_file_ops_from_message(&message, &mut file_ops);

        let mut expected_read = BTreeSet::new();
        expected_read.insert("/a.ts".to_owned());
        assert_eq!(file_ops.read, expected_read);
        assert!(file_ops.written.contains("/b.ts"));
        assert!(file_ops.edited.contains("/c.ts"));

        let (read_files, modified_files) = compute_file_lists(&file_ops);
        assert_eq!(read_files, vec!["/a.ts".to_owned()]);
        assert_eq!(modified_files, vec!["/b.ts".to_owned(), "/c.ts".to_owned()]);
    }

    #[test]
    fn formats_file_operations_sections() {
        assert_eq!(format_file_operations(&[], &[]), "");
        let formatted = format_file_operations(
            &["/a.ts".to_owned()],
            &["/b.ts".to_owned(), "/c.ts".to_owned()],
        );
        assert_eq!(
            formatted,
            "\n\n<read-files>\n/a.ts\n</read-files>\n\n<modified-files>\n/b.ts\n/c.ts\n</modified-files>"
        );
    }

    #[test]
    fn serializes_conversation_roles() {
        let user = Message::User {
            content: pillar_ai::types::UserContent::Text("hello".to_owned()),
            timestamp: 1,
        };
        let assistant = pillar_ai::types::AssistantMessage {
            content: vec![
                Content::thinking("thought"),
                Content::text("answer"),
                Content::tool_call("t1", "read", serde_json::json!({"path": "/a.ts"})),
            ],
            api: "openai-completions".into(),
            provider: "openai".into(),
            model: "m".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: usage(),
            stop_reason: pillar_ai::types::StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 2,
        };
        let tool_result = pillar_ai::types::ToolResultMessage {
            tool_call_id: "t1".to_owned(),
            tool_name: "read".to_owned(),
            content: vec![Content::text("contents")],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 3,
        };
        let messages = vec![
            user,
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(tool_result)),
        ];
        let serialized = serialize_conversation(&messages);
        assert!(serialized.contains("[User]: hello"), "{serialized}");
        assert!(serialized.contains("[Assistant thinking]: thought"));
        assert!(serialized.contains("[Assistant]: answer"));
        assert!(
            serialized.contains("[Assistant tool calls]: read(path=\"/a.ts\")"),
            "{serialized}"
        );
        assert!(serialized.contains("[Tool result]: contents"));
    }
}
