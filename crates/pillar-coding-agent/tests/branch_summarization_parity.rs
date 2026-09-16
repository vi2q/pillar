//! Parity tests for branch-summarization.ts (pi v0.84.3): entry collection
//! over the session tree, budgeted entry preparation with cumulative file
//! tracking, and summary generation against a stubbed stream function.

use std::sync::Mutex;

use pillar_ai::types::{
    Content, Message, Model, ModelCost, ModelCostRates, StopReason, Usage, UserContent,
};
use pillar_coding_agent::core::compaction::branch_summarization::{
    BranchSummaryDetails, GenerateBranchSummaryOptions, collect_entries_for_branch_summary,
    generate_branch_summary, prepare_branch_entries,
};
use pillar_coding_agent::core::compaction::driver::SummarizationOptions;
use pillar_coding_agent::core::compaction::utils::SUMMARIZATION_SYSTEM_PROMPT;
use pillar_coding_agent::core::session_entries::{
    CompactionEntry, SessionEntry, SessionEntryBase, SessionMessageEntry, SessionTreeView,
    get_message_from_entry,
};

fn base(id: &str, parent: Option<&str>) -> SessionEntryBase {
    SessionEntryBase {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: 1000,
    }
}

fn user_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
    SessionEntry::Message(SessionMessageEntry {
        base: base(id, parent),
        message: pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::User {
            content: UserContent::Text(text.to_string()),
            timestamp: 1000,
        }),
    })
}

fn assistant_entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
    SessionEntry::Message(SessionMessageEntry {
        base: base(id, parent),
        message: pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::Assistant(
            Box::new(pillar_ai::types::AssistantMessage {
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
                timestamp: 1000,
            }),
        )),
    })
}

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

// --- entry tree + collection -----------------------------------------------------

fn tree() -> SessionTreeView {
    // a <- b <- c (branch 1)
    //   \\- d <- e (branch 2)
    let mut tree = SessionTreeView::new();
    tree.insert(user_entry("a", None, "root"));
    tree.insert(user_entry("b", Some("a"), "b"));
    tree.insert(assistant_entry("c", Some("b"), "c"));
    tree.insert(user_entry("d", Some("a"), "d"));
    tree.insert(assistant_entry("e", Some("d"), "e"));
    tree
}

#[test]
fn get_branch_returns_root_first_path() {
    let tree = tree();
    let branch = tree.get_branch("e");
    let ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec!["a", "d", "e"]);
}

#[test]
fn collect_entries_walks_to_common_ancestor() {
    let tree = tree();
    let result = collect_entries_for_branch_summary(&tree, Some("c"), "e");
    // Old branch a->b->c vs target a->d->e: common ancestor a; entries b, c.
    let ids: Vec<&str> = result.entries.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec!["b", "c"]);
    assert_eq!(result.common_ancestor_id.as_deref(), Some("a"));
}

#[test]
fn collect_entries_no_old_position_is_empty() {
    let tree = tree();
    let result = collect_entries_for_branch_summary(&tree, None, "e");
    assert!(result.entries.is_empty());
    assert_eq!(result.common_ancestor_id, None);
}

#[test]
fn collect_entries_includes_compaction_boundaries() {
    let mut tree = tree();
    tree.insert(SessionEntry::Compaction(CompactionEntry {
        base: base("comp", Some("c")),
        summary: "checkpoint".to_string(),
        first_kept_entry_id: "a".to_string(),
        tokens_before: 1000,
        details: None,
        usage: None,
        from_hook: false,
    }));
    let result = collect_entries_for_branch_summary(&tree, Some("comp"), "e");
    // Compaction entries are not stop points: b, c, comp are collected.
    let ids: Vec<&str> = result.entries.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec!["b", "c", "comp"]);
}

// --- get_message_from_entry -------------------------------------------------------------

#[test]
fn get_message_skips_tool_results_and_metadata() {
    let label = SessionEntry::Label(pillar_coding_agent::core::session_entries::LabelEntry {
        base: base("l", Some("a")),
        target_id: "a".to_string(),
        label: Some("mark".to_string()),
    });
    assert!(get_message_from_entry(&label).is_none());
    let model_change = SessionEntry::ModelChange(
        pillar_coding_agent::core::session_entries::ModelChangeEntry {
            base: base("mc", Some("a")),
            provider: "p".to_string(),
            model_id: "m".to_string(),
        },
    );
    assert!(get_message_from_entry(&model_change).is_none());
    let tool_result = SessionEntry::Message(SessionMessageEntry {
        base: base("tr", Some("a")),
        message: pillar_coding_agent::core::messages::CodingAgentMessage::Base(
            Message::ToolResult(Box::new(pillar_ai::types::ToolResultMessage {
                tool_call_id: "c".to_string(),
                tool_name: "read".to_string(),
                content: vec![Content::text("x")],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 1000,
            })),
        ),
    });
    assert!(get_message_from_entry(&tool_result).is_none());
}

// --- prepare_branch_entries ---------------------------------------------------------------

#[test]
fn prepare_walks_newest_to_oldest_within_budget() {
    let entries = vec![
        user_entry("a", None, "0123456789"),      // ~3 tokens
        user_entry("b", Some("a"), "0123456789"), // ~3 tokens
        user_entry("c", Some("b"), "0123456789"), // ~3 tokens
    ];
    // Budget 7 tokens: newest two fit (6), oldest would exceed -> dropped.
    let preparation = prepare_branch_entries(&entries, 7);
    let ids: Vec<String> = preparation
        .messages
        .iter()
        .filter_map(|m| match m {
            pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::User {
                content: UserContent::Text(text),
                ..
            }) => Some(text.clone()),
            _ => None,
        })
        .collect();
    // Newest messages are kept: b and c (each "0123456789").
    assert_eq!(ids.len(), 2);
    assert_eq!(preparation.total_tokens, 6);
}

#[test]
fn prepare_collects_file_ops_from_tool_calls_and_nested_summaries() {
    let mut entries = vec![assistant_entry("a", None, "work")];
    // Nested branch summary with cumulative details.
    entries.push(SessionEntry::BranchSummary(
        pillar_coding_agent::core::session_entries::BranchSummaryEntry {
            base: base("bs", Some("a")),
            from_id: "x".to_string(),
            summary: "nested".to_string(),
            details: Some(
                serde_json::to_value(BranchSummaryDetails {
                    read_files: vec!["/nested-read".to_string()],
                    modified_files: vec!["/nested-edit".to_string()],
                })
                .unwrap(),
            ),
            usage: None,
            from_hook: false,
        },
    ));
    // Extension-generated summary details are ignored.
    entries.push(SessionEntry::BranchSummary(
        pillar_coding_agent::core::session_entries::BranchSummaryEntry {
            base: base("bs2", Some("bs")),
            from_id: "y".to_string(),
            summary: "hook".to_string(),
            details: Some(
                serde_json::to_value(BranchSummaryDetails {
                    read_files: vec!["/hook-read".to_string()],
                    modified_files: vec![],
                })
                .unwrap(),
            ),
            usage: None,
            from_hook: true,
        },
    ));
    let preparation = prepare_branch_entries(&entries, 0);
    assert!(preparation.file_ops.read.contains("/nested-read"));
    assert!(preparation.file_ops.edited.contains("/nested-edit"));
    assert!(!preparation.file_ops.read.contains("/hook-read"));
}

// --- generate_branch_summary -----------------------------------------------------------------

type StubFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<pillar_ai::types::AssistantMessage, String>> + Send,
    >,
>;

fn stub_stream(
    text: &'static str,
    stop_reason: StopReason,
    prompts: &'static Mutex<Vec<String>>,
) -> impl Fn(&Model, &pillar_ai::types::Context, &SummarizationOptions) -> StubFuture {
    move |_model, context, _options| {
        let user_prompt = match &context.messages[0] {
            Message::User {
                content: UserContent::Blocks(blocks),
                ..
            } => match blocks.first() {
                Some(Content::Text { text, .. }) => text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        prompts
            .lock()
            .unwrap()
            .push(context.system_prompt.clone().unwrap_or_default() + "\x1f" + &user_prompt);
        Box::pin(async move {
            Ok(pillar_ai::types::AssistantMessage {
                content: vec![Content::text(text)],
                api: "test-api".to_string(),
                provider: "p".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: Vec::new(),
                usage: Usage {
                    total_tokens: 7,
                    ..Default::default()
                },
                stop_reason,
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
async fn generate_branch_summary_prepends_preamble_and_appends_file_ops() {
    let prompts: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let mut tree = SessionTreeView::new();
    tree.insert(user_entry("a", None, "root question"));
    tree.insert(SessionEntry::Message(SessionMessageEntry {
        base: base("b", Some("a")),
        message: pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::Assistant(
            Box::new(pillar_ai::types::AssistantMessage {
                content: vec![Content::ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "/x"}),
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
                timestamp: 1000,
            }),
        )),
    }));
    let result = collect_entries_for_branch_summary(&tree, Some("b"), "elsewhere");
    let options = GenerateBranchSummaryOptions {
        model: &model(),
        api_key: None,
        headers: None,
        env: None,
        signal: None,
        custom_instructions: None,
        replace_instructions: false,
        reserve_tokens: 16_384,
        stream_fn: &stub_stream("branch work done", StopReason::Stop, prompts),
    };
    let summary = generate_branch_summary(&result.entries, options).await;
    assert!(summary.error.is_none());
    assert!(!summary.aborted);
    let text = summary.summary.as_deref().unwrap();
    assert!(
        text.starts_with(
            "The user explored a different conversation branch before returning here."
        )
    );
    assert!(text.contains("branch work done"));
    // File ops appended.
    assert!(text.contains("<read-files>\n/x\n</read-files>"), "{text}");
    assert_eq!(summary.read_files.as_deref(), Some(&["/x".to_string()][..]));
    assert_eq!(summary.usage.as_ref().unwrap().total_tokens, 7);

    // Prompt construction: system prompt + conversation tag + branch prompt.
    let recorded = prompts.lock().unwrap();
    let (system, user_prompt) = recorded[0].split_once('\x1f').unwrap();
    assert_eq!(system, SUMMARIZATION_SYSTEM_PROMPT);
    assert!(user_prompt.contains("<conversation>"));
    assert!(user_prompt.contains("## Goal"));
}

#[tokio::test]
async fn generate_branch_summary_empty_returns_no_content() {
    let prompts: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let options = GenerateBranchSummaryOptions {
        model: &model(),
        api_key: None,
        headers: None,
        env: None,
        signal: None,
        custom_instructions: None,
        replace_instructions: false,
        reserve_tokens: 16_384,
        stream_fn: &stub_stream("unused", StopReason::Stop, prompts),
    };
    let summary = generate_branch_summary(&[], options).await;
    assert_eq!(summary.summary.as_deref(), Some("No content to summarize"));
}

#[tokio::test]
async fn generate_branch_summary_aborted_and_error_stops() {
    let prompts: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let entries = vec![user_entry("a", None, "content")];
    let aborted = generate_branch_summary(
        &entries,
        GenerateBranchSummaryOptions {
            model: &model(),
            api_key: None,
            headers: None,
            env: None,
            signal: None,
            custom_instructions: None,
            replace_instructions: false,
            reserve_tokens: 16_384,
            stream_fn: &stub_stream("partial", StopReason::Aborted, prompts),
        },
    )
    .await;
    assert!(aborted.aborted);

    let errored = generate_branch_summary(
        &entries,
        GenerateBranchSummaryOptions {
            model: &model(),
            api_key: None,
            headers: None,
            env: None,
            signal: None,
            custom_instructions: None,
            replace_instructions: false,
            reserve_tokens: 16_384,
            stream_fn: &stub_stream("partial", StopReason::Length, prompts),
        },
    )
    .await;
    assert_eq!(
        errored.error.as_deref(),
        Some(
            "Branch summarization failed: generation hit the token cap and the summary is incomplete"
        )
    );
}

#[tokio::test]
async fn generate_branch_summary_custom_instructions_modes() {
    let prompts: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let entries = vec![user_entry("a", None, "content")];
    // Appended mode.
    let appended = generate_branch_summary(
        &entries,
        GenerateBranchSummaryOptions {
            model: &model(),
            api_key: None,
            headers: None,
            env: None,
            signal: None,
            custom_instructions: Some("focus on tests"),
            replace_instructions: false,
            reserve_tokens: 16_384,
            stream_fn: &stub_stream("ok", StopReason::Stop, prompts),
        },
    )
    .await;
    assert!(appended.summary.is_some());
    {
        let recorded = prompts.lock().unwrap();
        let (_, user_prompt) = recorded.last().unwrap().split_once('\x1f').unwrap();
        assert!(user_prompt.contains("Additional focus: focus on tests"));
        assert!(user_prompt.contains("## Goal"));
    }
    // Replace mode.
    let replaced = generate_branch_summary(
        &entries,
        GenerateBranchSummaryOptions {
            model: &model(),
            api_key: None,
            headers: None,
            env: None,
            signal: None,
            custom_instructions: Some("just list files"),
            replace_instructions: true,
            reserve_tokens: 16_384,
            stream_fn: &stub_stream("ok", StopReason::Stop, prompts),
        },
    )
    .await;
    assert!(replaced.summary.is_some());
    let recorded = prompts.lock().unwrap();
    let (_, user_prompt) = recorded.last().unwrap().split_once('\x1f').unwrap();
    assert!(user_prompt.contains("just list files"));
    assert!(!user_prompt.contains("## Goal"));
}
