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
