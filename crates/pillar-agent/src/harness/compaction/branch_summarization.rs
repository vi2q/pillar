//! Port of packages/agent/src/harness/compaction/branch-summarization.ts
//! (pi v0.84.3).
//!
//! Full port: `BranchSummaryError` codes, the summarization prompt
//! constants, `collectEntriesForBranchSummary` (over the shared
//! `Session` facade), `prepareBranchEntries`, and `generateBranchSummary`.

use pillar_ai::retry::{RetryCallbacks, RetryPolicy};
use pillar_ai::types::{Message, StopReason, UserContent};

use crate::harness::compaction::shared::{
    SUMMARIZATION_SYSTEM_PROMPT, complete_simple_with_retries, estimate_tokens,
};
use crate::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::harness::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::harness::session::memory::Session;
use crate::harness::session::types::{Entry, EntryPayload, SessionError, SessionErrorCode};
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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// Entries selected for branch summarization (upstream
/// `CollectEntriesResult`).
#[derive(Debug, Clone)]
pub struct CollectEntriesResult {
    /// Entries to summarize in chronological order.
    pub entries: Vec<Entry>,
    /// Deepest common ancestor between the previous leaf and target entry.
    pub common_ancestor_id: Option<String>,
}

/// Options for generating a branch summary (upstream
/// `GenerateBranchSummaryOptions`).
pub struct GenerateBranchSummaryOptions<'a> {
    /// Provider collection the summarization request goes through.
    pub models: &'a pillar_ai::Models,
    /// Model used for summarization.
    pub model: &'a pillar_ai::types::Model,
    /// Abort signal for the summarization request.
    pub signal: Option<pillar_ai::AbortSignal>,
    /// Optional instructions appended to or replacing the default prompt.
    pub custom_instructions: Option<String>,
    /// Replace the default prompt with custom instructions instead of
    /// appending them.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt and model output. Defaults to 16384.
    pub reserve_tokens: Option<u64>,
    /// Optional retry policy for transient summarization errors.
    pub retry: Option<RetryPolicy>,
    /// Optional callbacks for retry reporting.
    pub callbacks: Option<RetryCallbacks<'a>>,
}

/// Collect entries that should be summarized before navigating to a
/// different session tree entry (upstream `collectEntriesForBranchSummary`).
pub fn collect_entries_for_branch_summary(
    session: &Session,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> Result<CollectEntriesResult, SessionError> {
    let Some(old_leaf_id) = old_leaf_id else {
        return Ok(CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        });
    };
    let old_path: std::collections::BTreeSet<String> = session
        .find_entries_on_branch(
            &crate::harness::session::types::EntryQuery {
                start: Some(old_leaf_id.to_owned()),
                ..Default::default()
            },
            &Default::default(),
        )?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    let target_path = session.find_entries_on_branch(
        &crate::harness::session::types::EntryQuery {
            start: Some(target_id.to_owned()),
            ..Default::default()
        },
        &Default::default(),
    )?;
    let mut common_ancestor_id: Option<String> = None;
    for entry in &target_path {
        if old_path.contains(&entry.id) {
            common_ancestor_id = Some(entry.id.clone());
            break;
        }
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<String> = Some(old_leaf_id.to_owned());
    while let Some(id) = current {
        if Some(&id) == common_ancestor_id.as_ref() {
            break;
        }
        let entry = session.get_entry(&id)?.ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::InvalidEntry,
                format!("Entry {id} not found"),
            )
        })?;
        current = entry.parent_id.clone();
        entries.push(entry);
    }
    entries.reverse();

    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

/// Extract the summarization-relevant message from an entry (upstream
/// `getMessageFromEntry`).
fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match &entry.payload {
        EntryPayload::Message { message, .. } => match message {
            AgentMessage::Message(Message::ToolResult { .. }) => None,
            other => Some(other.clone()),
        },
        EntryPayload::BranchSummary {
            from_id, summary, ..
        } => Some(create_branch_summary_message(
            summary.clone(),
            from_id.clone(),
            entry.timestamp,
        )),
        EntryPayload::Compaction {
            summary,
            tokens_before,
            ..
        } => Some(create_compaction_summary_message(
            summary.clone(),
            *tokens_before,
            entry.timestamp,
        )),
        EntryPayload::ThinkingLevelChange { .. }
        | EntryPayload::ModelChange { .. }
        | EntryPayload::ActiveToolsChange { .. }
        | EntryPayload::Custom { .. } => None,
    }
}

/// Prepare branch entries for summarization within an optional token budget
/// (upstream `prepareBranchEntries`).
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens: u64 = 0;

    for entry in entries {
        if let EntryPayload::BranchSummary {
            details: Some(details),
            ..
        } = &entry.payload
            && let Ok(parsed) = serde_json::from_value::<BranchSummaryDetails>(details.clone())
        {
            for f in &parsed.read_files {
                file_ops.read.insert(f.clone());
            }
            for f in &parsed.modified_files {
                file_ops.edited.insert(f.clone());
            }
        }
    }

    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);

        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(
                entry.payload,
                EntryPayload::Compaction { .. } | EntryPayload::BranchSummary { .. }
            ) && total_tokens < (token_budget as f64 * 0.9) as u64
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
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
    let conversation_text = serialize_conversation(&llm_messages);
    let instructions = match (custom_instructions, replace_instructions) {
        (Some(custom), true) => custom.to_owned(),
        (Some(custom), false) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        (None, _) => BRANCH_SUMMARY_PROMPT.to_owned(),
    };
    format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}")
}

/// Generate a summary for abandoned branch entries (upstream
/// `generateBranchSummary`).
pub async fn generate_branch_summary(
    entries: &[Entry],
    options: GenerateBranchSummaryOptions<'_>,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let GenerateBranchSummaryOptions {
        models,
        model,
        signal,
        custom_instructions,
        replace_instructions,
        reserve_tokens,
        retry,
        callbacks,
    } = options;
    let context_window = if model.context_window == 0 {
        128_000
    } else {
        model.context_window
    };
    let token_budget = context_window.saturating_sub(reserve_tokens.unwrap_or(16_384));

    let preparation = prepare_branch_entries(entries, token_budget);

    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_owned(),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }
    let prompt_text = build_branch_summary_prompt(
        &preparation,
        custom_instructions.as_deref(),
        replace_instructions,
    );

    let context = pillar_ai::types::Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message::User {
            content: UserContent::Blocks(vec![pillar_ai::types::Content::text(prompt_text)]),
            timestamp: now_millis(),
        }],
        tools: Vec::new(),
    };
    let mut callbacks = callbacks;
    let response = complete_simple_with_retries(
        models,
        model,
        context,
        signal,
        // upstream: { signal, maxTokens: 2048 }
        retry,
        callbacks.as_mut(),
    )
    .await;
    if response.stop_reason == StopReason::Aborted {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Branch summary aborted".to_owned()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::SummarizationFailed,
            format!(
                "Branch summary failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_owned())
            ),
        ));
    }

    let text = pillar_ai::text::content_text(&response.content, "");
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{text}");
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(BranchSummaryResult {
        summary: if summary.is_empty() {
            "No summary generated".to_owned()
        } else {
            summary
        },
        usage: Some(response.usage),
        read_files,
        modified_files,
    })
}

fn now_millis() -> u64 {
    // The host's clock: `SystemTime::now` traps on the embedding target.
    pillar_ai::clock::now_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::session::memory::InMemorySessionStorage;
    use crate::harness::session::types::{Entry, EntryPayload, SessionMetadata};
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

    fn message_entry(id: &str, parent: Option<&str>, message: AgentMessage) -> Entry {
        Entry {
            id: id.to_owned(),
            seq: 0,
            parent_id: parent.map(str::to_owned),
            timestamp: 0,
            payload: EntryPayload::Message {
                message,
                terminate: false,
            },
        }
    }

    /// Upstream `prepareBranchEntries` ordering: chronological entries yield
    /// chronological messages, budget trims oldest first.
    #[test]
    fn prepares_entries_newest_first_with_budget() {
        let entries = vec![
            message_entry("first", None, user_message(&"u".repeat(1600))),
            message_entry(
                "second",
                Some("first"),
                bash_message("cmd", &"x".repeat(2400)),
            ),
        ];

        // Budget large enough for both.
        let prep = prepare_branch_entries(&entries, 10_000);
        assert_eq!(prep.messages.len(), 2);
        assert_eq!(prep.messages[0].role_name(), "user");
        assert_eq!(prep.messages[1].role_name(), "bashExecution");
        assert!(prep.total_tokens > 0);

        // Tight budget keeps only the newest message.
        let prep_tight = prepare_branch_entries(&entries, 800);
        assert_eq!(prep_tight.messages.len(), 1);
        assert_eq!(prep_tight.messages[0].role_name(), "bashExecution");
    }

    #[test]
    fn merges_branch_summary_details_into_file_ops() {
        let details = serde_json::json!({
            "readFiles": ["/kept.ts"],
            "modifiedFiles": ["/edited.ts"],
        });
        let entry = Entry {
            id: "bs".to_owned(),
            seq: 0,
            parent_id: None,
            timestamp: 0,
            payload: EntryPayload::BranchSummary {
                from_id: "old".to_owned(),
                summary: "s".to_owned(),
                details: Some(details),
                usage: None,
            },
        };
        let prep = prepare_branch_entries(&[entry], 0);
        let (read_files, modified_files) = branch_summary_file_lists(&prep);
        assert_eq!(read_files, vec!["/kept.ts".to_owned()]);
        assert_eq!(modified_files, vec!["/edited.ts".to_owned()]);
    }

    #[test]
    fn builds_prompt_with_conversation_wrapper() {
        let entries = vec![message_entry("m", None, user_message("hello"))];
        let prep = prepare_branch_entries(&entries, 0);
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
        let details = serde_json::json!({
            "readFiles": ["/r.ts"],
            "modifiedFiles": [],
        });
        let entry = Entry {
            id: "bs".to_owned(),
            seq: 0,
            parent_id: None,
            timestamp: 0,
            payload: EntryPayload::BranchSummary {
                from_id: "old".to_owned(),
                summary: "s".to_owned(),
                details: Some(details),
                usage: None,
            },
        };
        let prep = prepare_branch_entries(&[entry], 0);
        let finished = finish_branch_summary("## Goal\nstuff", &prep);
        assert!(finished.starts_with(BRANCH_SUMMARY_PREAMBLE));
        assert!(finished.contains("<read-files>\n/r.ts\n</read-files>"));
    }

    // --- branch-summarization.test.ts ------------------------------------

    fn test_session() -> Session {
        Session::new(Box::new(InMemorySessionStorage::new(SessionMetadata {
            id: "session".to_owned(),
            created_at: 1,
            parent_session_id: None,
        })))
    }

    /// upstream test: "collects the abandoned side of a branch in
    /// chronological order"
    #[test]
    fn collects_the_abandoned_side_of_a_branch_in_chronological_order() {
        let session = test_session();
        let root_id = session
            .append_message(user_message("root"))
            .expect("append root");
        let common_id = session
            .append_message(user_message("common"))
            .expect("append common");
        let abandoned_1 = session
            .append_message(user_message("abandoned 1"))
            .expect("append abandoned 1");
        let abandoned_2 = session
            .append_message(user_message("abandoned 2"))
            .expect("append abandoned 2");
        session
            .create_lane("target", Some(&common_id))
            .expect("create lane");
        let target_id = session
            .append_message_to_lane("target", user_message("target"))
            .expect("append target");

        let result = collect_entries_for_branch_summary(&session, Some(&abandoned_2), &target_id)
            .expect("collect");
        assert_eq!(
            result.common_ancestor_id.as_deref(),
            Some(common_id.as_str())
        );
        assert_eq!(
            result
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec![abandoned_1.as_str(), abandoned_2.as_str()]
        );
        assert!(!result.entries.iter().any(|entry| entry.id == root_id));
    }

    /// upstream test: "returns no entries when there was no previous leaf"
    #[test]
    fn returns_no_entries_when_there_was_no_previous_leaf() {
        let session = test_session();
        let target_id = session
            .append_message(user_message("target"))
            .expect("append target");
        let result =
            collect_entries_for_branch_summary(&session, None, &target_id).expect("collect");
        assert!(result.entries.is_empty());
        assert_eq!(result.common_ancestor_id, None);
    }
}
