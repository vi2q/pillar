//! Port of packages/coding-agent/src/core/compaction/compaction.ts (pi
//! v0.84.3): pure functions for context compaction — token estimation, cut
//! point detection, summarization prompts, and the compact() driver.
//!
//! divergence: upstream consumes session-manager `SessionEntry` shapes; the
//! port operates on `CodingAgentMessage` streams plus a session-entry
//! abstraction declared here (the session manager itself is not yet
//! ported). The LLM call goes through a caller-supplied stream function.

use pillar_ai::text::content_text;
use pillar_ai::types::{AssistantMessage, Context, Message, StopReason, Usage};

pub use crate::core::compaction::utils::SUMMARIZATION_SYSTEM_PROMPT;
use crate::core::compaction::utils::{
    FileOperations, compute_file_lists, extract_file_ops_from_message, format_file_operations,
    serialize_conversation,
};
use crate::core::messages::CodingAgentMessage;

// ============================================================================
// Compaction settings
// ============================================================================

/// Compaction settings (upstream `CompactionSettings`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

// ============================================================================
// Token calculation
// ============================================================================

/// Calculate total context tokens from usage. Uses the native totalTokens
/// field when available, falls back to computing from components (upstream
/// `calculateContextTokens`).
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        return usage.total_tokens;
    }
    usage.input + usage.output + usage.cache_read + usage.cache_write
}

/// Get usage from an assistant message if valid (upstream
/// `getAssistantUsage`): skips aborted, error, and all-zero usage messages.
fn get_assistant_usage(msg: &CodingAgentMessage) -> Option<Usage> {
    let CodingAgentMessage::Base(Message::Assistant(assistant)) = msg else {
        return None;
    };
    if assistant.stop_reason != StopReason::Aborted
        && assistant.stop_reason != StopReason::Error
        && calculate_context_tokens(&assistant.usage) > 0
    {
        return Some(assistant.usage.clone());
    }
    None
}

/// Find the last valid assistant message usage (upstream
/// `getLastAssistantUsage` over session entries).
pub fn get_last_assistant_usage(messages: &[CodingAgentMessage]) -> Option<Usage> {
    messages.iter().rev().find_map(get_assistant_usage)
}

/// Context usage estimate (upstream `ContextUsageEstimate`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

/// Estimate context tokens from messages, using the last assistant usage
/// when available. Messages after the last usage are estimated with
/// `estimate_tokens` (upstream `estimateContextTokens`).
pub fn estimate_context_tokens(messages: &[CodingAgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, msg)| get_assistant_usage(msg).map(|usage| (usage, index)));

    let Some((usage, index)) = usage_info else {
        let estimated: u64 = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(&usage);
    let mut trailing_tokens: u64 = 0;
    for message in &messages[index + 1..] {
        trailing_tokens += estimate_tokens(message);
    }

    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Check if compaction should trigger based on context usage (upstream
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

// ============================================================================
// Token estimation (chars/4 heuristic)
// ============================================================================

const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn estimate_user_content_chars(content: &pillar_ai::types::UserContent) -> usize {
    match content {
        pillar_ai::types::UserContent::Text(text) => text.chars().count(),
        pillar_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                pillar_ai::types::Content::Text { text, .. } => text.chars().count(),
                pillar_ai::types::Content::Image { .. } => ESTIMATED_IMAGE_CHARS,
                _ => 0,
            })
            .sum(),
    }
}

fn estimate_blocks_chars(blocks: &[pillar_ai::types::Content]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            pillar_ai::types::Content::Text { text, .. } => text.chars().count(),
            pillar_ai::types::Content::Image { .. } => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum()
}

/// Estimate token count for a message using the chars/4 heuristic. This is
/// conservative (overestimates tokens) (upstream `estimateTokens`).
pub fn estimate_tokens(message: &CodingAgentMessage) -> u64 {
    let chars: usize = match message {
        CodingAgentMessage::Base(Message::User { content, .. }) => {
            return (estimate_user_content_chars(content) as u64).div_ceil(4);
        }
        CodingAgentMessage::Base(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .map(|block| match block {
                pillar_ai::types::Content::Text { text, .. } => text.chars().count(),
                pillar_ai::types::Content::Thinking { thinking, .. } => thinking.chars().count(),
                pillar_ai::types::Content::ToolCall {
                    name, arguments, ..
                } => {
                    name.chars().count()
                        + serde_json::to_string(arguments)
                            .map(|s| s.len())
                            .unwrap_or(0)
                }
                _ => 0,
            })
            .sum(),
        CodingAgentMessage::Base(Message::ToolResult(result)) => {
            estimate_blocks_chars(&result.content)
        }
        CodingAgentMessage::Custom(custom) => custom
            .content
            .iter()
            .map(|content| match content {
                crate::core::messages::CustomContent::Text(text) => text.chars().count(),
                crate::core::messages::CustomContent::Image { .. } => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        CodingAgentMessage::BashExecution(bash) => {
            bash.command.chars().count() + bash.output.chars().count()
        }
        CodingAgentMessage::BranchSummary(summary) => summary.summary.chars().count(),
        CodingAgentMessage::CompactionSummary(summary) => summary.summary.chars().count(),
    };
    (chars as u64).div_ceil(4)
}

// ============================================================================
// Cut point detection
// ============================================================================

fn is_cut_point_message(message: &CodingAgentMessage) -> bool {
    !matches!(message, CodingAgentMessage::Base(Message::ToolResult(_)))
}

fn is_turn_start_message(message: &CodingAgentMessage) -> bool {
    !matches!(
        message,
        CodingAgentMessage::Base(Message::Assistant(_))
            | CodingAgentMessage::Base(Message::ToolResult(_))
    )
}

/// Find valid cut points: indices of context-visible user-like or assistant
/// messages. Never cut at tool results (upstream `findValidCutPoints` over
/// session entries; the port works directly on messages).
fn find_valid_cut_points(
    messages: &[CodingAgentMessage],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (i, message) in messages
        .iter()
        .enumerate()
        .take(end_index)
        .skip(start_index)
    {
        if is_cut_point_message(message) {
            cut_points.push(i);
        }
    }
    cut_points
}

/// Find the user-role message that starts the turn containing the given
/// message index (upstream `findTurnStartIndex`). Returns None when no turn
/// start exists before the index.
pub fn find_turn_start_index(
    messages: &[CodingAgentMessage],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&i| is_turn_start_message(&messages[i]))
}

/// Cut point result (upstream `CutPointResult`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CutPointResult {
    /// Index of first message to keep.
    pub first_kept_entry_index: usize,
    /// Index of user message that starts the turn being split, if splitting.
    pub turn_start_index: Option<usize>,
    /// Whether this cut splits a turn (cut point is not a user message).
    pub is_split_turn: bool,
}

/// Find the cut point that keeps approximately `keepRecentTokens` (upstream
/// `findCutPoint`). Walk backwards from newest, accumulating estimated
/// message sizes; stop when the budget is exceeded. Can cut at user OR
/// assistant messages (never tool results). Only considers messages between
/// `start_index` and `end_index` (exclusive).
pub fn find_cut_point(
    messages: &[CodingAgentMessage],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(messages, start_index, end_index);

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    // Walk backwards from newest, accumulating estimated message sizes.
    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0]; // Default: keep from first message

    for i in (start_index..end_index).rev() {
        let message_tokens = estimate_tokens(&messages[i]);
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens += message_tokens;

        if accumulated_tokens >= keep_recent_tokens {
            // Find the closest valid cut point at or after this entry.
            for &cp in &cut_points {
                if cp >= i {
                    cut_index = cp;
                    break;
                }
            }
            break;
        }
    }

    // Determine if this is a split turn.
    let starts_turn = is_turn_start_message(&messages[cut_index]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_turn_start_index(messages, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

// ============================================================================
// Summarization
// ============================================================================

pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

/// Returns an error message when a summarization response cannot safely be
/// persisted (upstream `getSummarizationFailure`). A length stop contains
/// partial text and must not become a session checkpoint.
pub fn get_summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
    if response.stop_reason == StopReason::Error {
        return Some(format!(
            "{label} failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        ));
    }
    if response.stop_reason == StopReason::Length {
        return Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        ));
    }
    None
}

fn build_summarization_context(prompt_text: &str) -> Context {
    Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User {
            content: pillar_ai::types::UserContent::Blocks(vec![pillar_ai::types::Content::text(
                prompt_text,
            )]),
            timestamp: pillar_ai::models::now_ms(),
        }],
        tools: Vec::new(),
    }
}

/// The single LLM call choke point for every compaction/branch-summary
/// summarization (upstream `completeSummarization`): invokes the
/// caller-supplied stream function with the summarization options.
///
/// divergence: retry policy plumbing lands with the retry-callbacks port;
/// the port performs a single call.
pub async fn complete_summarization(
    model: &Model,
    context: &Context,
    options: SummarizationOptions,
    stream_fn: &dyn SummarizeFn,
) -> Result<AssistantMessage, String> {
    // Avoid cache writes for one-off summaries. Reuse caller-supplied
    // routing when available; callers without a session ID, including branch
    // summaries, receive a fresh routing ID.
    let mut request_options = options.clone();
    request_options
        .session_id
        .get_or_insert_with(fresh_session_id);

    let response = stream_fn.call(model, context, &request_options).await?;
    Ok(response)
}

/// Options threaded to the summarization stream function (upstream
/// `SimpleStreamOptions` subset).
#[derive(Debug, Clone, Default)]
pub struct SummarizationOptions {
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub signal: Option<pillar_ai::abort::AbortSignal>,
    pub reasoning: Option<String>,
    pub session_id: Option<String>,
}

/// Stream function used for summarization calls.
pub trait SummarizeFn: Send + Sync {
    fn call(
        &self,
        model: &Model,
        context: &Context,
        options: &SummarizationOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AssistantMessage, String>> + Send>>;
}

impl<F> SummarizeFn for F
where
    F: Fn(
            &Model,
            &Context,
            &SummarizationOptions,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<AssistantMessage, String>> + Send>,
        > + Send
        + Sync,
{
    fn call(
        &self,
        model: &Model,
        context: &Context,
        options: &SummarizationOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AssistantMessage, String>> + Send>>
    {
        self(model, context, options)
    }
}

fn fresh_session_id() -> String {
    pillar_ai::uuid::uuidv7()
}

use pillar_ai::types::Model;

/// Generate or update a conversation summary and return its provider usage
/// (upstream `generateSummaryWithUsage`).
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary_with_usage(
    current_messages: &[CodingAgentMessage],
    model: &Model,
    reserve_tokens: u64,
    options: SummarizationOptions,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    stream_fn: &dyn SummarizeFn,
) -> Result<(String, Usage), String> {
    let max_tokens = if model.max_tokens > 0 {
        ((reserve_tokens as f64 * 0.8) as u64).min(model.max_tokens)
    } else {
        (reserve_tokens as f64 * 0.8) as u64
    };

    // Use update prompt if we have a previous summary, otherwise initial prompt.
    let base_prompt: String = {
        let base = if previous_summary.is_some() {
            UPDATE_SUMMARIZATION_PROMPT
        } else {
            SUMMARIZATION_PROMPT
        };
        match custom_instructions {
            Some(custom_instructions) => {
                format!("{base}\n\nAdditional focus: {custom_instructions}")
            }
            None => base.to_string(),
        }
    };

    // Serialize conversation to text so model doesn't try to continue it.
    let conversation_text = serialize_conversation(&convert_to_llm(current_messages));

    // Build the prompt with conversation wrapped in tags.
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous_summary) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt);

    let completion_options = SummarizationOptions {
        max_tokens: Some(max_tokens),
        ..options
    };

    let response = complete_summarization(
        model,
        &build_summarization_context(&prompt_text),
        completion_options,
        stream_fn,
    )
    .await?;

    if let Some(failure) = get_summarization_failure(&response, "Summarization") {
        return Err(failure);
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, pillar_ai::types::Content::ToolCall { .. }))
    {
        return Err("Summarization attempted to call a tool".to_string());
    }

    Ok((content_text(&response.content, ""), response.usage))
}

// ============================================================================
// Compaction preparation and driver
// ============================================================================

/// Details stored in a compaction entry for file tracking (upstream
/// `CompactionDetails`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Extract file operations from messages (upstream `extractFileOperations`
/// without the previous-compaction-entries half, which needs session
/// entries).
pub fn extract_file_operations(messages: &[CodingAgentMessage]) -> FileOperations {
    let mut file_ops = FileOperations::new();
    for msg in messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    file_ops
}

/// Result from `compact` (upstream `CompactionResult`).
#[derive(Debug, Clone, Default)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    /// Usage from the LLM call(s) that generated this summary.
    pub usage: Option<Usage>,
    pub details: Option<CompactionDetails>,
}

/// Compaction preparation (upstream `CompactionPreparation`).
#[derive(Debug, Clone, Default)]
pub struct CompactionPreparation {
    /// Id of the first entry to keep.
    pub first_kept_entry_id: String,
    /// Messages that will be summarized and discarded.
    pub messages_to_summarize: Vec<CodingAgentMessage>,
    /// Messages that will be turned into a turn prefix summary (if splitting).
    pub turn_prefix_messages: Vec<CodingAgentMessage>,
    /// Whether this is a split turn (cut point in the middle of a turn).
    pub is_split_turn: bool,
    pub tokens_before: u64,
    /// Summary from the previous compaction, for iterative update.
    pub previous_summary: Option<String>,
    /// File operations extracted from messages to summarize.
    pub file_ops: FileOperations,
    /// Compaction settings.
    pub settings: CompactionSettings,
}

/// Prepare compaction inputs from the context-visible message stream
/// (upstream `prepareCompaction` over session entries; the port takes the
/// already-extracted message list and a first-kept-id resolver).
///
/// `first_kept_entry_id_of(index)` maps a cut index to its entry id; return
/// None to signal "session needs migration".
pub fn prepare_compaction(
    messages: &[CodingAgentMessage],
    settings: CompactionSettings,
    first_kept_entry_id_of: impl Fn(usize) -> Option<String>,
) -> Option<CompactionPreparation> {
    let tokens_before = estimate_context_tokens(messages).tokens;
    let cut_point = find_cut_point(messages, 0, messages.len(), settings.keep_recent_tokens);

    let first_kept_entry_id = first_kept_entry_id_of(cut_point.first_kept_entry_index)?;

    let history_end = if cut_point.is_split_turn {
        cut_point
            .turn_start_index
            .unwrap_or(cut_point.first_kept_entry_index)
    } else {
        cut_point.first_kept_entry_index
    };

    // Messages to summarize (will be discarded after summary).
    let messages_to_summarize: Vec<CodingAgentMessage> = messages[..history_end].to_vec();

    // Messages for the turn prefix summary (if splitting a turn).
    let turn_prefix_messages: Vec<CodingAgentMessage> = if cut_point.is_split_turn {
        messages[cut_point.turn_start_index.unwrap()..cut_point.first_kept_entry_index].to_vec()
    } else {
        Vec::new()
    };

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut file_ops = extract_file_operations(&messages_to_summarize);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary: None,
        file_ops,
        settings,
    })
}

/// Generate summaries for compaction using prepared data (upstream
/// `compact`).
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: CompactionPreparation,
    model: &Model,
    options: SummarizationOptions,
    custom_instructions: Option<&str>,
    stream_fn: &dyn SummarizeFn,
) -> Result<CompactionResult, String> {
    let CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    } = preparation;

    // Generate summaries and merge into one.
    let (summary, summary_usage): (String, Option<Usage>) =
        if is_split_turn && !turn_prefix_messages.is_empty() {
            let mut history_text = "No prior history.".to_string();
            let mut history_usage: Option<Usage> = None;
            if !messages_to_summarize.is_empty() {
                let (text, usage) = generate_summary_with_usage(
                    &messages_to_summarize,
                    model,
                    settings.reserve_tokens,
                    options.clone(),
                    custom_instructions,
                    previous_summary.as_deref(),
                    stream_fn,
                )
                .await?;
                history_text = text;
                history_usage = Some(usage);
            }
            let turn_prefix_result = generate_turn_prefix_summary(
                &turn_prefix_messages,
                model,
                settings.reserve_tokens,
                options,
                stream_fn,
            )
            .await?;
            // Merge into a single summary.
            let summary = format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                turn_prefix_result.0
            );
            (
                summary,
                history_usage
                    .map(|u| combine_usage(&u, &turn_prefix_result.1))
                    .or(Some(turn_prefix_result.1)),
            )
        } else {
            let (text, usage) = generate_summary_with_usage(
                &messages_to_summarize,
                model,
                settings.reserve_tokens,
                options,
                custom_instructions,
                previous_summary.as_deref(),
                stream_fn,
            )
            .await?;
            (text, Some(usage))
        };

    // Compute file lists and append to the summary.
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );

    if first_kept_entry_id.is_empty() {
        return Err("First kept entry has no UUID - session may need migration".to_string());
    }

    Ok(CompactionResult {
        summary,
        first_kept_entry_id,
        tokens_before,
        estimated_tokens_after: None,
        usage: summary_usage,
        details: Some(CompactionDetails {
            read_files,
            modified_files,
        }),
    })
}

/// Combine two usage records field-wise (upstream `combineUsage`).
fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: match (first.cache_write_1h, second.cache_write_1h) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
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

/// Generate a summary for a turn prefix (when splitting a turn) (upstream
/// `generateTurnPrefixSummary`).
async fn generate_turn_prefix_summary(
    messages: &[CodingAgentMessage],
    model: &Model,
    reserve_tokens: u64,
    options: SummarizationOptions,
    stream_fn: &dyn SummarizeFn,
) -> Result<(String, Usage), String> {
    // Smaller budget for the turn prefix.
    let max_tokens = if model.max_tokens > 0 {
        ((reserve_tokens as f64 * 0.5) as u64).min(model.max_tokens)
    } else {
        (reserve_tokens as f64 * 0.5) as u64
    };
    let conversation_text = serialize_conversation(&convert_to_llm(messages));
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let response = complete_summarization(
        model,
        &build_summarization_context(&prompt_text),
        SummarizationOptions {
            max_tokens: Some(max_tokens),
            ..options
        },
        stream_fn,
    )
    .await?;

    if let Some(failure) = get_summarization_failure(&response, "Turn prefix summarization") {
        return Err(failure);
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, pillar_ai::types::Content::ToolCall { .. }))
    {
        return Err("Turn prefix summarization attempted to call a tool".to_string());
    }

    Ok((content_text(&response.content, ""), response.usage))
}

/// Convenience wrapper returning only the summary text (upstream
/// `generateSummary`).
pub async fn generate_summary(
    current_messages: &[CodingAgentMessage],
    model: &Model,
    reserve_tokens: u64,
    options: SummarizationOptions,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    stream_fn: &dyn SummarizeFn,
) -> Result<String, String> {
    generate_summary_with_usage(
        current_messages,
        model,
        reserve_tokens,
        options,
        custom_instructions,
        previous_summary,
        stream_fn,
    )
    .await
    .map(|(text, _)| text)
}

fn convert_to_llm(messages: &[CodingAgentMessage]) -> Vec<Message> {
    crate::core::messages::convert_to_llm(messages)
}
