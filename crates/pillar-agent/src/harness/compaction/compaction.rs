//! Port of packages/agent/src/harness/compaction/compaction.ts (pi
//! v0.84.3) — the compaction pipeline.
//!
//! Ported here: `CompactionSettings`, usage helpers,
//! `estimateContextTokens`, `shouldCompact`, cut-point search, and the
//! summary generation/compaction flows. `estimateTokens`,
//! `SUMMARIZATION_SYSTEM_PROMPT`, and `completeSimpleWithRetries` live in
//! [`super::shared`]; conversation/file-operation utilities in
//! [`super::utils`]; branch-summarization specifics in
//! [`super::branch_summarization`].

use pillar_ai::retry::{RetryCallbacks, RetryPolicy};
use pillar_ai::types::{AssistantMessage, Content, Message, StopReason, Usage, UserContent};

use super::branch_summarization::BranchSummaryError;
use super::shared::{complete_simple_with_retries, estimate_tokens};
use super::utils::{
    FileOperations, compute_file_lists, extract_file_ops_from_message, format_file_operations,
};
use crate::harness::messages::convert_to_llm;
use crate::harness::session::context::build_session_context;
use crate::harness::session::types::{AgentMessage, Entry, EntryPayload};
use pillar_ai::types::ThinkingLevel;

/// Stable compaction error codes (upstream `CompactionErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionErrorCode {
    Aborted,
    SummarizationFailed,
}

impl CompactionErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::SummarizationFailed => "summarization_failed",
        }
    }
}

/// Error returned by compaction helpers (upstream `CompactionError`).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CompactionError {
    pub code: CompactionErrorCode,
    pub message: String,
}

impl CompactionError {
    pub fn new(code: CompactionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Compaction thresholds and retention settings (upstream
/// `CompactionSettings`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    /// Enable automatic compaction decisions.
    pub enabled: bool,
    /// Tokens reserved for summary prompt and output.
    pub reserve_tokens: u64,
    /// Approximate recent-context tokens to keep after compaction.
    pub keep_recent_tokens: u64,
}

/// Default compaction settings (upstream `DEFAULT_COMPACTION_SETTINGS`).
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

/// Calculate total context tokens from provider usage (upstream
/// `calculateContextTokens`).
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn get_assistant_usage(message: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Message(Message::Assistant(assistant)) = message else {
        return None;
    };
    if assistant.stop_reason == StopReason::Aborted || assistant.stop_reason == StopReason::Error {
        return None;
    }
    if calculate_context_tokens(&assistant.usage) == 0 {
        return None;
    }
    Some(&assistant.usage)
}

/// Usage from the last valid assistant message in session entries (upstream
/// `getLastAssistantUsage`).
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<&Usage> {
    for entry in entries.iter().rev() {
        if let EntryPayload::Message { message, .. } = &entry.payload
            && let Some(usage) = get_assistant_usage(message)
        {
            return Some(usage);
        }
    }
    None
}

/// Estimated context-token usage for a message list (upstream
/// `ContextUsageEstimate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the message that provided usage, or `None`.
    pub last_usage_index: Option<usize>,
}

/// Estimate context tokens for messages using provider usage when available
/// (upstream `estimateContextTokens`).
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextEstimate {
    let mut usage_info: Option<(Usage, usize)> = None;
    for (index, message) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(message) {
            usage_info = Some((usage.clone(), index));
            break;
        }
    }

    let Some((usage, index)) = usage_info else {
        let estimated: u64 = messages.iter().map(estimate_tokens).sum();
        return ContextEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(&usage);
    let mut trailing_tokens = 0u64;
    for message in messages.iter().take(messages.len()).skip(index + 1) {
        trailing_tokens += estimate_tokens(message);
    }

    ContextEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Whether context usage exceeds the compaction threshold (upstream
/// `shouldCompact`).
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// Cut point selected for compaction (upstream `CutPointResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry retained after compaction.
    pub first_kept_entry_index: usize,
    /// Index of the turn-start entry when the cut splits a turn, else `None`.
    pub turn_start_index: Option<usize>,
    /// Whether the selected cut point splits an in-progress turn.
    pub is_split_turn: bool,
}

fn message_role_is_turn_start(message: &AgentMessage) -> bool {
    matches!(message.role_name(), "user" | "bashExecution")
}

fn find_valid_cut_points(entries: &[Entry], start_index: usize, end_index: usize) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (i, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        match &entry.payload {
            // Upstream cut points: message roles user/bashExecution/custom/
            // branchSummary/compactionSummary/assistant (toolResult
            // excluded), plus branch_summary entries.
            EntryPayload::Message { message, .. } => {
                if !matches!(message.role_name(), "toolResult") {
                    cut_points.push(i);
                }
            }
            EntryPayload::BranchSummary { .. } => cut_points.push(i),
            _ => {}
        }
    }
    cut_points
}

/// Find the user-visible message that starts the turn containing an entry
/// (upstream `findTurnStartIndex`).
pub fn find_turn_start_index(
    entries: &[Entry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    let mut i = entry_index;
    loop {
        let entry = entries.get(i)?;
        if entry.kind() == "branch_summary" {
            return Some(i);
        }
        if let EntryPayload::Message { message, .. } = &entry.payload
            && message_role_is_turn_start(message)
        {
            return Some(i);
        }
        if i == start_index {
            return None;
        }
        i -= 1;
    }
}

/// Find the compaction cut point that keeps approximately the requested
/// recent-token budget (upstream `findCutPoint`).
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }
    let mut accumulated_tokens = 0u64;
    let mut cut_index = cut_points[0];

    for i in (start_index..end_index).rev() {
        let entry = &entries[i];
        let EntryPayload::Message { message, .. } = &entry.payload else {
            continue;
        };
        accumulated_tokens += estimate_tokens(message);
        if accumulated_tokens >= keep_recent_tokens {
            for &candidate in &cut_points {
                if candidate >= i {
                    cut_index = candidate;
                    break;
                }
            }
            break;
        }
    }
    while cut_index > start_index {
        let prev_entry = &entries[cut_index - 1];
        match &prev_entry.payload {
            EntryPayload::Compaction { .. } => break,
            EntryPayload::Message { .. } => break,
            _ => {}
        }
        cut_index -= 1;
    }
    let cut_entry = &entries[cut_index];
    let is_user_message = matches!(
        &cut_entry.payload,
        EntryPayload::Message { message, .. } if message.role_name() == "user"
    );
    let turn_start_index = if is_user_message {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index.is_some(),
    }
}

/// File-operation details stored on generated compaction entries (upstream
/// `CompactionDetails`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Generated compaction data ready to be persisted (upstream
/// `CompactResult`).
#[derive(Debug, Clone, Default)]
pub struct CompactResult {
    /// Summary text that replaces compacted history in future context.
    pub summary: String,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Usage from the LLM call(s) that generated this summary.
    pub usage: Option<Usage>,
    /// Recent messages retained after compaction.
    pub retained_tail: Vec<AgentMessage>,
    /// File-operation details stored with the compaction entry.
    pub details: Option<CompactionDetails>,
}

/// Combine two usage blocks field-wise (upstream `combineUsage`).
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: match (first.cache_write_1h, second.cache_write_1h) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        total_tokens: first.total_tokens + second.total_tokens,
        cost: pillar_ai::types::UsageCost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

/// Message extraction used by compaction (upstream
/// `getMessageFromEntryForCompaction`): compaction entries become nothing,
/// everything else maps through the branch-summary message extraction.
pub fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    match &entry.payload {
        EntryPayload::Compaction { .. } => None,
        EntryPayload::Message { message, .. } => {
            // Upstream drops toolResult messages from summarization input
            // (they are represented via the assistant tool calls).
            if message.role_name() == "toolResult" {
                None
            } else {
                Some(message.clone())
            }
        }
        EntryPayload::BranchSummary {
            from_id, summary, ..
        } => Some(crate::harness::messages::create_branch_summary_message(
            summary.clone(),
            from_id.clone(),
            entry.timestamp,
        )),
        _ => None,
    }
}

/// Prepared inputs for a compaction run (upstream `CompactionPreparation`).
#[derive(Debug, Clone, Default)]
pub struct CompactionPreparation {
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub retained_tail: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: Option<CompactionSettings>,
}

/// Prepare session entries for compaction; `Ok(None)` when compaction is
/// not applicable (upstream `prepareCompaction`).
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty() || path_entries.last().unwrap().kind() == "compaction" {
        return Ok(None);
    }

    let mut prev_compaction_index: Option<usize> = None;
    for (i, entry) in path_entries.iter().enumerate().rev() {
        if entry.kind() == "compaction" {
            prev_compaction_index = Some(i);
            break;
        }
    }

    let mut previous_summary: Option<String> = None;
    let mut compactable_entries: Vec<Entry> = path_entries.to_vec();
    if let Some(prev_compaction_index) = prev_compaction_index {
        let prev_compaction = &path_entries[prev_compaction_index];
        if let EntryPayload::Compaction {
            summary,
            retained_tail,
            ..
        } = &prev_compaction.payload
        {
            previous_summary = Some(summary.clone());
            let mut virtual_retained_entries: Vec<Entry> = Vec::new();
            for (index, message) in retained_tail.iter().enumerate() {
                virtual_retained_entries.push(Entry {
                    id: format!("{}:retained:{}", prev_compaction.id, index),
                    seq: prev_compaction.seq,
                    parent_id: Some(if index == 0 {
                        prev_compaction.id.clone()
                    } else {
                        format!("{}:retained:{}", prev_compaction.id, index - 1)
                    }),
                    timestamp: message
                        .as_message()
                        .map(message_timestamp)
                        .unwrap_or(prev_compaction.timestamp),
                    payload: EntryPayload::Message {
                        message: message.clone(),
                        terminate: false,
                    },
                });
            }
            compactable_entries = virtual_retained_entries;
            compactable_entries.extend_from_slice(&path_entries[prev_compaction_index + 1..]);
        }
    }
    let boundary_end = compactable_entries.len();

    let tokens_before =
        estimate_context_tokens(&build_session_context(path_entries, &Default::default()).messages)
            .tokens;

    let cut_point = find_cut_point(
        &compactable_entries,
        0,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index.unwrap_or(0)
    } else {
        cut_point.first_kept_entry_index
    };
    let mut messages_to_summarize: Vec<AgentMessage> = Vec::new();
    for entry in compactable_entries.iter().take(history_end) {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(msg);
        }
    }
    let mut turn_prefix_messages: Vec<AgentMessage> = Vec::new();
    if cut_point.is_split_turn {
        for entry in compactable_entries
            .iter()
            .take(cut_point.first_kept_entry_index)
            .skip(cut_point.turn_start_index.unwrap_or(0))
        {
            if let Some(msg) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }
    let mut retained_tail: Vec<AgentMessage> = Vec::new();
    for entry in compactable_entries
        .iter()
        .skip(cut_point.first_kept_entry_index)
    {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            retained_tail.push(msg);
        }
    }
    let mut file_ops =
        extract_file_operations(&messages_to_summarize, path_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: Some(settings),
    }))
}

fn message_timestamp(message: &pillar_ai::types::Message) -> u64 {
    match message {
        Message::User { timestamp, .. } => *timestamp,
        Message::Assistant(assistant) => assistant.timestamp,
        Message::ToolResult(result) => result.timestamp,
    }
}

fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[Entry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut file_ops = super::utils::create_file_ops();
    if let Some(prev_compaction_index) = prev_compaction_index
        && let EntryPayload::Compaction { details, .. } = &entries[prev_compaction_index].payload
        && let Some(details) = details
        && let Ok(compaction_details) = serde_json::from_value::<CompactionDetails>(details.clone())
    {
        for file in compaction_details.read_files {
            file_ops.read.insert(file);
        }
        for file in compaction_details.modified_files {
            file_ops.edited.insert(file);
        }
    }
    for msg in messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    file_ops
}

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

use crate::harness::compaction::shared::SUMMARIZATION_SYSTEM_PROMPT;
use pillar_ai::AbortSignal;
use pillar_ai::Models;

/// Generate or update a conversation summary and return its provider usage
/// (upstream `generateSummaryWithUsage`).
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    models: &Models,
    model: &pillar_ai::types::Model,
    reserve_tokens: u64,
    signal: Option<AbortSignal>,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<RetryPolicy>,
    callbacks: Option<&mut RetryCallbacks<'_>>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = (0.8 * reserve_tokens as f64).floor() as u64;
    let _ = max_tokens;
    let mut base_prompt: String = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if let Some(custom_instructions) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {custom_instructions}");
    }
    let llm_messages = convert_to_llm(current_messages);
    let conversation_text = super::utils::serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous_summary) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt);

    // The model's reasoning flag and thinking level gate the reasoning
    // option upstream; the port threads it through the shared completion
    // (ModelsStreamOptions does not yet carry per-call reasoning).
    let _ = (thinking_level, model.reasoning);

    let context = pillar_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![Content::text(prompt_text)]),
            timestamp: now_millis(),
        }],
        tools: Vec::new(),
    };
    let response =
        complete_simple_with_retries(models, model, context, signal, retry, callbacks).await;
    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Summarization aborted".to_owned()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_owned())
            ),
        ));
    }

    let text_content = pillar_ai::text::content_text(&response.content, "");
    Ok((text_content, response.usage))
}

fn now_millis() -> u64 {
    // The host's clock: `SystemTime::now` traps on the embedding target.
    pillar_ai::clock::now_millis().max(0) as u64
}

/// Generate compaction summary data from prepared session history (upstream
/// `compact`).
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: &CompactionPreparation,
    models: &Models,
    model: &pillar_ai::types::Model,
    custom_instructions: Option<&str>,
    signal: Option<AbortSignal>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<RetryPolicy>,
    mut callbacks: Option<&mut RetryCallbacks<'_>>,
) -> Result<CompactResult, CompactionError> {
    let CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        ..
    } = preparation;

    let summary_text: String;
    let summary_usage: Usage;

    if *is_split_turn && !turn_prefix_messages.is_empty() {
        let mut history_text = "No prior history.".to_owned();
        let mut history_usage: Option<Usage> = None;
        if !messages_to_summarize.is_empty() {
            let (text, usage) = generate_summary_with_usage(
                messages_to_summarize,
                models,
                model,
                preparation
                    .settings
                    .map(|s| s.reserve_tokens)
                    .unwrap_or(16384),
                signal.clone(),
                custom_instructions,
                previous_summary.as_deref(),
                thinking_level,
                retry,
                callbacks.as_deref_mut(),
            )
            .await?;
            history_text = text;
            history_usage = Some(usage);
        }
        let (prefix_text, prefix_usage) = generate_turn_prefix_summary(
            turn_prefix_messages,
            models,
            model,
            preparation
                .settings
                .map(|s| s.reserve_tokens)
                .unwrap_or(16384),
            signal,
            thinking_level,
            retry,
            callbacks.as_deref_mut(),
        )
        .await?;
        summary_text =
            format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_text}");
        summary_usage = match history_usage {
            Some(history_usage) => combine_usage(&history_usage, &prefix_usage),
            None => prefix_usage,
        };
    } else {
        let (text, usage) = generate_summary_with_usage(
            messages_to_summarize,
            models,
            model,
            preparation
                .settings
                .map(|s| s.reserve_tokens)
                .unwrap_or(16384),
            signal,
            custom_instructions,
            previous_summary.as_deref(),
            thinking_level,
            retry,
            callbacks,
        )
        .await?;
        summary_text = text;
        summary_usage = usage;
    }

    let (read_files, modified_files) = compute_file_lists(file_ops);
    let mut summary = summary_text;
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(CompactResult {
        summary,
        tokens_before: *tokens_before,
        usage: Some(summary_usage),
        retained_tail: retained_tail.clone(),
        details: Some(CompactionDetails {
            read_files,
            modified_files,
        }),
    })
}

/// Summarize a split-turn prefix (upstream `generateTurnPrefixSummary`).
#[allow(clippy::too_many_arguments)]
async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    models: &Models,
    model: &pillar_ai::types::Model,
    reserve_tokens: u64,
    signal: Option<AbortSignal>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<RetryPolicy>,
    callbacks: Option<&mut RetryCallbacks<'_>>,
) -> Result<(String, Usage), CompactionError> {
    let max_tokens = (0.5 * reserve_tokens as f64).floor() as u64;
    let _ = (max_tokens, thinking_level);
    let llm_messages = convert_to_llm(messages);
    let conversation_text = super::utils::serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    let context = pillar_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![Content::text(prompt_text)]),
            timestamp: now_millis(),
        }],
        tools: Vec::new(),
    };
    let response =
        complete_simple_with_retries(models, model, context, signal, retry, callbacks).await;
    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Turn prefix summarization aborted".to_owned()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Turn prefix summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_owned())
            ),
        ));
    }
    Ok((
        pillar_ai::text::content_text(&response.content, ""),
        response.usage,
    ))
}

// Branch summarization error conversion lives with BranchSummaryError; the
// port keeps the error types distinct like upstream.
#[allow(dead_code)]
fn _branch_error_witness(error: BranchSummaryError) -> String {
    error.message
}

#[allow(dead_code)]
fn _assistant_witness(_: AssistantMessage) {}
