//! Port of packages/coding-agent/src/core/compaction/branch-summarization.ts
//! (pi v0.84.3): summarization of the branch being left when navigating to
//! a different point in the session tree, so context isn't lost.

use pillar_ai::text::content_text;
use pillar_ai::types::{Message, StopReason, Usage, UserContent};

use crate::core::compaction::driver::{
    SummarizationOptions, SummarizeFn, estimate_tokens, get_summarization_failure,
};
use crate::core::compaction::utils::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::core::messages::convert_to_llm;
use crate::core::session_entries::{SessionEntry, SessionTreeView, get_message_from_entry};

// ============================================================================
// Types
// ============================================================================

/// Result of branch summary generation (upstream `BranchSummaryResult`).
#[derive(Debug, Clone, Default)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub usage: Option<Usage>,
    pub read_files: Option<Vec<String>>,
    pub modified_files: Option<Vec<String>>,
    pub aborted: bool,
    pub error: Option<String>,
}

/// Details stored in a branch summary entry for file tracking (upstream
/// `BranchSummaryDetails`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Prepared branch entries (upstream `BranchPreparation`).
#[derive(Debug, Clone, Default)]
pub struct BranchPreparation {
    /// Messages extracted for summarization, in chronological order.
    pub messages: Vec<crate::core::messages::CodingAgentMessage>,
    /// File operations extracted from tool calls.
    pub file_ops: FileOperations,
    /// Total estimated tokens in the messages.
    pub total_tokens: u64,
}

/// Collected entries for summarization (upstream `CollectEntriesResult`).
#[derive(Debug, Clone, Default)]
pub struct CollectEntriesResult {
    /// Entries to summarize, in chronological order.
    pub entries: Vec<SessionEntry>,
    /// Common ancestor between the old and new position, if any.
    pub common_ancestor_id: Option<String>,
}

// ============================================================================
// Entry collection
// ============================================================================

/// Collect the entries that should be summarized when navigating from one
/// position to another (upstream `collectEntriesForBranchSummary`): walk
/// from `old_leaf_id` back to the common ancestor with `target_id`. Does NOT
/// stop at compaction boundaries — those are included and their summaries
/// become context.
pub fn collect_entries_for_branch_summary(
    session: &SessionTreeView,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> CollectEntriesResult {
    // If no old position, nothing to summarize.
    let Some(old_leaf_id) = old_leaf_id else {
        return CollectEntriesResult::default();
    };

    // Find common ancestor (deepest node on both paths).
    let old_path: std::collections::BTreeSet<String> = session
        .get_branch(old_leaf_id)
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    let target_path = session.get_branch(target_id);

    // targetPath is root-first, so iterate backwards to find the deepest
    // common ancestor.
    let mut common_ancestor_id: Option<String> = None;
    for entry in target_path.iter().rev() {
        if old_path.contains(entry.id()) {
            common_ancestor_id = Some(entry.id().to_string());
            break;
        }
    }

    // Collect entries from the old leaf back to the common ancestor.
    let mut entries: Vec<SessionEntry> = Vec::new();
    let mut current: Option<String> = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if Some(&id) == common_ancestor_id.as_ref() {
            break;
        }
        let Some(entry) = session.get_entry(&id) else {
            break;
        };
        entries.push(entry.clone());
        current = entry.parent_id().map(str::to_string);
    }

    // Reverse to chronological order.
    entries.reverse();

    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

// ============================================================================
// Entry preparation
// ============================================================================

/// Prepare entries for summarization with a token budget (upstream
/// `prepareBranchEntries`). Walks entries from NEWEST to OLDEST, adding
/// messages until the budget is hit, keeping the most recent context when
/// the branch is too long. File operations come from assistant tool calls
/// plus existing pi-generated branch summary details (cumulative tracking).
///
/// `token_budget` of 0 means no limit.
pub fn prepare_branch_entries(entries: &[SessionEntry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<crate::core::messages::CodingAgentMessage> = Vec::new();
    let mut file_ops = FileOperations::new();
    let mut total_tokens: u64 = 0;

    // First pass: collect file ops from ALL entries (even those outside the
    // token budget) so cumulative file tracking from nested branch summaries
    // is captured. Only pi-generated summaries (from_hook == false).
    for entry in entries {
        if let SessionEntry::BranchSummary(branch) = entry {
            if !branch.from_hook {
                if let Some(details) = &branch.details {
                    if let Ok(parsed) =
                        serde_json::from_value::<BranchSummaryDetails>(details.clone())
                    {
                        for file in parsed.read_files {
                            file_ops.read.insert(file);
                        }
                        // Modified files go into edited for deduplication.
                        for file in parsed.modified_files {
                            file_ops.edited.insert(file);
                        }
                    }
                }
            }
        }
    }

    // Second pass: walk from newest to oldest, adding messages until budget.
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };

        // Extract file ops from assistant messages (tool calls).
        extract_file_ops_from_message(&message, &mut file_ops);

        let tokens = estimate_tokens(&message);

        if token_budget > 0 && total_tokens + tokens > token_budget {
            // Summary entries try to fit anyway — they are important context.
            if matches!(
                entry,
                SessionEntry::Compaction(_) | SessionEntry::BranchSummary(_)
            ) && total_tokens < token_budget * 9 / 10
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            // Stop — budget hit.
            break;
        }

        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

// ============================================================================
// Summary generation
// ============================================================================

pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Generation options (upstream `GenerateBranchSummaryOptions` minus the
/// retry plumbing, which lands with the retry port).
pub struct GenerateBranchSummaryOptions<'a> {
    pub model: &'a pillar_ai::types::Model,
    pub signal: Option<pillar_ai::abort::AbortSignal>,
    pub custom_instructions: Option<&'a str>,
    /// If true, custom instructions replace the default prompt.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt + LLM response (default 16384).
    pub reserve_tokens: u64,
    pub stream_fn: &'a dyn SummarizeFn,
}

/// Generate a summary of abandoned branch entries (upstream
/// `generateBranchSummary`).
pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    options: GenerateBranchSummaryOptions<'_>,
) -> BranchSummaryResult {
    let GenerateBranchSummaryOptions {
        model,
        signal,
        custom_instructions,
        replace_instructions,
        reserve_tokens,
        stream_fn,
    } = options;

    // Token budget = context window minus reserved space.
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        128_000
    };
    let token_budget = context_window.saturating_sub(reserve_tokens);

    let preparation = prepare_branch_entries(entries, token_budget);
    if preparation.messages.is_empty() {
        return BranchSummaryResult {
            summary: Some("No content to summarize".to_string()),
            ..Default::default()
        };
    }

    // Transform to LLM-compatible messages, then serialize to text.
    let conversation_text = serialize_conversation(&convert_to_llm(&preparation.messages));

    // Build the prompt.
    let instructions: String = if replace_instructions {
        custom_instructions
            .unwrap_or(BRANCH_SUMMARY_PROMPT)
            .to_string()
    } else if let Some(custom_instructions) = custom_instructions {
        format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom_instructions}")
    } else {
        BRANCH_SUMMARY_PROMPT.to_string()
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let context = Message::User {
        content: UserContent::Blocks(vec![pillar_ai::types::Content::text(prompt_text)]),
        timestamp: pillar_ai::models::now_ms(),
    };
    let request_context = pillar_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![context],
        tools: Vec::new(),
    };
    let request_options = SummarizationOptions {
        signal,
        max_tokens: Some(2048),
        ..Default::default()
    };
    let Ok(response) = crate::core::compaction::driver::complete_summarization(
        model,
        &request_context,
        request_options,
        stream_fn,
    )
    .await
    else {
        return BranchSummaryResult {
            error: Some("Branch summarization stream failed".to_string()),
            ..Default::default()
        };
    };

    // Check if aborted or errored.
    if response.stop_reason == StopReason::Aborted {
        return BranchSummaryResult {
            aborted: true,
            ..Default::default()
        };
    }
    if let Some(failure) = get_summarization_failure(&response, "Branch summarization") {
        return BranchSummaryResult {
            error: Some(failure),
            ..Default::default()
        };
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, pillar_ai::types::Content::ToolCall { .. }))
    {
        return BranchSummaryResult {
            error: Some("Branch summarization attempted to call a tool".to_string()),
            ..Default::default()
        };
    }

    let mut summary = content_text(&response.content, "");

    // Prepend the preamble for context about the branch summary.
    summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");

    // Compute file lists and append to the summary.
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    BranchSummaryResult {
        summary: Some(if summary.is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        }),
        usage: Some(response.usage),
        read_files: Some(read_files),
        modified_files: Some(modified_files),
        aborted: false,
        error: None,
    }
}
