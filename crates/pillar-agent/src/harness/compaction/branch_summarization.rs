//! Port of packages/agent/src/harness/compaction/branch-summarization.ts
//! (pi v0.84.3) — the parts that do not require the session module.
//!
//! Ported now: `BranchSummaryError` codes, the summarization prompt
//! constants, and `prepareBranchEntriesForMessages` (the message-level
//! preparation once entries have been converted). `collectEntriesForBranchSummary`
//! and `generateBranchSummary` land with the session module port since
//! they depend on `Session`/`Entry`.

use crate::harness::compaction::shared::estimate_tokens;
use crate::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations,
};
use crate::harness::messages::convert_to_llm;
use crate::types::AgentMessage;

/// Stable branch-summary error codes (upstream `BranchSummaryErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchSummaryErrorCode {
    Aborted,
    SummarizationFailed,
}

impl BranchSummaryErrorCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::SummarizationFailed => "summarization_failed",
        }
    }
}

/// Error returned by branch summarization helpers (upstream
/// `BranchSummaryError`).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BranchSummaryError {
    pub code: BranchSummaryErrorCode,
    pub message: String,
}

impl BranchSummaryError {
    pub fn new(code: BranchSummaryErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Generated branch summary data ready to be persisted (upstream
/// `BranchSummaryResult`).
#[derive(Debug, Clone, Default)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Option<pillar_ai::types::Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// File-operation details stored on generated branch summary entries
/// (upstream `BranchSummaryDetails`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchSummaryDetails {
    /// Files read while exploring the summarized branch.
    pub read_files: Vec<String>,
    /// Files modified while exploring the summarized branch.
    pub modified_files: Vec<String>,
}

/// Prepared branch content for summarization (upstream
/// `BranchPreparation`).
#[derive(Debug, Clone, Default)]
pub struct BranchPreparation {
    /// Messages selected for the branch summary.
    pub messages: Vec<AgentMessage>,
    /// File operations extracted from the branch.
    pub file_ops: FileOperations,
    /// Estimated token count for selected messages.
    pub total_tokens: u64,
}

/// Merge file-operation details from branch summary entries into an
/// accumulator (upstream's first pass inside `prepareBranchEntries`).
fn merge_branch_summary_details(details: &[BranchSummaryDetails], file_ops: &mut FileOperations) {
    for detail in details {
        for file in &detail.read_files {
            file_ops.read.insert(file.clone());
        }
        for file in &detail.modified_files {
            file_ops.edited.insert(file.clone());
        }
    }
}

/// Message-level preparation (upstream `prepareBranchEntries` loop body):
/// newest-first iteration with a token budget. `keep_entry` reports whether
/// the budget wants the current message retained when the budget is hit
/// (upstream keeps compaction/branch-summary entries under 90% of budget).
pub fn prepare_branch_entries_from_messages(
    messages_newest_first: &[AgentMessage],
    branch_summary_details: &[BranchSummaryDetails],
    token_budget: u64,
) -> BranchPreparation {
    let mut file_ops = create_file_ops();
    merge_branch_summary_details(branch_summary_details, &mut file_ops);

    let mut selected: Vec<AgentMessage> = Vec::new();
    let mut total_tokens: u64 = 0;

    for message in messages_newest_first.iter() {
        extract_file_ops_from_message(message, &mut file_ops);
        let tokens = estimate_tokens(message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            break;
        }
        selected.insert(0, message.clone());
        total_tokens += tokens;
    }

    BranchPreparation {
        messages: selected,
        file_ops,
        total_tokens,
    }
}

/// Compute the file lists for a branch summary (upstream
/// `computeFileLists` usage inside `generateBranchSummary`).
pub fn branch_summary_file_lists(preparation: &BranchPreparation) -> (Vec<String>, Vec<String>) {
    compute_file_lists(&preparation.file_ops)
}

/// Append formatted file operations to a summary (upstream
/// `generateBranchSummary` tail).
pub fn finish_branch_summary(summary: &str, preparation: &BranchPreparation) -> String {
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let mut full = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");
    full.push_str(&format_file_operations(&read_files, &modified_files));
    full
}

pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Compose the summarization user prompt (upstream promptText assembly:
/// `<conversation>` wrapper plus instructions with optional replacement).
pub fn build_branch_summary_prompt(
    preparation: &BranchPreparation,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
) -> String {
    let llm_messages = convert_to_llm(&preparation.messages);
    let conversation_text =
        crate::harness::compaction::utils::serialize_conversation(&llm_messages);
    let instructions = match (custom_instructions, replace_instructions) {
        (Some(custom), true) => custom.to_owned(),
        (Some(custom), false) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        (None, _) => BRANCH_SUMMARY_PROMPT.to_owned(),
    };
    format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BashExecutionMessage;
    use pillar_ai::types::Message;

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::Message(Message::User {
            content: pillar_ai::types::UserContent::Text(text.to_owned()),
            timestamp: 1,
        })
    }

    fn bash_message(command: &str, output: &str) -> AgentMessage {
        AgentMessage::BashExecution(Box::new(BashExecutionMessage {
            command: command.to_owned(),
            output: output.to_owned(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1,
            exclude_from_context: false,
        }))
    }

    /// Upstream `prepareBranchEntries` ordering: newest-first input yields
    /// chronological output, budget trims oldest first.
    #[test]
    fn prepares_entries_newest_first_with_budget() {
        // Newest first: bash(700 tokens), user(400 tokens).
        let newest_first = vec![
            bash_message("cmd", &"x".repeat(2400)),
            user_message(&"u".repeat(1600)),
        ];

        // Budget large enough for both.
        let prep = prepare_branch_entries_from_messages(&newest_first, &[], 10_000);
        assert_eq!(prep.messages.len(), 2);
        // Chronological: user first, bash second.
        assert_eq!(prep.messages[0].role_name(), "user");
        assert_eq!(prep.messages[1].role_name(), "bashExecution");
        assert!(prep.total_tokens > 0);

        // Tight budget keeps only the newest message.
        let prep_tight = prepare_branch_entries_from_messages(&newest_first, &[], 800);
        assert_eq!(prep_tight.messages.len(), 1);
        assert_eq!(prep_tight.messages[0].role_name(), "bashExecution");
    }

    #[test]
    fn merges_branch_summary_details_into_file_ops() {
        let details = vec![BranchSummaryDetails {
            read_files: vec!["/kept.ts".to_owned()],
            modified_files: vec!["/edited.ts".to_owned()],
        }];
        let prep = prepare_branch_entries_from_messages(&[], &details, 0);
        let (read_files, modified_files) = branch_summary_file_lists(&prep);
        assert_eq!(read_files, vec!["/kept.ts".to_owned()]);
        assert_eq!(modified_files, vec!["/edited.ts".to_owned()]);
    }

    #[test]
    fn builds_prompt_with_conversation_wrapper() {
        let prep = prepare_branch_entries_from_messages(&[user_message("hello")], &[], 0);
        let prompt = build_branch_summary_prompt(&prep, None, false);
        assert!(prompt.starts_with("<conversation>\n[User]: hello\n</conversation>\n\n"));
        assert!(prompt.contains("## Goal"));

        // Custom instructions append an "Additional focus" line.
        let with_custom = build_branch_summary_prompt(&prep, Some("auth flow"), false);
        assert!(with_custom.contains("Additional focus: auth flow"));

        // replaceInstructions swaps the whole instruction block.
        let replaced = build_branch_summary_prompt(&prep, Some("just the facts"), true);
        assert!(!replaced.contains("## Goal"));
        assert!(replaced.ends_with("just the facts"));
    }

    #[test]
    fn finishes_summary_with_preamble_and_file_ops() {
        let details = vec![BranchSummaryDetails {
            read_files: vec!["/r.ts".to_owned()],
            modified_files: vec![],
        }];
        let prep = prepare_branch_entries_from_messages(&[], &details, 0);
        let finished = finish_branch_summary("## Goal\nstuff", &prep);
        assert!(finished.starts_with(BRANCH_SUMMARY_PREAMBLE));
        assert!(finished.contains("<read-files>\n/r.ts\n</read-files>"));
    }
}
