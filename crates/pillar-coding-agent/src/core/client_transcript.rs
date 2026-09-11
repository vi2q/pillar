//! Port of packages/coding-agent/src/client/transcript.ts (pi v0.84.3):
//! the remote-session transcript projection — an authoritative
//! `SessionSnapshot` plus streaming progress items layered on top, with
//! streamed tool-call argument buffering.
//!
//! divergence: upstream keeps a `JsonValue` union for tool-call input
//! (parsed value or raw partial string); the port uses `serde_json::Value`,
//! where a partial argument prefix is `Value::String`.

use std::collections::{BTreeMap, BTreeSet};

use pillar_protocol::schemas::{
    AssistantContent, AssistantTranscriptItem, DeltaKind, NonTerminalItem, NonTerminalItemKind,
    SessionSnapshot, TerminalItem, TerminalItemKind, TranscriptItem, TranscriptProgress,
};
use serde_json::Value;

/// Remote transcript projection state (upstream `TranscriptState`).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptState {
    /// Authoritative snapshot; never mutated by progress application.
    pub snapshot: SessionSnapshot,
    /// Progress items layered over the snapshot, keyed by item id.
    pub progress_items: BTreeMap<String, TranscriptItem>,
    /// Insertion order of progress items (for items absent from the snapshot).
    pub progress_order: Vec<String>,
    /// Raw accumulated tool-call argument buffers, keyed `messageId:contentIndex`.
    pub tool_call_buffers: BTreeMap<String, String>,
}

impl TranscriptState {
    pub fn new(snapshot: &SessionSnapshot) -> Self {
        Self {
            snapshot: snapshot.clone(),
            progress_items: BTreeMap::new(),
            progress_order: Vec::new(),
            tool_call_buffers: BTreeMap::new(),
        }
    }
}

/// The stable id of any transcript item (upstream reading `item.id`).
pub fn transcript_item_id(item: &TranscriptItem) -> &str {
    match item {
        TranscriptItem::User { id, .. } => id,
        TranscriptItem::Assistant(assistant) => &assistant.id,
        TranscriptItem::Tool(tool) => &tool.id,
    }
}

/// Create projection state from an authoritative snapshot (upstream
/// `createTranscriptState`).
pub fn create_transcript_state(snapshot: &SessionSnapshot) -> TranscriptState {
    TranscriptState::new(snapshot)
}

/// Replace the authoritative snapshot (upstream `applyTranscriptSnapshot`):
/// stale snapshots for the same session are ignored, a different session id
/// always resets.
pub fn apply_transcript_snapshot(
    state: &TranscriptState,
    snapshot: &SessionSnapshot,
) -> TranscriptState {
    if state.snapshot.id == snapshot.id && snapshot.revision < state.snapshot.revision {
        return state.clone();
    }
    create_transcript_state(snapshot)
}

/// Apply one streamed progress event (upstream `applyTranscriptProgress`).
pub fn apply_transcript_progress(
    state: &TranscriptState,
    progress: &TranscriptProgress,
) -> TranscriptState {
    match progress {
        TranscriptProgress::ItemStarted { item } => set_progress_item(state, item),
        TranscriptProgress::ItemUpdated { item } => {
            set_progress_item(state, &non_terminal_to_item(item))
        }
        TranscriptProgress::ItemFinished { item } => {
            let prefix = format!("{}:", terminal_item_id(item));
            let mut tool_call_buffers = state.tool_call_buffers.clone();
            tool_call_buffers.retain(|key, _| !key.starts_with(&prefix));
            let mut next = state.clone();
            next.tool_call_buffers = tool_call_buffers;
            set_progress_item(&next, &terminal_to_item(item))
        }
        TranscriptProgress::AssistantDelta {
            message_id,
            content_index,
            kind,
            delta,
        } => apply_assistant_delta(state, message_id, *content_index, *kind, delta),
    }
}

/// Layer snapshot items with progress items, appending transient items and
/// accepted steering messages (upstream `selectTranscript`).
pub fn select_transcript(state: &TranscriptState) -> Vec<TranscriptItem> {
    let mut transcript: Vec<TranscriptItem> = state
        .snapshot
        .transcript
        .iter()
        .map(|item| {
            state
                .progress_items
                .get(transcript_item_id(item))
                .cloned()
                .unwrap_or_else(|| item.clone())
        })
        .collect();
    let mut ids: BTreeSet<String> = transcript
        .iter()
        .map(|item| transcript_item_id(item).to_string())
        .collect();

    for id in &state.progress_order {
        if ids.contains(id) {
            continue;
        }
        if let Some(item) = state.progress_items.get(id) {
            transcript.push(item.clone());
            ids.insert(id.clone());
        }
    }
    for item in &state.snapshot.queued_steer {
        let id = transcript_item_id(item);
        if ids.contains(id) {
            continue;
        }
        transcript.push(item.clone());
        ids.insert(id.to_string());
    }
    transcript
}

fn apply_assistant_delta(
    state: &TranscriptState,
    message_id: &str,
    content_index: u64,
    kind: DeltaKind,
    delta: &str,
) -> TranscriptState {
    let item = state.progress_items.get(message_id).cloned().or_else(|| {
        state
            .snapshot
            .transcript
            .iter()
            .find(|item| transcript_item_id(item) == message_id)
            .cloned()
    });
    let Some(TranscriptItem::Assistant(assistant)) = item else {
        return state.clone();
    };

    let mut tool_call_buffers = state.tool_call_buffers.clone();
    let mut content = Vec::with_capacity(assistant.content.len());
    for (index, part) in assistant.content.iter().enumerate() {
        if index as u64 != content_index {
            content.push(part.clone());
            continue;
        }
        match (kind, part) {
            (DeltaKind::Text, AssistantContent::Text { text }) => {
                content.push(AssistantContent::Text {
                    text: format!("{text}{delta}"),
                });
            }
            (DeltaKind::Thinking, AssistantContent::Thinking { thinking, redacted }) => {
                content.push(AssistantContent::Thinking {
                    thinking: format!("{thinking}{delta}"),
                    redacted: *redacted,
                });
            }
            (
                DeltaKind::ToolCall,
                AssistantContent::ToolCall {
                    tool_call_id,
                    tool_name,
                    input,
                },
            ) => {
                let key = format!("{message_id}:{content_index}");
                let existing =
                    tool_call_buffers
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| match input {
                            Value::String(text) => text.clone(),
                            _ => String::new(),
                        });
                let buffer = format!("{existing}{delta}");
                tool_call_buffers.insert(key, buffer.clone());
                content.push(AssistantContent::ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    input: parse_partial_tool_input(&buffer),
                });
            }
            _ => content.push(part.clone()),
        }
    }

    let updated = AssistantTranscriptItem {
        content,
        ..assistant
    };
    let mut next = state.clone();
    next.tool_call_buffers = tool_call_buffers;
    set_progress_item(&next, &TranscriptItem::Assistant(updated))
}

fn set_progress_item(state: &TranscriptState, item: &TranscriptItem) -> TranscriptState {
    let id = transcript_item_id(item).to_string();
    let mut progress_items = state.progress_items.clone();
    let mut progress_order = state.progress_order.clone();
    if !progress_items.contains_key(&id) {
        progress_order.push(id.clone());
    }
    progress_items.insert(id, item.clone());
    TranscriptState {
        snapshot: state.snapshot.clone(),
        progress_items,
        progress_order,
        tool_call_buffers: state.tool_call_buffers.clone(),
    }
}

fn non_terminal_to_item(item: &NonTerminalItem) -> TranscriptItem {
    match &item.item {
        NonTerminalItemKind::Assistant(assistant) => TranscriptItem::Assistant(assistant.clone()),
        NonTerminalItemKind::Tool(tool) => TranscriptItem::Tool(tool.clone()),
    }
}

fn terminal_to_item(item: &TerminalItem) -> TranscriptItem {
    match &item.item {
        TerminalItemKind::Assistant(assistant) => TranscriptItem::Assistant(assistant.clone()),
        TerminalItemKind::Tool(tool) => TranscriptItem::Tool(tool.clone()),
    }
}

fn terminal_item_id(item: &TerminalItem) -> &str {
    match &item.item {
        TerminalItemKind::Assistant(assistant) => &assistant.id,
        TerminalItemKind::Tool(tool) => &tool.id,
    }
}

/// Parse an accumulated tool-call argument buffer. Incomplete JSON keeps the
/// raw prefix (upstream `parsePartialToolInput`).
fn parse_partial_tool_input(value: &str) -> Value {
    serde_json::from_str::<Value>(value).unwrap_or_else(|_| Value::String(value.to_string()))
}
