//! Port of packages/agent/src/harness/session/context.ts (pi v0.84.3).
//!
//! Builds the LLM context from a session path: compaction boundary
//! handling, thinking-level/model/active-tools state derivation, and
//! custom-entry projection.

use pillar_ai::types::StopReason;

use super::types::{AgentMessage, Entry, EntryPayload};
use crate::harness::messages::{create_branch_summary_message, create_compaction_summary_message};

/// Derived session context (upstream `SessionContext`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<SessionModelRef>,
    pub active_tool_names: Option<Vec<String>>,
}

/// The resolved model reference (upstream `{ provider, modelId }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelRef {
    pub provider: String,
    pub model_id: String,
}

/// Caller transform over context entries (upstream
/// `ContextEntryTransform`).
pub type ContextEntryTransform = Arc<dyn Fn(&[Entry]) -> Vec<Entry> + Send + Sync>;

/// Custom-entry projector (upstream
/// `CustomEntryContextMessageProjector`): maps a custom entry to context
/// messages; returning `None`/empty drops it.
pub type CustomEntryProjector = Arc<dyn Fn(&Entry) -> Option<Vec<AgentMessage>> + Send + Sync>;

/// Context build options (upstream `SessionContextBuildOptions`).
#[derive(Default, Clone)]
pub struct SessionContextBuildOptions {
    pub entry_transforms: Vec<ContextEntryTransform>,
    /// Keyed by custom entry type.
    pub entry_projectors: std::collections::BTreeMap<String, CustomEntryProjector>,
}

use std::sync::Arc;

fn derive_session_context_state(
    path_entries: &[Entry],
) -> (String, Option<SessionModelRef>, Option<Vec<String>>) {
    let mut thinking_level = "off".to_owned();
    let mut model: Option<SessionModelRef> = None;
    let mut active_tool_names: Option<Vec<String>> = None;

    for entry in path_entries {
        match &entry.payload {
            EntryPayload::ThinkingLevelChange {
                thinking_level: level,
            } => {
                thinking_level = level.clone();
            }
            EntryPayload::ModelChange { provider, model_id } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            EntryPayload::Message { message } => {
                if let Some(pillar_ai::types::Message::Assistant(assistant)) = message.as_message()
                {
                    model = Some(SessionModelRef {
                        provider: assistant.provider.clone(),
                        model_id: assistant.model.clone(),
                    });
                }
            }
            EntryPayload::ActiveToolsChange {
                active_tool_names: names,
            } => {
                active_tool_names = Some(names.clone());
            }
            _ => {}
        }
    }

    (thinking_level, model, active_tool_names)
}

/// Upstream `defaultContextEntryTransform`: start at the latest compaction
/// (compaction entry + everything after it), or the whole path.
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index: Option<usize> = None;
    for index in (0..path_entries.len()).rev() {
        if matches!(path_entries[index].payload, EntryPayload::Compaction { .. }) {
            compaction_index = Some(index);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(compaction_index) => {
            let mut entries = vec![path_entries[compaction_index].clone()];
            entries.extend_from_slice(&path_entries[compaction_index + 1..]);
            entries
        }
    }
}

/// Apply the default transform then caller transforms (upstream
/// `buildContextEntries`).
pub fn build_context_entries(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<Entry> {
    let mut entries = default_context_entry_transform(path_entries);
    for transform in &options.entry_transforms {
        entries = transform(&entries);
    }
    entries
}

/// Convert one context entry into agent messages (upstream
/// `sessionEntryToContextMessages`).
pub fn session_entry_to_context_messages(
    entry: &Entry,
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match &entry.payload {
        EntryPayload::Message { message } => {
            // Deferred assistant handles are placeholders; they carry no
            // context until the deferred fetch resolves.
            if let Some(pillar_ai::types::Message::Assistant(assistant)) = message.as_message() {
                if assistant.stop_reason == StopReason::Deferred {
                    return Vec::new();
                }
            }
            vec![message.clone()]
        }
        EntryPayload::Compaction {
            summary,
            retained_tail,
            tokens_before,
            ..
        } => {
            let mut messages = vec![create_compaction_summary_message(
                summary.clone(),
                *tokens_before,
                entry.timestamp,
            )];
            messages.extend(retained_tail.iter().cloned());
            messages
        }
        EntryPayload::BranchSummary {
            from_id, summary, ..
        } if !summary.is_empty() => {
            vec![create_branch_summary_message(
                summary.clone(),
                from_id.clone(),
                entry.timestamp,
            )]
        }
        EntryPayload::Custom { custom_type, .. } => match options.entry_projectors.get(custom_type)
        {
            Some(projector) => projector(entry).unwrap_or_default(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Build the session context from a path (upstream `buildSessionContext`).
pub fn build_session_context(
    path_entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> SessionContext {
    let (thinking_level, model, active_tool_names) = derive_session_context_state(path_entries);
    let context_entries = build_context_entries(path_entries, options);
    let messages = context_entries
        .iter()
        .flat_map(|entry| session_entry_to_context_messages(entry, options))
        .collect();
    SessionContext {
        messages,
        thinking_level,
        model,
        active_tool_names,
    }
}
