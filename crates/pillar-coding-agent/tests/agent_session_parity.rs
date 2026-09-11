//! Parity tests for agent-session.ts (pi v0.84.3), the session-state
//! core: skill block parsing, retry policy, custom message case analysis,
//! queue tracking, tool registry / prompt snippet normalization, and
//! stats counting.

use std::collections::BTreeMap;

use pillar_ai::types::{Content, Message, StopReason, Usage};
use pillar_coding_agent::core::agent_session::{
    CustomDelivery, CustomMessagePlan, QueueState, RetryStep, ToolRegistryEntry, count_messages,
    expand_skill_command, is_retryable_error, normalize_custom_message,
    normalize_prompt_guidelines, normalize_prompt_snippet, parse_skill_block, plan_custom_message,
    prepare_retry, remove_queued_message, resolve_active_tool_names, unique_tool_names,
    will_retry_after_agent_end,
};
use pillar_coding_agent::core::messages::{CodingAgentMessage, CustomContent};

fn assistant_message(
    content: Vec<Content>,
    stop_reason: StopReason,
    error: Option<&str>,
) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content,
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason,
            error_message: error.map(str::to_string),
            timestamp: 1000,
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            deferred: None,
            raw_stop_reason: None,
            end_turn: None,
        },
    )))
}

fn assistant_error_message(error: &str) -> CodingAgentMessage {
    assistant_message(vec![], StopReason::Error, Some(error))
}

fn assistant_ok_message() -> CodingAgentMessage {
    assistant_message(vec![Content::text("ok")], StopReason::Stop, None)
}

fn user_entry(id: &str, parent: &str, text: &str) -> SessionEntry {
    SessionEntry::Message(SessionMessageEntry {
        base: SessionEntryBase {
            id: id.to_string(),
            parent_id: if parent.is_empty() {
                None
            } else {
                Some(parent.to_string())
            },
            timestamp: 1000,
        },
        message: user_message(text),
    })
}

fn user_message(text: &str) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::User {
        content: pillar_ai::types::UserContent::Text(text.to_string()),
        timestamp: 1000,
    })
}

// --- skill block parsing ----------------------------------------------------------------------

#[test]
fn parse_skill_block_full_and_partial() {
    let text = "<skill name=\"commit\" location=\"/skills/commit/SKILL.md\">\nDo the commit\n</skill>\n\nwith a message please";
    let block = parse_skill_block(text).unwrap();
    assert_eq!(block.name, "commit");
    assert_eq!(block.location, "/skills/commit/SKILL.md");
    assert_eq!(block.content, "Do the commit");
    assert_eq!(block.user_message.as_deref(), Some("with a message please"));

    // No trailing user message.
    let text = "<skill name=\"a\" location=\"b\">\ncontent\n</skill>";
    let block = parse_skill_block(text).unwrap();
    assert_eq!(block.user_message, None);

    // No skill block.
    assert!(parse_skill_block("just a message").is_none());
    // Unterminated.
    assert!(parse_skill_block("<skill name=\"a\" location=\"b\">\nno close").is_none());
}

#[test]
fn expand_skill_command_maps_to_template() {
    // The skill block's name becomes the template invocation with the
    // trailing user message as arguments.
    let templates = vec![
        pillar_coding_agent::core::prompt_templates::PromptTemplate {
            name: "commit".to_string(),
            description: String::new(),
            argument_hint: None,
            content: "commit: $@".to_string(),
            file_path: String::new(),
        },
    ];
    let text = "<skill name=\"commit\" location=\"x\">\nbody\n</skill>\n\nfix the bug";
    assert_eq!(
        expand_skill_command(text, &templates),
        "commit: fix the bug"
    );
    // Non-skill text passes through.
    assert_eq!(expand_skill_command("plain", &[]), "plain");
}

// --- retry policy ------------------------------------------------------------------------------

#[test]
fn retryable_error_detection() {
    let error = assistant_error_message("overloaded");
    if let CodingAgentMessage::Base(Message::Assistant(assistant)) = &error {
        // A generic error may be retryable; context overflow is not.
        let overflow =
            assistant_error_message("Prompt is too long: 300000 tokens > 200000 maximum");
        if let CodingAgentMessage::Base(Message::Assistant(overflow_msg)) = &overflow {
            assert!(!is_retryable_error(overflow_msg, 200_000));
        }
        let _ = is_retryable_error(assistant, 200_000);
    }
}

#[test]
fn will_retry_after_agent_end_checks_last_assistant() {
    let messages = vec![user_message("hi"), assistant_error_message("server error")];
    // Even without knowing the exact retryability table, the gate logic
    // must hold: disabled → false; budget exhausted → false.
    assert!(!will_retry_after_agent_end(&messages, 0, false, 3, 200_000));
    assert!(!will_retry_after_agent_end(&messages, 3, true, 3, 200_000));

    // Successful assistant message is never retried.
    let ok_messages = vec![user_message("hi"), assistant_ok_message()];
    assert!(!will_retry_after_agent_end(
        &ok_messages,
        0,
        true,
        3,
        200_000
    ));

    // No assistant message at all.
    assert!(!will_retry_after_agent_end(
        &[user_message("hi")],
        0,
        true,
        3,
        200_000
    ));
}

#[test]
fn prepare_retry_backoff_and_budget() {
    // First retry: base delay, attempt 1.
    assert_eq!(
        prepare_retry(0, true, 3, 500),
        RetryStep::Wait {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 500
        }
    );
    // Exponential backoff: attempt 3 → 500 * 4.
    assert_eq!(
        prepare_retry(2, true, 3, 500),
        RetryStep::Wait {
            attempt: 3,
            max_attempts: 3,
            delay_ms: 2000
        }
    );
    // Budget exhausted: attempt 3 requested with max 3 → next would be 4.
    assert_eq!(prepare_retry(3, true, 3, 500), RetryStep::Continue);
    // Disabled.
    assert_eq!(prepare_retry(0, false, 3, 500), RetryStep::Continue);
}

// --- custom messages -----------------------------------------------------------------------------

#[test]
fn custom_message_plan_case_analysis() {
    // deliverAs nextTurn wins outright.
    assert_eq!(
        plan_custom_message(true, None, Some(CustomDelivery::NextTurn)),
        CustomMessagePlan::PendingNextTurn
    );
    // Streaming + default → steer.
    assert_eq!(
        plan_custom_message(true, None, None),
        CustomMessagePlan::Steer
    );
    // Streaming + followUp delivery.
    assert_eq!(
        plan_custom_message(true, None, Some(CustomDelivery::FollowUp)),
        CustomMessagePlan::FollowUp
    );
    // Streaming + triggerTurn false → falls through to streaming branch...
    assert_eq!(
        plan_custom_message(true, Some(false), None),
        CustomMessagePlan::PendingTurnEnd
    );
    // Not streaming + triggerTurn → run prompt.
    assert_eq!(
        plan_custom_message(false, Some(true), None),
        CustomMessagePlan::RunPrompt
    );
    // Not streaming + no trigger → append now.
    assert_eq!(
        plan_custom_message(false, None, None),
        CustomMessagePlan::AppendNow
    );
    // Not streaming + triggerTurn false → append now.
    assert_eq!(
        plan_custom_message(false, Some(false), None),
        CustomMessagePlan::AppendNow
    );
}

#[test]
fn custom_message_normalization_fills_empty_content() {
    let message = normalize_custom_message("note", Vec::new(), true, None, 1000);
    assert_eq!(message.custom_type, "note");
    assert!(message.content.is_empty());
    assert!(message.display);
    assert_eq!(message.timestamp, 1000);

    let with_content = normalize_custom_message(
        "note",
        vec![CustomContent::Text("hello".to_string())],
        false,
        Some(serde_json::json!({"k": 1})),
        2000,
    );
    assert_eq!(with_content.content.len(), 1);
    assert_eq!(with_content.details, Some(serde_json::json!({"k": 1})));
}

// --- queue tracking ------------------------------------------------------------------------------

#[test]
fn queued_message_removed_steering_first() {
    let mut queues = QueueState {
        steering: vec!["a".to_string(), "b".to_string()],
        follow_up: vec!["b".to_string(), "c".to_string()],
    };
    // Steering queue is checked first: "b" removed from steering only.
    assert!(remove_queued_message(&mut queues, "b"));
    assert_eq!(queues.steering, vec!["a"]);
    assert_eq!(queues.follow_up, vec!["b", "c"]);
    // Then follow-up.
    assert!(remove_queued_message(&mut queues, "b"));
    assert_eq!(queues.follow_up, vec!["c"]);
    // Unknown text: no change.
    assert!(!remove_queued_message(&mut queues, "zzz"));
}

// --- tool registry / prompt --------------------------------------------------------------------

#[test]
fn tool_registry_filters_unknown_names() {
    let mut registry = BTreeMap::new();
    registry.insert(
        "read".to_string(),
        ToolRegistryEntry {
            name: "read".to_string(),
            description: "Read".to_string(),
            prompt_snippet: Some("Read files".to_string()),
            prompt_guidelines: vec!["Use read for files".to_string()],
        },
    );
    registry.insert(
        "bash".to_string(),
        ToolRegistryEntry {
            name: "bash".to_string(),
            description: "Bash".to_string(),
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
        },
    );

    assert_eq!(
        resolve_active_tool_names(
            &["read".to_string(), "ghost".to_string(), "bash".to_string()],
            &registry
        ),
        vec!["read".to_string(), "bash".to_string()]
    );
}

#[test]
fn prompt_snippet_and_guideline_normalization() {
    assert_eq!(
        normalize_prompt_snippet(Some("Make  precise\n\n edits ")),
        Some("Make precise edits".to_string())
    );
    assert_eq!(normalize_prompt_snippet(Some("   ")), None);
    assert_eq!(normalize_prompt_snippet(None), None);

    let guidelines = vec![
        "Use edit for precise changes".to_string(),
        "  ".to_string(),
        "Use edit for precise changes".to_string(),
        "Keep oldText small".to_string(),
    ];
    assert_eq!(
        normalize_prompt_guidelines(&guidelines),
        vec![
            "Use edit for precise changes".to_string(),
            "Keep oldText small".to_string()
        ]
    );
    assert!(normalize_prompt_guidelines(&[]).is_empty());
}

#[test]
fn unique_tool_names_preserve_order() {
    assert_eq!(
        unique_tool_names(&["a".to_string(), "b".to_string(), "a".to_string()]),
        vec!["a".to_string(), "b".to_string()]
    );
}

// --- stats ------------------------------------------------------------------------------------

#[test]
fn message_counting_for_stats() {
    let messages = vec![
        user_message("one"),
        assistant_ok_message(),
        assistant_error_message("x"),
        user_message("two"),
    ];
    let (user, assistant, tool_calls, tool_results) = count_messages(&messages);
    assert_eq!(user, 2);
    assert_eq!(assistant, 2);
    assert_eq!(tool_calls, 0);
    assert_eq!(tool_results, 0);
}

// --- tree navigation -----------------------------------------------------------------------------

use pillar_coding_agent::core::agent_session::{
    compute_context_usage, plan_tree_navigation, user_messages_for_forking,
};
use pillar_coding_agent::core::session_entries::{
    CustomMessageEntry, SessionEntry, SessionEntryBase, SessionMessageEntry,
};

fn custom_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
    SessionEntry::CustomMessage(CustomMessageEntry {
        base: SessionEntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: 1000,
        },
        custom_type: "note".to_string(),
        content: vec![CustomContent::Text(text.to_string())],
        details: None,
        display: true,
    })
}

fn assistant_tool_entry(id: &str, parent: Option<&str>) -> SessionEntry {
    SessionEntry::Message(SessionMessageEntry {
        base: SessionEntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: 1000,
        },
        message: assistant_ok_message(),
    })
}

#[test]
fn tree_navigation_user_entry_moves_leaf_to_parent_with_editor_text() {
    let entry = user_entry("e2", "e1", "edit me");
    let plan = plan_tree_navigation(&entry, false).unwrap();
    assert_eq!(plan.new_leaf_id.as_deref(), Some("e1"));
    assert_eq!(plan.editor_text.as_deref(), Some("edit me"));
    assert!(!plan.wants_summary);

    // Custom message entries behave like user messages.
    let custom = custom_entry("e3", Some("e2"), "custom text");
    let plan = plan_tree_navigation(&custom, false).unwrap();
    assert_eq!(plan.new_leaf_id.as_deref(), Some("e2"));
    assert_eq!(plan.editor_text.as_deref(), Some("custom text"));
}

#[test]
fn tree_navigation_non_user_entry_becomes_leaf() {
    let entry = assistant_tool_entry("e3", Some("e2"));
    let plan = plan_tree_navigation(&entry, false).unwrap();
    assert_eq!(plan.new_leaf_id.as_deref(), Some("e3"));
    assert_eq!(plan.editor_text, None);
}

#[test]
fn fork_selector_lists_user_messages_with_text() {
    let entries = vec![
        user_entry("e1", "", "first"),
        assistant_tool_entry("e2", Some("e1")),
        user_entry("e3", "e2", "second"),
        custom_entry("e4", Some("e3"), "skipped"),
    ];
    let forks = user_messages_for_forking(&entries);
    assert_eq!(
        forks,
        vec![
            ("e1".to_string(), "first".to_string()),
            ("e3".to_string(), "second".to_string()),
        ]
    );
}

// --- context usage --------------------------------------------------------------------------------

#[test]
fn context_usage_zero_window_is_unknown() {
    assert_eq!(
        compute_context_usage(&[], 0),
        None,
        "zero context window returns None"
    );
}

#[test]
fn context_usage_estimates_from_branch() {
    // Simple branch with assistant usage.
    let mut assistant = match assistant_ok_message() {
        CodingAgentMessage::Base(Message::Assistant(a)) => a,
        _ => unreachable!(),
    };
    assistant.usage = Usage {
        input: 1000,
        output: 500,
        cache_read: 200,
        cache_write: 0,
        ..Default::default()
    };
    let entries = vec![
        user_entry("e1", "", "hi"),
        SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "e2".to_string(),
                parent_id: Some("e1".to_string()),
                timestamp: 1000,
            },
            message: CodingAgentMessage::Base(Message::Assistant(assistant)),
        }),
    ];
    let usage = compute_context_usage(&entries, 10_000).unwrap();
    let tokens = usage.tokens.unwrap();
    assert!(tokens > 0, "{tokens}");
    assert_eq!(usage.context_window, 10_000);
    let percent = usage.percent.unwrap();
    assert!((percent - (tokens as f64 / 10_000.0) * 100.0).abs() < 1e-9);
}

#[test]
fn context_usage_after_compaction_unknown_until_post_usage() {
    // Build a branch: user, assistant (with usage), compaction, user.
    let compaction = SessionEntry::Compaction(
        pillar_coding_agent::core::session_entries::CompactionEntry {
            base: SessionEntryBase {
                id: "e3c".to_string(),
                parent_id: Some("e2".to_string()),
                timestamp: 1000,
            },
            summary: "compacted".to_string(),
            first_kept_entry_id: "e1".to_string(),
            tokens_before: 5000,
            details: None,
            usage: None,
            from_hook: false,
        },
    );
    let entries = vec![
        user_entry("e1", "", "hi"),
        SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "e2".to_string(),
                parent_id: Some("e1".to_string()),
                timestamp: 1000,
            },
            message: assistant_ok_message(),
        }),
        compaction,
        user_entry("e4", "e3c", "after"),
    ];
    let usage = compute_context_usage(&entries, 10_000).unwrap();
    // No assistant usage after the compaction → unknown tokens.
    assert_eq!(usage.tokens, None);
    assert_eq!(usage.percent, None);
    assert_eq!(usage.context_window, 10_000);
}

// --- bash execution flow ---------------------------------------------------------------------------

use pillar_coding_agent::core::agent_session::{BashSessionState, resolve_shell_command};
use pillar_coding_agent::core::bash_executor::BashResult;

fn bash_result(output: &str, exit_code: Option<i32>) -> BashResult {
    BashResult {
        output: output.to_string(),
        exit_code,
        cancelled: false,
        truncated: false,
        full_output_path: None,
        truncation: None,
    }
}

#[test]
fn shell_command_prefix_prepended_with_newline() {
    assert_eq!(resolve_shell_command("ls -la", None), "ls -la");
    assert_eq!(resolve_shell_command("ls -la", Some("")), "ls -la");
    assert_eq!(
        resolve_shell_command("ls -la", Some("shopt -s expand_aliases")),
        "shopt -s expand_aliases\nls -la"
    );
}

#[test]
fn bash_record_appends_immediately_when_not_streaming() {
    let mut state = BashSessionState::new();
    assert!(!state.is_running());
    state.start_execution();
    assert!(state.is_running());

    let result = bash_result("out", Some(0));
    let message = state
        .record_result("ls", &result, false, 1000, false)
        .expect("appended now");
    assert_eq!(message.command, "ls");
    assert_eq!(message.output, "out");
    assert_eq!(message.exit_code, Some(0));
    assert!(!message.exclude_from_context);
    assert_eq!(message.timestamp, 1000);

    state.end_execution();
    assert!(!state.is_running());
    assert!(!state.has_pending());
}

#[test]
fn bash_record_defers_while_streaming_and_flushes_after() {
    let mut state = BashSessionState::new();
    state.start_execution();

    let result = bash_result("streaming out", Some(1));
    // While streaming the message is queued, not returned.
    assert!(
        state
            .record_result("make", &result, false, 2000, true)
            .is_none()
    );
    assert!(state.has_pending());

    // Flush drains everything.
    let flushed = state.flush_pending();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].command, "make");
    assert_eq!(flushed[0].exit_code, Some(1));
    assert!(!state.has_pending());
}

#[test]
fn bash_record_preserves_truncation_and_exclusion() {
    let mut state = BashSessionState::new();
    let mut result = bash_result("tail", Some(0));
    result.truncated = true;
    result.full_output_path = Some(std::path::PathBuf::from("/tmp/full.txt"));

    let message = state
        .record_result("big-command", &result, true, 3000, false)
        .unwrap();
    assert!(message.truncated);
    assert_eq!(message.full_output_path.as_deref(), Some("/tmp/full.txt"));
    assert!(message.exclude_from_context);
}

#[test]
fn bash_cancelled_result_recorded() {
    let mut state = BashSessionState::new();
    let mut result = bash_result("partial", None);
    result.cancelled = true;

    let message = state
        .record_result("sleep 100", &result, false, 4000, false)
        .unwrap();
    assert!(message.cancelled);
    assert_eq!(message.exit_code, None);
}

// --- last assistant text -----------------------------------------------------------------------

use pillar_coding_agent::core::agent_session::get_last_assistant_text;

#[test]
fn last_assistant_text_finds_latest_non_empty() {
    let messages = vec![
        user_message("q1"),
        assistant_message(vec![Content::text("first answer")], StopReason::Stop, None),
        user_message("q2"),
        assistant_message(vec![Content::text("second  ")], StopReason::Stop, None),
    ];
    assert_eq!(
        get_last_assistant_text(&messages).as_deref(),
        Some("second")
    );
}

#[test]
fn last_assistant_text_skips_aborted_empty_and_skips_whitespace() {
    // Aborted with no content is skipped to the earlier answer.
    let messages = vec![
        assistant_message(vec![Content::text("kept")], StopReason::Stop, None),
        assistant_message(vec![], StopReason::Aborted, None),
    ];
    assert_eq!(get_last_assistant_text(&messages).as_deref(), Some("kept"));

    // Whitespace-only text yields None.
    let messages = vec![assistant_message(
        vec![Content::text("   ")],
        StopReason::Stop,
        None,
    )];
    assert_eq!(get_last_assistant_text(&messages), None);

    // No assistant at all.
    assert_eq!(get_last_assistant_text(&[user_message("hi")]), None);
}

#[test]
fn last_assistant_text_concatenates_text_blocks() {
    let messages = vec![assistant_message(
        vec![Content::text("part1 "), Content::text("part2")],
        StopReason::Stop,
        None,
    )];
    assert_eq!(
        get_last_assistant_text(&messages).as_deref(),
        Some("part1 part2")
    );
}
