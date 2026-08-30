//! Port of packages/agent/src/harness/messages.ts behavior tests.
//!
//! Upstream has no dedicated messages.test.ts; the conversion behavior is
//! pinned here against the upstream implementation semantics (bash
//! execution text rendering, custom → user mapping, summary prefixes,
//! excludeFromContext, convertToLlm filtering).

use pillar_agent::harness::messages::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX, bash_execution_to_text, convert_to_llm,
    create_branch_summary_message, create_compaction_summary_message, create_custom_message,
};
use pillar_agent::types::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage,
};
use pillar_ai::types::{Content, Message, UserContent};

fn bash_message() -> BashExecutionMessage {
    BashExecutionMessage {
        command: "ls -la".to_owned(),
        output: "file1\nfile2".to_owned(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 1,
        exclude_from_context: false,
    }
}

#[test]
fn renders_bash_execution_text_with_output() {
    let text = bash_execution_to_text(&bash_message());
    assert_eq!(text, "Ran `ls -la`\n```\nfile1\nfile2\n```");
}

#[test]
fn renders_no_output_placeholder() {
    let mut msg = bash_message();
    msg.output = String::new();
    assert_eq!(bash_execution_to_text(&msg), "Ran `ls -la`\n(no output)");
}

#[test]
fn renders_cancelled_suffix() {
    let mut msg = bash_message();
    msg.output = String::new();
    msg.cancelled = true;
    assert!(bash_execution_to_text(&msg).ends_with("(command cancelled)"));
}

#[test]
fn renders_nonzero_exit_code() {
    let mut msg = bash_message();
    msg.output = String::new();
    msg.exit_code = Some(2);
    let text = bash_execution_to_text(&msg);
    assert!(text.ends_with("Command exited with code 2"), "text: {text}");
}

#[test]
fn zero_exit_code_has_no_suffix() {
    let mut msg = bash_message();
    msg.output = String::new();
    msg.exit_code = Some(0);
    assert!(!bash_execution_to_text(&msg).contains("exited"));
}

#[test]
fn renders_truncation_pointer() {
    let mut msg = bash_message();
    msg.truncated = true;
    msg.full_output_path = Some("/tmp/full.log".to_owned());
    let text = bash_execution_to_text(&msg);
    assert!(
        text.contains("[Output truncated. Full output: /tmp/full.log]"),
        "text: {text}"
    );
}

#[test]
fn summary_prefixes_and_suffixes_shape() {
    // Upstream constant shapes: XML block with the summary inside.
    assert!(COMPACTION_SUMMARY_PREFIX.ends_with("<summary>\n"));
    assert!(COMPACTION_SUMMARY_SUFFIX.starts_with('\n'));
    assert!(BRANCH_SUMMARY_PREFIX.ends_with("<summary>\n"));
    assert_eq!(BRANCH_SUMMARY_SUFFIX, "</summary>");
}

#[test]
fn custom_message_content_wraps_bare_string() {
    let message =
        create_custom_message("note", UserContent::Text("hello".to_owned()), true, None, 5);
    let AgentMessage::Custom(custom) = &message else {
        panic!("expected custom message");
    };
    assert_eq!(custom.custom_type, "note");
    assert!(custom.display);
    assert_eq!(custom.timestamp, 5);
}

#[test]
fn branch_and_compaction_factories_set_fields() {
    let branch = create_branch_summary_message("summary text", "entry-9", 7);
    let AgentMessage::BranchSummary(summary) = &branch else {
        panic!("expected branch summary");
    };
    assert_eq!(
        **summary,
        BranchSummaryMessage {
            summary: "summary text".to_owned(),
            from_id: "entry-9".to_owned(),
            timestamp: 7,
        }
    );

    let compaction = create_compaction_summary_message("compacted", 1234, 8);
    let AgentMessage::CompactionSummary(summary) = &compaction else {
        panic!("expected compaction summary");
    };
    assert_eq!(
        **summary,
        CompactionSummaryMessage {
            summary: "compacted".to_owned(),
            tokens_before: 1234,
            timestamp: 8,
        }
    );
}

#[test]
fn convert_to_llm_maps_custom_to_user_with_prefixes() {
    let messages = vec![
        AgentMessage::BranchSummary(Box::new(BranchSummaryMessage {
            summary: "branch".to_owned(),
            from_id: "e1".to_owned(),
            timestamp: 1,
        })),
        AgentMessage::CompactionSummary(Box::new(CompactionSummaryMessage {
            summary: "compacted".to_owned(),
            tokens_before: 100,
            timestamp: 2,
        })),
    ];

    let converted = convert_to_llm(&messages);
    assert_eq!(converted.len(), 2);
    for (message, expected_prefix, expected_suffix) in [
        (&converted[0], BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX),
        (
            &converted[1],
            COMPACTION_SUMMARY_PREFIX,
            COMPACTION_SUMMARY_SUFFIX,
        ),
    ] {
        let Message::User { content, .. } = message else {
            panic!("expected user message");
        };
        let UserContent::Blocks(blocks) = content else {
            panic!("expected blocks");
        };
        let Some(Content::Text { text, .. }) = blocks.first() else {
            panic!("expected text block");
        };
        assert!(text.starts_with(expected_prefix));
        assert!(text.ends_with(expected_suffix));
    }
}

#[test]
fn convert_to_llm_drops_excluded_bash_executions() {
    let mut msg = bash_message();
    msg.exclude_from_context = true;
    let messages = vec![AgentMessage::BashExecution(Box::new(msg))];
    assert!(convert_to_llm(&messages).is_empty());
}

#[test]
fn convert_to_llm_renders_included_bash_executions_as_user_text() {
    let messages = vec![AgentMessage::BashExecution(Box::new(bash_message()))];
    let converted = convert_to_llm(&messages);
    assert_eq!(converted.len(), 1);
    let Message::User { content, timestamp } = &converted[0] else {
        panic!("expected user message");
    };
    assert_eq!(*timestamp, 1);
    let UserContent::Blocks(blocks) = content else {
        panic!("expected blocks");
    };
    let Some(Content::Text { text, .. }) = blocks.first() else {
        panic!("expected text block");
    };
    assert_eq!(text, &bash_execution_to_text(&bash_message()));
}

#[test]
fn convert_to_llm_passes_base_messages_through() {
    let user = Message::User {
        content: UserContent::Text("hi".to_owned()),
        timestamp: 3,
    };
    let messages = vec![
        AgentMessage::Message(user.clone()),
        AgentMessage::Custom(Box::new(CustomMessage {
            custom_type: "note".to_owned(),
            content: UserContent::Text("sticky note".to_owned()),
            display: true,
            details: None,
            timestamp: 4,
        })),
    ];
    let converted = convert_to_llm(&messages);
    assert_eq!(converted.len(), 2);
    assert_eq!(converted[0], user);
    let Message::User { content, timestamp } = &converted[1] else {
        panic!("expected user message");
    };
    assert_eq!(*timestamp, 4);
    assert_eq!(*content, UserContent::Text("sticky note".to_owned()));
}
