//! Parity tests for compaction utils.ts + compaction.ts (pi v0.84.3):
//! file-op tracking, conversation serialization, token estimation, cut
//! point detection, summarization prompts/failures, and the compact driver
//! against a stubbed stream function.

use std::collections::BTreeSet;
use std::sync::Mutex;

use pillar_ai::types::{
    Content, Message, Model, ModelCost, ModelCostRates, StopReason, Usage, UserContent,
};
use pillar_coding_agent::core::compaction::driver::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS, SUMMARIZATION_PROMPT,
    TURN_PREFIX_SUMMARIZATION_PROMPT, UPDATE_SUMMARIZATION_PROMPT, calculate_context_tokens,
    estimate_context_tokens, estimate_tokens, find_cut_point, find_turn_start_index,
    generate_summary_with_usage, get_summarization_failure, prepare_compaction, should_compact,
};
use pillar_coding_agent::core::compaction::utils::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use pillar_coding_agent::core::messages::BashExecutionMessage;
use pillar_coding_agent::core::messages::{
    CodingAgentMessage, CompactionSummaryMessage, bash_execution_to_text,
};

fn model() -> Model {
    Model {
        id: "m".to_string(),
        name: "m".to_string(),
        api: "test-api".to_string(),
        provider: "p".to_string(),
        base_url: "https://x.test".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates::default(),
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn user(text: &str, ts: u64) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: ts,
    })
}

fn assistant_text(text: &str, ts: u64) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![Content::text(text)],
            api: "test-api".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: ts,
        },
    )))
}

fn assistant_tool_call(name: &str, path: &str, ts: u64) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![Content::ToolCall {
                id: "call-1".to_string(),
                name: name.to_string(),
                arguments: serde_json::json!({"path": path}),
                thought_signature: None,
                namespace: None,
            }],
            api: "test-api".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: ts,
        },
    )))
}

fn tool_result(text: &str, ts: u64) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::ToolResult(Box::new(
        pillar_ai::types::ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![Content::text(text)],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: ts,
        },
    )))
}

// --- file operations -----------------------------------------------------------

#[test]
fn extract_file_ops_from_tool_calls() {
    let mut ops = FileOperations::new();
    extract_file_ops_from_message(&assistant_tool_call("read", "/a", 1), &mut ops);
    extract_file_ops_from_message(&assistant_tool_call("write", "/b", 2), &mut ops);
    extract_file_ops_from_message(&assistant_tool_call("edit", "/c", 3), &mut ops);
    extract_file_ops_from_message(&assistant_tool_call("other", "/d", 4), &mut ops);
    assert_eq!(ops.read, BTreeSet::from(["/a".to_string()]));
    assert_eq!(ops.written, BTreeSet::from(["/b".to_string()]));
    assert_eq!(ops.edited, BTreeSet::from(["/c".to_string()]));

    // Untracked tool names contribute nothing.
    assert!(ops.read.contains("/a") && ops.read.len() == 1);
    let (read, modified) = compute_file_lists(&ops);
    assert_eq!(read, vec!["/a".to_string()]);
    assert_eq!(modified, vec!["/b".to_string(), "/c".to_string()]);
}

#[test]
fn read_only_excludes_modified_files() {
    let mut ops = FileOperations::new();
    extract_file_ops_from_message(&assistant_tool_call("read", "/a", 1), &mut ops);
    extract_file_ops_from_message(&assistant_tool_call("edit", "/a", 2), &mut ops);
    let (read, modified) = compute_file_lists(&ops);
    assert!(read.is_empty());
    assert_eq!(modified, vec!["/a".to_string()]);
}

#[test]
fn format_file_operations_xml() {
    assert_eq!(format_file_operations(&[], &[]), "");
    assert_eq!(
        format_file_operations(&["/a".to_string()], &[]),
        "\n\n<read-files>\n/a\n</read-files>"
    );
    assert_eq!(
        format_file_operations(&["/a".to_string()], &["/b".to_string()]),
        "\n\n<read-files>\n/a\n</read-files>\n\n<modified-files>\n/b\n</modified-files>"
    );
}

// --- serializeConversation ----------------------------------------------------------

#[test]
fn serialize_conversation_sections() {
    let messages = vec![
        user("hello", 1),
        assistant_text("response", 2),
        tool_result("tool output", 3),
    ];
    let text = serialize_conversation(&pillar_coding_agent::core::messages::convert_to_llm(
        &messages,
    ));
    assert_eq!(
        text,
        "[User]: hello\n\n[Assistant]: response\n\n[Tool result]: tool output"
    );
}

#[test]
fn serialize_conversation_thinking_and_tool_calls() {
    let messages = vec![CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![
                Content::Thinking {
                    thinking: "deep thought".to_string(),
                    thinking_signature: None,
                    redacted: None,
                },
                Content::ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "/a", "limit": 5}),
                    thought_signature: None,
                    namespace: None,
                },
            ],
            api: "test-api".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    )))];
    let text = serialize_conversation(&pillar_coding_agent::core::messages::convert_to_llm(
        &messages,
    ));
    assert_eq!(
        text,
        "[Assistant thinking]: deep thought\n\n[Assistant tool calls]: read(limit=5, path=\"/a\")"
    );
}

#[test]
fn serialize_conversation_truncates_long_tool_results() {
    let long = "x".repeat(3000);
    let messages = vec![tool_result(&long, 1)];
    let text = serialize_conversation(&pillar_coding_agent::core::messages::convert_to_llm(
        &messages,
    ));
    assert!(
        text.contains("[... 1000 more characters truncated]"),
        "{text}"
    );
    assert!(text.len() < long.len());
}

// --- token estimation ------------------------------------------------------------------

#[test]
fn calculate_context_tokens_prefers_total_tokens() {
    let usage = Usage {
        input: 10,
        output: 5,
        cache_read: 3,
        cache_write: 2,
        total_tokens: 100,
        ..Default::default()
    };
    assert_eq!(calculate_context_tokens(&usage), 100);
    let usage = Usage {
        input: 10,
        output: 5,
        cache_read: 3,
        cache_write: 2,
        ..Default::default()
    };
    assert_eq!(calculate_context_tokens(&usage), 20);
}

#[test]
fn estimate_tokens_chars_over_four() {
    // user text: 8 chars -> 2 tokens
    assert_eq!(estimate_tokens(&user("12345678", 1)), 2);
    // image: 4800 chars -> 1200 tokens
    let image_msg = CodingAgentMessage::Base(Message::User {
        content: UserContent::Blocks(vec![Content::Image {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        }]),
        timestamp: 1,
    });
    assert_eq!(estimate_tokens(&image_msg), 1200);
    // bash execution: command + output chars
    let bash = CodingAgentMessage::BashExecution(BashExecutionMessage {
        command: "abcd".to_string(),
        output: "ef".to_string(),
        ..Default::default()
    });
    assert_eq!(estimate_tokens(&bash), 2); // ceil(6/4)
    // compaction summary: summary chars
    let summary = CodingAgentMessage::CompactionSummary(CompactionSummaryMessage {
        summary: "abcdefgh".to_string(),
        ..Default::default()
    });
    assert_eq!(estimate_tokens(&summary), 2);
}

#[test]
fn estimate_context_tokens_uses_last_usage_and_estimates_trailing() {
    let usage = Usage {
        total_tokens: 1000,
        ..Default::default()
    };
    let with_usage = pillar_ai::types::AssistantMessage {
        content: vec![Content::text("with usage")],
        api: "test-api".to_string(),
        provider: "p".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage,
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    let mut zero_usage = with_usage.clone();
    zero_usage.usage = Usage::default();

    let messages = vec![
        user("a", 1),
        CodingAgentMessage::Base(Message::Assistant(Box::new(with_usage.clone()))),
        user("trailing message", 2),
        CodingAgentMessage::Base(Message::Assistant(Box::new(zero_usage))),
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1000);
    // trailing: "trailing message" (16 chars -> 4) + zero-usage assistant (10 chars -> 3)
    assert_eq!(estimate.trailing_tokens, 7);
    assert_eq!(estimate.tokens, 1007);
    assert_eq!(estimate.last_usage_index, Some(1));
}

#[test]
fn estimate_context_tokens_without_usage_estimates_everything() {
    let messages = vec![user("12345678", 1), user("1234", 2)];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.tokens, 3);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.trailing_tokens, 3);
    assert_eq!(estimate.last_usage_index, None);
}

#[test]
fn should_compact_respects_reserve_and_enabled() {
    let settings = DEFAULT_COMPACTION_SETTINGS;
    assert!(should_compact(90_000, 100_000, &settings));
    assert!(
        !should_compact(80_000, 100_000, &settings),
        "below reserve boundary"
    );
    assert!(!should_compact(
        90_000,
        100_000,
        &CompactionSettings {
            enabled: false,
            ..settings
        }
    ));
}

// --- cut points -----------------------------------------------------------------------------

#[test]
fn find_cut_point_never_cuts_at_tool_results() {
    // user(1) assistant(2) tool(3) user(4) assistant(5) tool(6)
    let messages = vec![
        user("turn one", 1),
        assistant_text("work", 2),
        tool_result("result", 3),
        user("turn two", 4),
        assistant_text("more work", 5),
        tool_result("more result", 6),
    ];
    // Budget large enough to keep everything: cut at the first message.
    let result = find_cut_point(&messages, 0, messages.len(), 100_000);
    assert_eq!(result.first_kept_entry_index, 0);
    assert!(!result.is_split_turn);

    // Small budget: walks back from the end accumulating tokens; must stop at
    // a user or assistant message, never the tool result.
    let result = find_cut_point(&messages, 0, messages.len(), 1);
    let cut = result.first_kept_entry_index;
    assert!(!matches!(
        &messages[cut],
        CodingAgentMessage::Base(Message::ToolResult(_))
    ));
}

#[test]
fn find_cut_point_split_turn_detection() {
    // Turn: user, assistant, toolResult, assistant.
    let messages = vec![
        user("start", 1),
        assistant_text("part1", 2),
        tool_result("r", 3),
        assistant_text("part2", 4),
    ];
    let result = find_cut_point(&messages, 0, messages.len(), 1);
    assert!(result.is_split_turn, "cut inside a turn after tool results");
    assert_eq!(result.turn_start_index, Some(0));

    // Cut at the user message itself is not a split.
    let messages = vec![user("turn one long text here", 1), user("turn two", 2)];
    let result = find_cut_point(&messages, 0, messages.len(), 1);
    assert!(!result.is_split_turn);
}

#[test]
fn find_turn_start_index_scans_backwards() {
    let messages = vec![
        user("t1", 1),
        assistant_text("a", 2),
        tool_result("r", 3),
        assistant_text("b", 4),
    ];
    assert_eq!(find_turn_start_index(&messages, 3, 0), Some(0));
    assert_eq!(find_turn_start_index(&messages, 0, 0), Some(0));
    // No user message before index (start_index beyond).
    assert_eq!(find_turn_start_index(&messages, 3, 4), None);
}

#[test]
fn empty_range_cut_point_defaults_to_start() {
    let messages = vec![tool_result("only tool result", 1)];
    let result = find_cut_point(&messages, 0, messages.len(), 1000);
    // No valid cut points -> keep from start.
    assert_eq!(result.first_kept_entry_index, 0);
    assert!(!result.is_split_turn);
}

// --- summarization prompts and failures ---------------------------------------------------------

#[test]
fn summarization_prompts_match_upstream() {
    assert!(SUMMARIZATION_SYSTEM_PROMPT.starts_with("You are a context summarization assistant."));
    assert!(
        SUMMARIZATION_PROMPT.starts_with("The messages above are a conversation to summarize.")
    );
    assert!(SUMMARIZATION_PROMPT.contains("## Goal"));
    assert!(SUMMARIZATION_PROMPT.contains("## Critical Context"));
    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("<previous-summary> tags"));
    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("PRESERVE all existing information"));
    assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("PREFIX of a turn"));
    assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Original Request"));
}

#[test]
fn summarization_failure_detection() {
    let mut response = pillar_ai::types::AssistantMessage {
        content: vec![Content::text("partial")],
        api: "test-api".to_string(),
        provider: "p".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    };
    assert!(get_summarization_failure(&response, "Summarization").is_none());

    response.stop_reason = StopReason::Length;
    assert_eq!(
        get_summarization_failure(&response, "Summarization").as_deref(),
        Some("Summarization failed: generation hit the token cap and the summary is incomplete")
    );

    response.stop_reason = StopReason::Error;
    response.error_message = Some("boom".to_string());
    assert_eq!(
        get_summarization_failure(&response, "Summarization").as_deref(),
        Some("Summarization failed: boom")
    );
    response.error_message = None;
    assert_eq!(
        get_summarization_failure(&response, "Summarization").as_deref(),
        Some("Summarization failed: Unknown error")
    );
}

// --- generate summary with a stubbed stream fn ----------------------------------------------------

type StubFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>> + Send,
    >,
>;

fn stub_stream(
    response_text: &'static str,
    calls: &'static Mutex<Vec<String>>,
) -> impl Fn(
    &Model,
    &pillar_ai::types::Context,
    &pillar_coding_agent::core::compaction::driver::SummarizationOptions,
) -> StubFuture {
    move |_model: &Model,
          context: &pillar_ai::types::Context,
          options: &pillar_coding_agent::core::compaction::driver::SummarizationOptions| {
        let calls = calls;
        let system_prompt = context.system_prompt.clone().unwrap_or_default();
        let max_tokens = options.max_tokens;
        Box::pin(async move {
            calls.lock().unwrap().push(system_prompt);
            // Verify the maxTokens budget is derived from reserveTokens.
            assert_eq!(max_tokens, Some(6_400));
            let _ = bash_execution_to_text(&BashExecutionMessage::default());
            Ok(pillar_ai::types::AssistantMessage {
                content: vec![Content::text(response_text)],
                api: "test-api".to_string(),
                provider: "p".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Usage {
                    total_tokens: 42,
                    ..Default::default()
                },
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1,
            })
        })
    }
}

#[tokio::test]
async fn generate_summary_produces_text_and_wraps_conversation() {
    let calls: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let messages = vec![user("hello world", 1)];
    let (text, usage) = generate_summary_with_usage(
        &messages,
        &model(),
        8_000, // reserveTokens -> maxTokens = 6400
        Default::default(),
        None,
        None,
        &stub_stream("## Goal\nsummary body", calls),
    )
    .await
    .unwrap();
    assert!(text.contains("## Goal"));
    assert_eq!(usage.total_tokens, 42);
    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].as_str(), SUMMARIZATION_SYSTEM_PROMPT);
}

#[tokio::test]
async fn generate_summary_with_previous_summary_uses_update_prompt() {
    let calls: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let captured_prompts: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let stream = |model: &Model,
                  context: &pillar_ai::types::Context,
                  options: &pillar_coding_agent::core::compaction::driver::SummarizationOptions|
     -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>>
                + Send,
        >,
    > {
        let _ = model;
        let _ = options;
        let user_text = match &context.messages[0] {
            Message::User { content, .. } => match content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => match &blocks[0] {
                    Content::Text { text, .. } => text.clone(),
                    _ => String::new(),
                },
            },
            _ => String::new(),
        };
        captured_prompts.lock().unwrap().push(user_text);
        Box::pin(async move {
            Ok(pillar_ai::types::AssistantMessage {
                content: vec![Content::text("updated summary")],
                api: "test-api".to_string(),
                provider: "p".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1,
            })
        })
    };
    let messages = vec![user("new messages", 1)];
    let (text, _) = generate_summary_with_usage(
        &messages,
        &model(),
        8_000,
        Default::default(),
        None,
        Some("previous summary body"),
        &stream,
    )
    .await
    .unwrap();
    assert_eq!(text, "updated summary");
    let prompts = captured_prompts.lock().unwrap();
    assert!(prompts[0].contains("<previous-summary>"), "{}", prompts[0]);
    assert!(prompts[0].contains("previous summary body"));
    assert!(prompts[0].contains(UPDATE_SUMMARIZATION_PROMPT));
    let _ = calls;
}

#[tokio::test]
async fn generate_summary_rejects_tool_calls_in_response() {
    let stream =
        |_model: &Model,
         _context: &pillar_ai::types::Context,
         _options: &pillar_coding_agent::core::compaction::driver::SummarizationOptions|
         -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>>
                    + Send,
            >,
        > {
            Box::pin(async move {
                Ok(pillar_ai::types::AssistantMessage {
                    content: vec![Content::ToolCall {
                        id: "c".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                        namespace: None,
                    }],
                    api: "test-api".to_string(),
                    provider: "p".to_string(),
                    model: "m".to_string(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 1,
                })
            })
        };
    let error = generate_summary_with_usage(
        &[user("hi", 1)],
        &model(),
        8_000,
        Default::default(),
        None,
        None,
        &stream,
    )
    .await
    .unwrap_err();
    assert_eq!(error, "Summarization attempted to call a tool");
}

#[tokio::test]
async fn generate_summary_error_stop_reason_fails() {
    let stream =
        |_model: &Model,
         _context: &pillar_ai::types::Context,
         _options: &pillar_coding_agent::core::compaction::driver::SummarizationOptions|
         -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>>
                    + Send,
            >,
        > {
            Box::pin(async move {
                Ok(pillar_ai::types::AssistantMessage {
                    content: vec![],
                    api: "test-api".to_string(),
                    provider: "p".to_string(),
                    model: "m".to_string(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Error,
                    deferred: None,
                    error_message: Some("stream died".to_string()),
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 1,
                })
            })
        };
    let error = generate_summary_with_usage(
        &[user("hi", 1)],
        &model(),
        8_000,
        Default::default(),
        None,
        None,
        &stream,
    )
    .await
    .unwrap_err();
    assert_eq!(error, "Summarization failed: stream died");
}

// --- prepare + compact -----------------------------------------------------------------------------

#[tokio::test]
async fn prepare_and_compact_end_to_end() {
    let messages = vec![
        user("old work", 1),
        assistant_text("did things", 2),
        tool_result("read /tmp/x", 3),
        user("recent work", 4),
        assistant_text("recent changes", 5),
    ];
    // Small keepRecentTokens so the cut lands mid-history and there is
    // something to summarize (a full budget keeps everything -> nothing to
    // do, matching upstream's undefined).
    let settings = CompactionSettings {
        keep_recent_tokens: 1,
        ..DEFAULT_COMPACTION_SETTINGS
    };
    let preparation =
        prepare_compaction(&messages, settings, |index| Some(format!("entry-{index}")))
            .expect("preparation");
    assert!(!preparation.messages_to_summarize.is_empty());

    // The id resolver receives the cut index; verify directly.
    let cut_index: usize = preparation
        .first_kept_entry_id
        .trim_start_matches("entry-")
        .parse()
        .unwrap();
    assert!(!messages[cut_index..].is_empty());

    // File ops include the tool result's implicit read? No — only tool calls
    // carry file ops; the fixture's tool result contributes nothing.
    assert!(preparation.file_ops.read.is_empty());

    // Compact with a stub stream.
    let stream =
        |_model: &Model,
         _context: &pillar_ai::types::Context,
         _options: &pillar_coding_agent::core::compaction::driver::SummarizationOptions|
         -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>>
                    + Send,
            >,
        > {
            Box::pin(async move {
                Ok(pillar_ai::types::AssistantMessage {
                    content: vec![Content::text("## Goal\ncompact summary")],
                    api: "test-api".to_string(),
                    provider: "p".to_string(),
                    model: "m".to_string(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 1,
                })
            })
        };
    let result = pillar_coding_agent::core::compaction::driver::compact(
        preparation,
        &model(),
        Default::default(),
        None,
        &stream,
    )
    .await
    .unwrap();
    assert!(result.summary.contains("## Goal"));
    assert!(result.first_kept_entry_id.starts_with("entry-"));
    assert!(result.details.is_some());
}

#[test]
fn prepare_compaction_empty_returns_none() {
    assert!(prepare_compaction(&[], DEFAULT_COMPACTION_SETTINGS, |_| None).is_none());
}
