//! Parity tests for messages.ts (pi v0.84.3): custom message shapes, the
//! bash-execution text rendering, summary prefixes/suffixes, and the
//! convertToLlm transformer.

use pillar_ai::types::{Content, Message, UserContent};
use pillar_coding_agent::core::messages::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, BashExecutionMessage, BranchSummaryMessage,
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, CodingAgentMessage,
    CompactionSummaryMessage, CustomContent, bash_execution_to_text, convert_to_llm,
    create_branch_summary_message, create_compaction_summary_message, create_custom_message,
};

fn base_user(text: &str, timestamp: u64) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp,
    })
}

fn bash(msg: BashExecutionMessage) -> CodingAgentMessage {
    CodingAgentMessage::BashExecution(msg)
}

fn bash_msg() -> BashExecutionMessage {
    BashExecutionMessage {
        command: "ls -la".to_string(),
        output: "file1\nfile2".to_string(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 1000,
        exclude_from_context: false,
    }
}

// --- constants ----------------------------------------------------------------

#[test]
fn summary_prefixes_and_suffixes_match_upstream() {
    assert_eq!(
        COMPACTION_SUMMARY_PREFIX,
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
    );
    assert_eq!(COMPACTION_SUMMARY_SUFFIX, "\n</summary>");
    assert_eq!(
        BRANCH_SUMMARY_PREFIX,
        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n"
    );
    assert_eq!(BRANCH_SUMMARY_SUFFIX, "</summary>");
}

// --- bashExecutionToText -----------------------------------------------------------

#[test]
fn bash_execution_text_with_output_and_success() {
    let text = bash_execution_to_text(&bash_msg());
    assert_eq!(text, "Ran `ls -la`\n```\nfile1\nfile2\n```");
}

#[test]
fn bash_execution_text_without_output() {
    let mut msg = bash_msg();
    msg.output = String::new();
    assert_eq!(bash_execution_to_text(&msg), "Ran `ls -la`\n(no output)");
}

#[test]
fn bash_execution_text_cancelled() {
    let mut msg = bash_msg();
    msg.cancelled = true;
    let text = bash_execution_to_text(&msg);
    assert!(text.ends_with("(command cancelled)"));
}

#[test]
fn bash_execution_text_nonzero_exit_code() {
    let mut msg = bash_msg();
    msg.exit_code = Some(2);
    let text = bash_execution_to_text(&msg);
    assert!(text.ends_with("Command exited with code 2"));
}

#[test]
fn bash_execution_text_zero_exit_code_not_reported() {
    let msg = bash_msg();
    assert!(!bash_execution_to_text(&msg).contains("exited with code"));
}

#[test]
fn bash_execution_text_truncated_with_full_output_path() {
    let mut msg = bash_msg();
    msg.truncated = true;
    msg.full_output_path = Some("/tmp/out.txt".to_string());
    let text = bash_execution_to_text(&msg);
    assert!(
        text.contains("[Output truncated. Full output: /tmp/out.txt]"),
        "{text}"
    );
}

#[test]
fn bash_execution_text_truncated_without_path_omits_note() {
    let mut msg = bash_msg();
    msg.truncated = true;
    assert!(!bash_execution_to_text(&msg).contains("Output truncated"));
}

// --- constructors ---------------------------------------------------------------------

#[test]
fn create_branch_summary_message_shape() {
    let msg = create_branch_summary_message("summary text", "node-1", 5000);
    assert_eq!(
        msg,
        BranchSummaryMessage {
            summary: "summary text".to_string(),
            from_id: "node-1".to_string(),
            timestamp: 5000,
        }
    );
}

#[test]
fn create_compaction_summary_message_shape() {
    let msg = create_compaction_summary_message("summary", 12_345, 6000);
    assert_eq!(
        msg,
        CompactionSummaryMessage {
            summary: "summary".to_string(),
            tokens_before: 12_345,
            timestamp: 6000,
        }
    );
}

#[test]
fn create_custom_message_shape() {
    let msg = create_custom_message(
        "my-type",
        vec![CustomContent::Text("hello".to_string())],
        true,
        Some(serde_json::json!({"k": 1})),
        7000,
    );
    assert_eq!(msg.custom_type, "my-type");
    assert_eq!(msg.content, vec![CustomContent::Text("hello".to_string())]);
    assert!(msg.display);
    assert_eq!(msg.details, Some(serde_json::json!({"k": 1})));
    assert_eq!(msg.timestamp, 7000);
}

// --- convertToLlm -----------------------------------------------------------------------

#[test]
fn convert_passes_base_messages_through() {
    let messages = vec![
        base_user("hello", 1),
        CodingAgentMessage::Base(Message::User {
            content: UserContent::Text("second".to_string()),
            timestamp: 2,
        }),
    ];
    let converted = convert_to_llm(&messages);
    assert_eq!(converted.len(), 2);
    assert!(matches!(&converted[0], Message::User { .. }));
}

#[test]
fn convert_bash_execution_to_user_text() {
    let converted = convert_to_llm(&[bash(bash_msg())]);
    let Message::User { content, timestamp } = &converted[0] else {
        panic!("expected user message");
    };
    assert_eq!(*timestamp, 1000);
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    assert_eq!(blocks.len(), 1);
    let Content::Text { text, .. } = &blocks[0] else {
        panic!("expected text");
    };
    assert_eq!(text, "Ran `ls -la`\n```\nfile1\nfile2\n```");
}

#[test]
fn convert_skips_excluded_bash_executions() {
    let mut msg = bash_msg();
    msg.exclude_from_context = true;
    let converted = convert_to_llm(&[bash(msg), base_user("kept", 2)]);
    assert_eq!(converted.len(), 1, "excluded message dropped");
}

#[test]
fn convert_custom_message_to_user_content() {
    let custom = create_custom_message(
        "t",
        vec![
            CustomContent::Text("part1 ".to_string()),
            CustomContent::Image {
                data: "abc".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
        true,
        None,
        3000,
    );
    let converted = convert_to_llm(&[CodingAgentMessage::Custom(custom)]);
    let Message::User { content, timestamp } = &converted[0] else {
        panic!("expected user message");
    };
    assert_eq!(*timestamp, 3000);
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], Content::Text { .. }));
    assert!(matches!(&blocks[1], Content::Image { .. }));
}

#[test]
fn convert_branch_summary_wraps_with_prefix_and_suffix() {
    let msg = create_branch_summary_message("the summary", "n", 10);
    let converted = convert_to_llm(&[CodingAgentMessage::BranchSummary(msg)]);
    let Message::User { content, .. } = &converted[0] else {
        panic!("expected user message");
    };
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    let Content::Text { text, .. } = &blocks[0] else {
        panic!("expected text");
    };
    assert_eq!(
        text.as_str(),
        format!("{BRANCH_SUMMARY_PREFIX}the summary{BRANCH_SUMMARY_SUFFIX}")
    );
}

#[test]
fn convert_compaction_summary_wraps_with_prefix_and_suffix() {
    let msg = create_compaction_summary_message("compact summary", 999, 20);
    let converted = convert_to_llm(&[CodingAgentMessage::CompactionSummary(msg)]);
    let Message::User { content, .. } = &converted[0] else {
        panic!("expected user message");
    };
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    let Content::Text { text, .. } = &blocks[0] else {
        panic!("expected text");
    };
    assert_eq!(
        text.as_str(),
        format!("{COMPACTION_SUMMARY_PREFIX}compact summary{COMPACTION_SUMMARY_SUFFIX}")
    );
}

#[test]
fn convert_mixed_stream_preserves_order() {
    let messages = vec![
        base_user("first", 1),
        bash(bash_msg()),
        CodingAgentMessage::CompactionSummary(create_compaction_summary_message("s", 1, 3)),
        base_user("last", 4),
    ];
    let converted = convert_to_llm(&messages);
    assert_eq!(converted.len(), 4);
    // All are user messages in order.
    for message in &converted {
        assert!(matches!(message, Message::User { .. }));
    }
}
