//! Port of packages/coding-agent/src/core/session-manager.ts (pi v0.84.3):
//! append-only JSONL session trees with leaf pointers, compaction-aware
//! context building, and branch/session extraction.
//!
//! divergence: upstream stores ISO timestamp strings and serializes entries
//! with their TypeScript shapes; the port carries resolved millisecond
//! timestamps and serializes via serde (same field names, camelCase). The
//! streaming header scan and concurrent info loading are simplified to
//! plain synchronous reads; v1/v2 migrations (firstKeptEntryIndex ->
//! firstKeptEntryId, hookMessage -> custom role) are applied on load.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use pillar_ai::types::Usage;

use pillar_ai::types::Message;

use crate::core::messages::{CodingAgentMessage, CustomContent, CustomMessage};
use crate::core::session_entries::{
    BranchSummaryEntry, CompactionEntry, CustomEntry, CustomMessageEntry, LabelEntry,
    ModelChangeEntry, SessionEntry as Entry, SessionEntryBase, SessionInfoEntry,
    SessionMessageEntry, ThinkingLevelChangeEntry,
};

pub const CURRENT_SESSION_VERSION: u32 = 3;

/// Session file header (upstream `SessionHeader`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub r#type: String,
    #[serde(default)]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Options for creating a new session (upstream `NewSessionOptions`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// A session tree node (upstream `SessionTreeNode`).
#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub entry: Entry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<u64>,
}

/// Session context settings (upstream `SessionContext`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<CodingAgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
}

/// File-level entry union: header or session entry (upstream `FileEntry`).
#[derive(Debug, Clone)]
pub enum FileEntry {
    Header(SessionHeader),
    Entry(Entry),
}

impl FileEntry {
    fn to_json(&self) -> Value {
        match self {
            FileEntry::Header(header) => serde_json::to_value(header).unwrap_or(Value::Null),
            FileEntry::Entry(entry) => entry_to_json(entry),
        }
    }
}

// --- entry (de)serialization ----------------------------------------------------------

/// Serialize a session-tree node (upstream `SessionTreeNode`): the entry,
/// its children, and the resolved label.
pub fn tree_node_to_json(node: &SessionTreeNode) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("entry".to_string(), entry_to_json(&node.entry));
    obj.insert(
        "children".to_string(),
        Value::Array(node.children.iter().map(tree_node_to_json).collect()),
    );
    if let Some(label) = &node.label {
        obj.insert("label".to_string(), Value::String(label.clone()));
    }
    if let Some(timestamp) = node.label_timestamp {
        obj.insert("labelTimestamp".to_string(), serde_json::json!(timestamp));
    }
    Value::Object(obj)
}

/// Serialize a session entry to its JSONL shape (upstream JSON.stringify of
/// the TS shapes; camelCase field names).
pub fn entry_to_json(entry: &Entry) -> Value {
    let mut obj = serde_json::Map::new();
    let base = entry.base();
    obj.insert(
        "type".to_string(),
        Value::String(entry_type(entry).to_string()),
    );
    obj.insert("id".to_string(), Value::String(base.id.clone()));
    obj.insert(
        "parentId".to_string(),
        base.parent_id
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    // Upstream serializes ISO timestamps; the port emits ms numbers.
    obj.insert("timestamp".to_string(), serde_json::json!(base.timestamp));
    match entry {
        Entry::Message(m) => {
            obj.insert("message".to_string(), message_to_json(&m.message));
        }
        Entry::ThinkingLevelChange(e) => {
            obj.insert(
                "thinkingLevel".to_string(),
                Value::String(e.thinking_level.clone()),
            );
        }
        Entry::ModelChange(e) => {
            obj.insert("provider".to_string(), Value::String(e.provider.clone()));
            obj.insert("modelId".to_string(), Value::String(e.model_id.clone()));
        }
        Entry::Compaction(e) => {
            obj.insert("summary".to_string(), Value::String(e.summary.clone()));
            obj.insert(
                "firstKeptEntryId".to_string(),
                Value::String(e.first_kept_entry_id.clone()),
            );
            obj.insert(
                "tokensBefore".to_string(),
                serde_json::json!(e.tokens_before),
            );
            if let Some(details) = &e.details {
                obj.insert("details".to_string(), details.clone());
            }
            if let Some(usage) = &e.usage {
                obj.insert(
                    "usage".to_string(),
                    serde_json::to_value(usage).unwrap_or(Value::Null),
                );
            }
            if e.from_hook {
                obj.insert("fromHook".to_string(), Value::Bool(true));
            }
        }
        Entry::BranchSummary(e) => {
            obj.insert("fromId".to_string(), Value::String(e.from_id.clone()));
            obj.insert("summary".to_string(), Value::String(e.summary.clone()));
            if let Some(details) = &e.details {
                obj.insert("details".to_string(), details.clone());
            }
            if let Some(usage) = &e.usage {
                obj.insert(
                    "usage".to_string(),
                    serde_json::to_value(usage).unwrap_or(Value::Null),
                );
            }
            if e.from_hook {
                obj.insert("fromHook".to_string(), Value::Bool(true));
            }
        }
        Entry::Custom(e) => {
            obj.insert(
                "customType".to_string(),
                Value::String(e.custom_type.clone()),
            );
            if let Some(data) = &e.data {
                obj.insert("data".to_string(), data.clone());
            }
        }
        Entry::Label(e) => {
            obj.insert("targetId".to_string(), Value::String(e.target_id.clone()));
            obj.insert(
                "label".to_string(),
                e.label.clone().map(Value::String).unwrap_or(Value::Null),
            );
        }
        Entry::SessionInfo(e) => {
            if let Some(name) = &e.name {
                obj.insert("name".to_string(), Value::String(name.clone()));
            }
        }
        Entry::CustomMessage(e) => {
            obj.insert(
                "customType".to_string(),
                Value::String(e.custom_type.clone()),
            );
            obj.insert("content".to_string(), custom_content_to_json(&e.content));
            if let Some(details) = &e.details {
                obj.insert("details".to_string(), details.clone());
            }
            obj.insert("display".to_string(), Value::Bool(e.display));
        }
    }
    Value::Object(obj)
}

fn entry_type(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message(_) => "message",
        Entry::ThinkingLevelChange(_) => "thinking_level_change",
        Entry::ModelChange(_) => "model_change",
        Entry::Compaction(_) => "compaction",
        Entry::BranchSummary(_) => "branch_summary",
        Entry::Custom(_) => "custom",
        Entry::Label(_) => "label",
        Entry::SessionInfo(_) => "session_info",
        Entry::CustomMessage(_) => "custom_message",
    }
}

fn message_to_json(message: &CodingAgentMessage) -> Value {
    // The port reuses the pillar-ai Message wire shape for base roles.
    match message {
        CodingAgentMessage::Base(message) => serde_json::to_value(message).unwrap_or(Value::Null),
        CodingAgentMessage::BashExecution(bash) => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "role".to_string(),
                Value::String("bashExecution".to_string()),
            );
            obj.insert("command".to_string(), Value::String(bash.command.clone()));
            obj.insert("output".to_string(), Value::String(bash.output.clone()));
            obj.insert(
                "exitCode".to_string(),
                bash.exit_code
                    .map(|c| serde_json::json!(c))
                    .unwrap_or(Value::Null),
            );
            obj.insert("cancelled".to_string(), Value::Bool(bash.cancelled));
            obj.insert("truncated".to_string(), Value::Bool(bash.truncated));
            if let Some(path) = &bash.full_output_path {
                obj.insert("fullOutputPath".to_string(), Value::String(path.clone()));
            }
            obj.insert("timestamp".to_string(), serde_json::json!(bash.timestamp));
            if bash.exclude_from_context {
                obj.insert("excludeFromContext".to_string(), Value::Bool(true));
            }
            Value::Object(obj)
        }
        CodingAgentMessage::Custom(custom) => {
            let mut obj = serde_json::Map::new();
            obj.insert("role".to_string(), Value::String("custom".to_string()));
            obj.insert(
                "customType".to_string(),
                Value::String(custom.custom_type.clone()),
            );
            obj.insert(
                "content".to_string(),
                custom_content_to_json(&custom.content),
            );
            obj.insert("display".to_string(), Value::Bool(custom.display));
            if let Some(details) = &custom.details {
                obj.insert("details".to_string(), details.clone());
            }
            obj.insert("timestamp".to_string(), serde_json::json!(custom.timestamp));
            Value::Object(obj)
        }
        CodingAgentMessage::BranchSummary(summary) => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "role".to_string(),
                Value::String("branchSummary".to_string()),
            );
            obj.insert(
                "summary".to_string(),
                Value::String(summary.summary.clone()),
            );
            obj.insert("fromId".to_string(), Value::String(summary.from_id.clone()));
            obj.insert(
                "timestamp".to_string(),
                serde_json::json!(summary.timestamp),
            );
            Value::Object(obj)
        }
        CodingAgentMessage::CompactionSummary(summary) => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "role".to_string(),
                Value::String("compactionSummary".to_string()),
            );
            obj.insert(
                "summary".to_string(),
                Value::String(summary.summary.clone()),
            );
            obj.insert(
                "tokensBefore".to_string(),
                serde_json::json!(summary.tokens_before),
            );
            obj.insert(
                "timestamp".to_string(),
                serde_json::json!(summary.timestamp),
            );
            Value::Object(obj)
        }
    }
}

fn custom_content_to_json(content: &[CustomContent]) -> Value {
    Value::Array(
        content
            .iter()
            .map(|content| match content {
                CustomContent::Text(text) => serde_json::json!({"type": "text", "text": text}),
                CustomContent::Image { data, mime_type } => {
                    serde_json::json!({"type": "image", "data": data, "mimeType": mime_type})
                }
            })
            .collect(),
    )
}

fn custom_content_from_json(value: &Value) -> Vec<CustomContent> {
    match value {
        Value::String(text) => vec![CustomContent::Text(text.clone())],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                let kind = block.get("type")?.as_str()?;
                match kind {
                    "text" => Some(CustomContent::Text(
                        block.get("text")?.as_str()?.to_string(),
                    )),
                    "image" => Some(CustomContent::Image {
                        data: block.get("data")?.as_str()?.to_string(),
                        mime_type: block.get("mimeType")?.as_str()?.to_string(),
                    }),
                    _ => None,
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn usage_from_json(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    serde_json::from_value(value.clone()).ok()
}

/// Parse a single JSONL line into a file entry; None for malformed lines
/// (upstream `parseSessionEntryLine`).
pub fn parse_session_entry_line(line: &str) -> Option<FileEntry> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let kind = value.get("type")?.as_str()?.to_string();
    if kind == "session" {
        let header: SessionHeader = serde_json::from_value(value).ok()?;
        return Some(FileEntry::Header(header));
    }
    let id = value.get("id")?.as_str()?.to_string();
    let parent_id = match value.get("parentId") {
        Some(Value::String(parent)) => Some(parent.clone()),
        _ => None,
    };
    let timestamp = value
        .get("timestamp")
        .and_then(|t| t.as_u64())
        .or_else(|| {
            // Tolerate ISO strings from upstream files.
            value
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(parse_iso_timestamp)
        })
        .unwrap_or(0);
    let base = SessionEntryBase {
        id,
        parent_id,
        timestamp,
    };
    let entry = match kind.as_str() {
        "message" => Entry::Message(SessionMessageEntry {
            base,
            message: message_from_json(value.get("message")?)?,
        }),
        "thinking_level_change" => Entry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            base,
            thinking_level: value
                .get("thinkingLevel")
                .and_then(|v| v.as_str())
                .unwrap_or("off")
                .to_string(),
        }),
        "model_change" => Entry::ModelChange(ModelChangeEntry {
            base,
            provider: value
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            model_id: value
                .get("modelId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
        "compaction" => Entry::Compaction(CompactionEntry {
            base,
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            first_kept_entry_id: value
                .get("firstKeptEntryId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            tokens_before: value
                .get("tokensBefore")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            details: value.get("details").cloned(),
            usage: usage_from_json(value.get("usage")),
            from_hook: value
                .get("fromHook")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        "branch_summary" => Entry::BranchSummary(BranchSummaryEntry {
            base,
            from_id: value
                .get("fromId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            details: value.get("details").cloned(),
            usage: usage_from_json(value.get("usage")),
            from_hook: value
                .get("fromHook")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        "custom" => Entry::Custom(CustomEntry {
            base,
            custom_type: value
                .get("customType")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            data: value.get("data").cloned(),
        }),
        "label" => Entry::Label(LabelEntry {
            base,
            target_id: value
                .get("targetId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            label: value
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        "session_info" => Entry::SessionInfo(SessionInfoEntry {
            base,
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        "custom_message" => Entry::CustomMessage(CustomMessageEntry {
            base,
            custom_type: value
                .get("customType")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            content: value
                .get("content")
                .map(custom_content_from_json)
                .unwrap_or_default(),
            details: value.get("details").cloned(),
            display: value
                .get("display")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }),
        _ => return None,
    };
    Some(FileEntry::Entry(entry))
}

fn message_from_json(value: &Value) -> Option<CodingAgentMessage> {
    let role = value.get("role")?.as_str()?.to_string();
    match role.as_str() {
        "user" | "assistant" | "toolResult" => {
            let message: Message = serde_json::from_value(value.clone()).ok()?;
            Some(CodingAgentMessage::Base(message))
        }
        "bashExecution" => Some(CodingAgentMessage::BashExecution(
            crate::core::messages::BashExecutionMessage {
                command: value
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                output: value
                    .get("output")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                exit_code: value
                    .get("exitCode")
                    .and_then(|v| v.as_i64())
                    .map(|c| c as i32),
                cancelled: value
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                truncated: value
                    .get("truncated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                full_output_path: value
                    .get("fullOutputPath")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                timestamp: value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0),
                exclude_from_context: value
                    .get("excludeFromContext")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        )),
        "custom" | "hookMessage" => Some(CodingAgentMessage::Custom(CustomMessage {
            custom_type: value
                .get("customType")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            content: value
                .get("content")
                .map(custom_content_from_json)
                .unwrap_or_default(),
            display: value
                .get("display")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            details: value.get("details").cloned(),
            timestamp: value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0),
        })),
        "branchSummary" => Some(CodingAgentMessage::BranchSummary(
            crate::core::messages::BranchSummaryMessage {
                summary: value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                from_id: value
                    .get("fromId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                timestamp: value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0),
            },
        )),
        "compactionSummary" => Some(CodingAgentMessage::CompactionSummary(
            crate::core::messages::CompactionSummaryMessage {
                summary: value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tokens_before: value
                    .get("tokensBefore")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                timestamp: value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0),
            },
        )),
        _ => None,
    }
}

/// Parse an ISO-8601 timestamp into Unix milliseconds (a minimal parser for
/// `YYYY-MM-DDTHH:MM:SS(.sss)Z` shapes from upstream session files).
pub fn parse_iso_timestamp(input: &str) -> Option<u64> {
    let input = input.trim();
    let bytes = input.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = input.get(0..4)?.parse().ok()?;
    let month: i64 = input.get(5..7)?.parse().ok()?;
    let day: i64 = input.get(8..10)?.parse().ok()?;
    let hour: i64 = input.get(11..13)?.parse().ok()?;
    let minute: i64 = input.get(14..16)?.parse().ok()?;
    let second: i64 = input.get(17..19)?.parse().ok()?;
    let millis: i64 = if bytes.len() >= 23 && bytes[19] == b'.' {
        input.get(20..23)?.parse().ok()?
    } else {
        0
    };
    // Days since the Unix epoch via the civil-from-days algorithm.
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000 + millis) as u64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ============================================================================
// Migrations
// ============================================================================

/// Run all necessary migrations to bring entries to the current version
/// (upstream `migrateToCurrentVersion`): v1 -> v2 adds id/parentId and
/// converts firstKeptEntryIndex to firstKeptEntryId; v2 -> v3 renames the
/// hookMessage role to custom. Returns true if any migration was applied.
pub fn migrate_session_entries(entries: &mut [FileEntry]) -> bool {
    let version = entries
        .iter()
        .find_map(|entry| match entry {
            FileEntry::Header(header) => header.version,
            _ => None,
        })
        .unwrap_or(1);

    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    if version < 3 {
        migrate_v2_to_v3(entries);
    }
    true
}

fn migrate_v1_to_v2(entries: &mut [FileEntry]) {
    let mut ids = std::collections::BTreeSet::new();
    let mut prev_id: Option<String> = None;
    let mut first_kept_index: Option<(usize, usize)> = None;

    for (index, entry) in entries.iter_mut().enumerate() {
        match entry {
            FileEntry::Header(header) => {
                header.version = Some(2);
                continue;
            }
            FileEntry::Entry(e) => {
                let new_id = generate_id_with(&ids);
                ids.insert(new_id.clone());
                e.base_mut().id = new_id.clone();
                e.base_mut().parent_id = prev_id.clone();
                prev_id = Some(new_id);
                if let Entry::Compaction(compaction) = e {
                    if let Some(value) = &compaction.details {
                        if let Some(first_kept) = value.get("firstKeptEntryIndex") {
                            if let Some(i) = first_kept.as_u64() {
                                first_kept_index = Some((index, i as usize));
                            }
                        }
                    }
                }
            }
        }
    }

    // Convert firstKeptEntryIndex to firstKeptEntryId.
    if let Some((entry_index, kept_index)) = first_kept_index {
        if let Some(FileEntry::Entry(target)) = entries.get(kept_index) {
            let target_id = target.id().to_string();
            if let Some(FileEntry::Entry(Entry::Compaction(compaction))) =
                entries.get_mut(entry_index)
            {
                compaction.first_kept_entry_id = target_id;
            }
        }
    }
}

fn migrate_v2_to_v3(entries: &mut [FileEntry]) {
    for entry in entries.iter_mut() {
        if let FileEntry::Header(header) = entry {
            header.version = Some(3);
            continue;
        }
        if let FileEntry::Entry(Entry::Message(message_entry)) = entry {
            if let CodingAgentMessage::Custom(custom) = &mut message_entry.message {
                // v2 hook messages became custom messages (role rename only).
                let _ = custom;
            }
        }
    }
}

/// Generate a unique short ID (8 hex chars, collision-checked) against the
/// given id set (upstream `generateId`).
pub fn generate_id_with(existing: &std::collections::BTreeSet<String>) -> String {
    for _ in 0..100 {
        let id = pillar_ai::uuid::uuidv7()[..8].to_string();
        if !existing.contains(&id) {
            return id;
        }
    }
    pillar_ai::uuid::uuidv7()
}

// ============================================================================
// Context building (pure functions over entries)
// ============================================================================

fn build_entry_index(entries: &[Entry]) -> BTreeMap<String, &Entry> {
    entries.iter().map(|e| (e.id().to_string(), e)).collect()
}

/// Build the path from root to `leaf_id` (or the last entry when omitted)
/// (upstream `buildSessionPath`).
pub fn build_session_path<'a>(
    entries: &'a [Entry],
    leaf_id: Option<&str>,
    by_id: &BTreeMap<String, &'a Entry>,
) -> Vec<&'a Entry> {
    if leaf_id == Some("") {
        return Vec::new();
    }
    let leaf = leaf_id
        .and_then(|id| by_id.get(id).copied())
        .or_else(|| entries.last());
    let Some(leaf) = leaf else {
        return Vec::new();
    };

    let mut path: Vec<&Entry> = Vec::new();
    let mut current: Option<&Entry> = Some(leaf);
    while let Some(entry) = current {
        path.push(entry);
        current = entry
            .parent_id()
            .and_then(|parent| by_id.get(parent).copied());
    }
    path.reverse();
    path
}

fn get_session_context_settings(path: &[&Entry]) -> (String, Option<(String, String)>) {
    let mut thinking_level = "off".to_string();
    let mut model: Option<(String, String)> = None;

    for entry in path {
        match entry {
            Entry::ThinkingLevelChange(change) => thinking_level = change.thinking_level.clone(),
            Entry::ModelChange(change) => {
                model = Some((change.provider.clone(), change.model_id.clone()));
            }
            Entry::Message(message_entry) => {
                if let CodingAgentMessage::Base(Message::Assistant(assistant)) =
                    &message_entry.message
                {
                    model = Some((assistant.provider.clone(), assistant.model.clone()));
                }
            }
            _ => {}
        }
    }

    (thinking_level, model)
}

/// Project one selected session entry into LLM/runtime messages (upstream
/// `sessionEntryToContextMessages`). Plain custom entries are display/state
/// entries and do not participate in context.
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<CodingAgentMessage> {
    match entry {
        Entry::Message(message_entry) => vec![message_entry.message.clone()],
        Entry::CustomMessage(custom) => vec![CodingAgentMessage::Custom(CustomMessage {
            custom_type: custom.custom_type.clone(),
            content: custom.content.clone(),
            display: custom.display,
            details: custom.details.clone(),
            timestamp: custom.base.timestamp,
        })],
        Entry::BranchSummary(branch) if !branch.summary.is_empty() => {
            vec![CodingAgentMessage::BranchSummary(
                crate::core::messages::BranchSummaryMessage {
                    summary: branch.summary.clone(),
                    from_id: branch.from_id.clone(),
                    timestamp: branch.base.timestamp,
                },
            )]
        }
        Entry::Compaction(compaction) => vec![CodingAgentMessage::CompactionSummary(
            crate::core::messages::CompactionSummaryMessage {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
                timestamp: compaction.base.timestamp,
            },
        )],
        _ => Vec::new(),
    }
}

/// Build the active, compaction-aware session entry list (upstream
/// `buildContextEntries`): follows the leaf path; the latest compaction is
/// represented by the compaction entry itself followed by the kept entries
/// from firstKeptEntryId onward. Older summarized entries are omitted.
pub fn build_context_entries<'a>(
    entries: &'a [Entry],
    leaf_id: Option<&str>,
    by_id: &BTreeMap<String, &'a Entry>,
) -> Vec<&'a Entry> {
    let path = build_session_path(entries, leaf_id, by_id);
    let compaction = path.iter().rev().find_map(|entry| match entry {
        Entry::Compaction(_) => Some(&**entry),
        _ => None,
    });

    let Some(compaction) = compaction else {
        return path;
    };

    let Some(compaction_idx) = path.iter().position(|entry| entry.id() == compaction.id()) else {
        return path;
    };

    let mut context_entries: Vec<&Entry> = vec![compaction];
    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if entry.id() == compaction_first_kept(compaction) {
            found_first_kept = true;
        }
        if found_first_kept {
            context_entries.push(entry);
        }
    }
    context_entries.extend(path[compaction_idx + 1..].iter().copied());
    context_entries
}

fn compaction_first_kept(entry: &Entry) -> &str {
    match entry {
        Entry::Compaction(compaction) => &compaction.first_kept_entry_id,
        _ => "",
    }
}

/// Build the session context from entries using tree traversal (upstream
/// `buildSessionContext`).
pub fn build_session_context(entries: &[Entry], leaf_id: Option<&str>) -> SessionContext {
    let by_id = build_entry_index(entries);
    let path = build_session_path(entries, leaf_id, &by_id);
    let (thinking_level, model) = get_session_context_settings(&path);
    let messages = build_context_entries(entries, leaf_id, &by_id)
        .iter()
        .flat_map(|entry| session_entry_to_context_messages(entry))
        .collect();
    SessionContext {
        messages,
        thinking_level,
        model,
    }
}

/// The latest compaction entry in a list, if any (upstream
/// `getLatestCompactionEntry`).
pub fn get_latest_compaction_entry(entries: &[Entry]) -> Option<&CompactionEntry> {
    entries.iter().rev().find_map(|entry| match entry {
        Entry::Compaction(compaction) => Some(compaction),
        _ => None,
    })
}

// ============================================================================
// SessionManager
// ============================================================================

/// Manages conversation sessions as append-only trees stored in JSONL files
/// (upstream `SessionManager`). Each entry has an id and parentId forming a
/// tree; the leaf pointer tracks the current position. Appending creates a
/// child of the current leaf; branching moves the leaf to an earlier entry.
pub struct SessionManager {
    session_id: String,
    session_file: Option<PathBuf>,
    session_dir: PathBuf,
    cwd: String,
    persist: bool,
    flushed: bool,
    file_entries: Vec<FileEntry>,
    by_id: BTreeMap<String, usize>,
    labels_by_id: BTreeMap<String, String>,
    label_timestamps_by_id: BTreeMap<String, u64>,
    leaf_id: Option<String>,
    now_ms: fn() -> u64,
}

/// Result of loading a session file (upstream `loadEntriesFromFile`).
pub fn load_entries_from_file(file_path: &Path) -> Vec<FileEntry> {
    if !file_path.exists() {
        return Vec::new();
    }
    let Ok(content) = fs::read_to_string(file_path) else {
        return Vec::new();
    };
    let entries: Vec<FileEntry> = content
        .lines()
        .filter_map(parse_session_entry_line)
        .collect();

    // Validate the session header before repairing the file.
    match entries.first() {
        Some(FileEntry::Header(header)) if !header.id.is_empty() => entries,
        _ => Vec::new(),
    }
}

impl SessionManager {
    fn new(
        cwd: &str,
        session_dir: &Path,
        session_file: Option<&Path>,
        persist: bool,
        new_session_options: Option<&NewSessionOptions>,
    ) -> Result<Self, String> {
        let mut manager = Self {
            session_id: String::new(),
            session_file: None,
            session_dir: session_dir.to_path_buf(),
            cwd: cwd.to_string(),
            persist,
            flushed: false,
            file_entries: Vec::new(),
            by_id: BTreeMap::new(),
            labels_by_id: BTreeMap::new(),
            label_timestamps_by_id: BTreeMap::new(),
            leaf_id: None,
            now_ms: pillar_ai::models::now_ms,
        };
        if persist && !session_dir.exists() {
            fs::create_dir_all(session_dir)
                .map_err(|e| format!("Failed to create session dir: {e}"))?;
        }

        if let Some(session_file) = session_file {
            manager.set_session_file(session_file)?;
        } else {
            manager.new_session(new_session_options);
        }
        Ok(manager)
    }

    /// Switch to a different session file (used for resume and branching).
    pub fn set_session_file(&mut self, session_file: &Path) -> Result<(), String> {
        self.set_session_file_with_preloaded(session_file, None)
    }

    fn set_session_file_with_preloaded(
        &mut self,
        session_file: &Path,
        preloaded: Option<Vec<FileEntry>>,
    ) -> Result<(), String> {
        self.session_file = Some(session_file.to_path_buf());
        if session_file.exists() {
            self.file_entries = match preloaded {
                Some(entries) => entries,
                None => load_entries_from_file(session_file),
            };

            if self.file_entries.is_empty() {
                // Empty file: initialize with a valid session header. Non-empty
                // files that did not parse fail without modification.
                let size = fs::metadata(session_file).map(|m| m.len()).unwrap_or(0);
                if size > 0 {
                    return Err(format!(
                        "Session file is not a valid pi session: {}",
                        session_file.display()
                    ));
                }
                self.new_session(None);
                self.session_file = Some(session_file.to_path_buf());
                self.rewrite_file()?;
                self.flushed = true;
                return Ok(());
            }

            self.session_id = self
                .file_entries
                .iter()
                .find_map(|entry| match entry {
                    FileEntry::Header(header) => Some(header.id.clone()),
                    _ => None,
                })
                .unwrap_or_else(pillar_ai::uuid::uuidv7);

            if migrate_session_entries(&mut self.file_entries) {
                self.rewrite_file()?;
            }

            self.build_index();
            self.flushed = true;
        } else {
            let explicit_path = session_file.to_path_buf();
            self.new_session(None);
            self.session_file = Some(explicit_path);
        }
        Ok(())
    }

    /// Start a fresh session; returns the new session file path when
    /// persisting (upstream `newSession`).
    pub fn new_session(&mut self, options: Option<&NewSessionOptions>) -> Option<PathBuf> {
        if let Some(id) = options.and_then(|o| o.id.as_ref()) {
            assert_valid_session_id(id);
        }
        self.session_id = options
            .and_then(|o| o.id.clone())
            .unwrap_or_else(pillar_ai::uuid::uuidv7);
        let timestamp = iso_now();
        let header = SessionHeader {
            r#type: "session".to_string(),
            version: Some(CURRENT_SESSION_VERSION),
            id: self.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.clone(),
            parent_session: options.and_then(|o| o.parent_session.clone()),
        };
        self.file_entries = vec![FileEntry::Header(header)];
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;

        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            let file = self
                .session_dir
                .join(format!("{file_timestamp}_{}.jsonl", self.session_id));
            self.session_file = Some(file.clone());
            return Some(file);
        }
        None
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for (index, entry) in self.file_entries.iter().enumerate() {
            let entry = match entry {
                FileEntry::Entry(entry) => entry,
                FileEntry::Header(_) => continue,
            };
            self.by_id.insert(entry.id().to_string(), index);
            self.leaf_id = Some(entry.id().to_string());
            if let Entry::Label(label) = entry {
                if let Some(text) = &label.label {
                    self.labels_by_id
                        .insert(label.target_id.clone(), text.clone());
                    self.label_timestamps_by_id
                        .insert(label.target_id.clone(), label.base.timestamp);
                } else {
                    self.labels_by_id.remove(&label.target_id);
                    self.label_timestamps_by_id.remove(&label.target_id);
                }
            }
        }
    }

    fn rewrite_file(&self) -> Result<(), String> {
        if !self.persist {
            return Ok(());
        }
        let Some(session_file) = &self.session_file else {
            return Ok(());
        };
        let mut content = String::new();
        for entry in &self.file_entries {
            content.push_str(&serde_json::to_string(&entry.to_json()).unwrap_or_default());
            content.push('\n');
        }
        fs::write(session_file, content).map_err(|e| format!("Failed to write session: {e}"))
    }

    fn persist_entry(&self, entry: &Entry) -> Result<(), String> {
        if !self.persist {
            return Ok(());
        }
        let Some(session_file) = &self.session_file else {
            return Ok(());
        };
        let has_assistant = self.file_entries.iter().any(|e| {
            matches!(
                e,
                FileEntry::Entry(Entry::Message(message_entry))
                    if matches!(
                        &message_entry.message,
                        CodingAgentMessage::Base(Message::Assistant(_))
                    )
            )
        });
        let serialized = format!(
            "{}\n",
            serde_json::to_string(&FileEntry::Entry(entry.clone()).to_json()).unwrap_or_default()
        );
        if !has_assistant {
            if self.flushed {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(session_file)
                    .and_then(|mut f| f.write_all(serialized.as_bytes()))
                    .map_err(|e| format!("Failed to append session entry: {e}"))?;
            }
            // Otherwise defer: everything gets written when the first
            // assistant message arrives.
            return Ok(());
        }
        if !self.flushed {
            let mut content = String::new();
            for e in &self.file_entries {
                content.push_str(&serde_json::to_string(&e.to_json()).unwrap_or_default());
                content.push('\n');
            }
            fs::write(session_file, content)
                .map_err(|e| format!("Failed to write session: {e}"))?;
        } else {
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(session_file)
                .and_then(|mut f| f.write_all(serialized.as_bytes()))
                .map_err(|e| format!("Failed to append session entry: {e}"))?;
        }
        Ok(())
    }

    fn append_entry(&mut self, entry: Entry) -> Result<String, String> {
        let id = entry.id().to_string();
        self.file_entries.push(FileEntry::Entry(entry));
        let index = self.file_entries.len() - 1;
        self.by_id.insert(id.clone(), index);
        self.leaf_id = Some(id.clone());
        self.persist_entry(self.file_entries[index].entry_clone())?;
        Ok(id)
    }

    fn next_id(&self) -> String {
        let existing: std::collections::BTreeSet<String> = self.by_id.keys().cloned().collect();
        generate_id_with(&existing)
    }

    /// Append a message as a child of the current leaf and advance the leaf
    /// (upstream `appendMessage`).
    pub fn append_message(&mut self, message: CodingAgentMessage) -> Result<String, String> {
        let entry = Entry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            message,
        });
        self.append_entry(entry)
    }

    /// Append a thinking-level change (upstream `appendThinkingLevelChange`).
    pub fn append_thinking_level_change(&mut self, thinking_level: &str) -> Result<String, String> {
        let entry = Entry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            thinking_level: thinking_level.to_string(),
        });
        self.append_entry(entry)
    }

    /// Append a model change (upstream `appendModelChange`).
    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, String> {
        let entry = Entry::ModelChange(ModelChangeEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        });
        self.append_entry(entry)
    }

    /// Append a compaction checkpoint (upstream `appendCompaction`).
    #[allow(clippy::too_many_arguments)]
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: bool,
        usage: Option<Usage>,
    ) -> Result<String, String> {
        let entry = Entry::Compaction(CompactionEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            summary: summary.to_string(),
            first_kept_entry_id: first_kept_entry_id.to_string(),
            tokens_before,
            details,
            usage,
            from_hook,
        });
        self.append_entry(entry)
    }

    /// Append an extension custom entry (upstream `appendCustomEntry`).
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String, String> {
        let entry = Entry::Custom(CustomEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            custom_type: custom_type.to_string(),
            data,
        });
        self.append_entry(entry)
    }

    /// Append a session info entry, e.g. a display name (upstream
    /// `appendSessionInfo`). Newlines are collapsed to spaces.
    pub fn append_session_info(&mut self, name: &str) -> Result<String, String> {
        let sanitized: String = name.replace(['\r', '\n'], " ").trim().to_string();
        let entry = Entry::SessionInfo(SessionInfoEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            name: Some(sanitized),
        });
        self.append_entry(entry)
    }

    /// Append a custom message entry participating in LLM context (upstream
    /// `appendCustomMessageEntry`).
    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: Vec<CustomContent>,
        display: bool,
        details: Option<Value>,
    ) -> Result<String, String> {
        let entry = Entry::CustomMessage(CustomMessageEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            custom_type: custom_type.to_string(),
            content,
            details,
            display,
        });
        self.append_entry(entry)
    }

    // --- tree traversal -----------------------------------------------------------

    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn get_leaf_entry(&self) -> Option<&Entry> {
        let index = self.leaf_id.as_ref().and_then(|id| self.by_id.get(id))?;
        self.file_entries.get(*index).and_then(|e| match e {
            FileEntry::Entry(entry) => Some(entry),
            FileEntry::Header(_) => None,
        })
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        let index = self.by_id.get(id)?;
        self.file_entries.get(*index).and_then(|e| match e {
            FileEntry::Entry(entry) => Some(entry),
            FileEntry::Header(_) => None,
        })
    }

    /// All direct children of an entry (upstream `getChildren`).
    pub fn get_children(&self, parent_id: &str) -> Vec<&Entry> {
        self.get_entries()
            .into_iter()
            .filter(|entry| entry.parent_id() == Some(parent_id))
            .collect()
    }

    /// The resolved label for an entry, if any (upstream `getLabel`).
    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels_by_id.get(id).map(String::as_str)
    }

    /// Set or clear a label on an entry (upstream `appendLabelChange`).
    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> Result<String, String> {
        if !self.by_id.contains_key(target_id) {
            return Err(format!("Entry {target_id} not found"));
        }
        let entry = Entry::Label(LabelEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: (self.now_ms)(),
            },
            target_id: target_id.to_string(),
            label: label.map(str::to_string),
        });
        let id = self.append_entry(entry)?;
        self.build_index();
        Ok(id)
    }

    /// Walk from an entry to the root, returning all entries in path order
    /// (upstream `getBranch`).
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<&Entry> {
        let mut path: Vec<&Entry> = Vec::new();
        let start_id = from_id.map(str::to_string).or_else(|| self.leaf_id.clone());
        let mut current = start_id.and_then(|id| self.get_entry(&id));
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|parent| self.get_entry(parent));
        }
        path.reverse();
        path
    }

    /// The compaction-aware entry list for context/rendering (upstream
    /// `buildContextEntries` method).
    pub fn context_entries(&self) -> Vec<Entry> {
        let entries = self.get_entries_owned();
        let by_id = build_entry_index(&entries);
        build_context_entries(&entries, self.leaf_id.as_deref(), &by_id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// The resolved message list for the LLM (upstream `buildSessionContext`
    /// method).
    pub fn session_context(&self) -> SessionContext {
        let entries = self.get_entries_owned();
        build_session_context(&entries, self.leaf_id.as_deref())
    }

    pub fn get_header(&self) -> Option<&SessionHeader> {
        self.file_entries.iter().find_map(|entry| match entry {
            FileEntry::Header(header) => Some(header),
            _ => None,
        })
    }

    pub fn get_entries(&self) -> Vec<&Entry> {
        self.file_entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Entry(entry) => Some(entry),
                FileEntry::Header(_) => None,
            })
            .collect()
    }

    pub fn get_entries_owned(&self) -> Vec<Entry> {
        self.get_entries().into_iter().cloned().collect()
    }

    /// The session as a tree structure (upstream `getTree`): roots are
    /// entries with a null/self parent or a broken parent chain; children
    /// are sorted oldest-first.
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        // Build child-id lists keyed by parent id (no mutation issues).
        let entries = self.get_entries();
        let mut children_by_parent: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
        let mut root_entries: Vec<&Entry> = Vec::new();
        for entry in &entries {
            match entry.parent_id() {
                Some(parent) if parent != entry.id() => children_by_parent
                    .entry(parent.to_string())
                    .or_default()
                    .push(entry),
                _ => root_entries.push(entry),
            }
        }
        fn build_node(
            entry: &Entry,
            labels_by_id: &BTreeMap<String, String>,
            label_timestamps_by_id: &BTreeMap<String, u64>,
            children_by_parent: &BTreeMap<String, Vec<&Entry>>,
        ) -> SessionTreeNode {
            let id = entry.id();
            let children = children_by_parent
                .get(id)
                .map(|kids| {
                    kids.iter()
                        .map(|kid| {
                            build_node(
                                kid,
                                labels_by_id,
                                label_timestamps_by_id,
                                children_by_parent,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            SessionTreeNode {
                entry: entry.clone(),
                children,
                label: labels_by_id.get(id).cloned(),
                label_timestamp: label_timestamps_by_id.get(id).copied(),
            }
        }
        let mut roots: Vec<SessionTreeNode> = root_entries
            .iter()
            .map(|entry| {
                build_node(
                    entry,
                    &self.labels_by_id,
                    &self.label_timestamps_by_id,
                    &children_by_parent,
                )
            })
            .collect();
        sort_tree_children(&mut roots);
        roots
    }

    // --- branching -------------------------------------------------------------

    /// Start a new branch from an earlier entry by moving the leaf pointer
    /// (upstream `branch`). Existing entries are not modified or deleted.
    pub fn branch(&mut self, branch_from_id: &str) -> Result<(), String> {
        if !self.by_id.contains_key(branch_from_id) {
            return Err(format!("Entry {branch_from_id} not found"));
        }
        self.leaf_id = Some(branch_from_id.to_string());
        Ok(())
    }

    /// Reset the leaf pointer (upstream `resetLeaf`).
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// Start a new branch with a summary of the abandoned path (upstream
    /// `branchWithSummary`).
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: bool,
        usage: Option<Usage>,
    ) -> Result<String, String> {
        if let Some(branch_from_id) = branch_from_id {
            if !self.by_id.contains_key(branch_from_id) {
                return Err(format!("Entry {branch_from_id} not found"));
            }
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".to_string());
        self.leaf_id = branch_from_id.map(str::to_string);
        let entry = Entry::BranchSummary(BranchSummaryEntry {
            base: SessionEntryBase {
                id: self.next_id(),
                parent_id: branch_from_id.map(str::to_string),
                timestamp: (self.now_ms)(),
            },
            from_id,
            summary: summary.to_string(),
            details,
            usage,
            from_hook,
        });
        let id = entry.id().to_string();
        self.file_entries.push(FileEntry::Entry(entry));
        let index = self.file_entries.len() - 1;
        self.by_id.insert(id.clone(), index);
        self.leaf_id = Some(id.clone());
        self.persist_entry(self.file_entries[index].entry_clone())?;
        Ok(id)
    }

    // --- constructors ------------------------------------------------------------

    /// Create a new session in `sessionDir` (or the default dir for `cwd`)
    /// (upstream `SessionManager.create`).
    pub fn create(
        cwd: &str,
        session_dir: Option<&Path>,
        options: Option<&NewSessionOptions>,
    ) -> Result<Self, String> {
        let dir = session_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| default_session_dir_path(cwd));
        Self::new(cwd, &dir, None, true, options)
    }

    /// Open a specific session file (upstream `SessionManager.open`).
    pub fn open(
        path: &Path,
        session_dir: Option<&Path>,
        cwd_override: Option<&str>,
    ) -> Result<Self, String> {
        let mut header_cwd: Option<String> = None;
        if cwd_override.is_none() && path.exists() {
            if let Some(FileEntry::Header(header)) = load_entries_from_file(path).first().cloned() {
                header_cwd = Some(header.cwd);
            }
        }
        let cwd = cwd_override
            .map(str::to_string)
            .or(header_cwd)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let dir = session_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| {
            path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
        });
        Self::new(&cwd, &dir, Some(path), true, None)
    }

    /// Create an in-memory session without persistence (upstream
    /// `SessionManager.inMemory`).
    pub fn in_memory(cwd: &str, options: Option<&NewSessionOptions>) -> Result<Self, String> {
        Self::new(cwd, Path::new(""), None, false, options)
    }

    // --- accessors ----------------------------------------------------------------

    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    /// The current session name from the latest session_info entry (upstream
    /// `getSessionName`); empty names explicitly clear the title.
    pub fn session_name(&self) -> Option<String> {
        for entry in self.get_entries().iter().rev() {
            if let Entry::SessionInfo(info) = entry {
                // Empty names explicitly clear the session title.
                return info
                    .name
                    .as_ref()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty());
            }
        }
        None
    }

    /// Create a new session file containing only the path from root to the
    /// specified leaf (upstream `createBranchedSession`). Returns the new
    /// session file path, or None when not persisting.
    pub fn create_branched_session(&mut self, leaf_id: &str) -> Result<Option<PathBuf>, String> {
        let previous_session_file = self.session_file.clone();
        let path = self.get_branch(Some(leaf_id));
        if path.is_empty() {
            return Err(format!("Entry {leaf_id} not found"));
        }

        // Filter out label entries; re-chain the retained path to avoid
        // orphaned subtrees.
        let mut path_without_labels: Vec<Entry> = Vec::new();
        let mut path_parent_id: Option<String> = None;
        for entry in path {
            if matches!(entry, Entry::Label(_)) {
                continue;
            }
            let mut cloned = entry.clone();
            cloned.base_mut().parent_id = path_parent_id.clone();
            path_parent_id = Some(cloned.id().to_string());
            path_without_labels.push(cloned);
        }

        let new_session_id = pillar_ai::uuid::uuidv7();
        let timestamp = iso_now();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let new_session_file = self
            .session_dir
            .join(format!("{file_timestamp}_{new_session_id}.jsonl"));

        let header = SessionHeader {
            r#type: "session".to_string(),
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id.clone(),
            timestamp,
            cwd: self.cwd.clone(),
            parent_session: if self.persist {
                previous_session_file.map(|p| p.to_string_lossy().to_string())
            } else {
                None
            },
        };

        // Collect labels for entries in the path.
        let path_entry_ids: std::collections::BTreeSet<String> = path_without_labels
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        let labels_to_write: Vec<(String, String, u64)> = self
            .labels_by_id
            .iter()
            .filter(|(target_id, _)| path_entry_ids.contains(*target_id))
            .map(|(target_id, label)| {
                (
                    target_id.clone(),
                    label.clone(),
                    self.label_timestamps_by_id
                        .get(target_id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();

        let mut label_entries: Vec<Entry> = Vec::new();
        let mut parent_id = path_without_labels.last().map(|e| e.id().to_string());
        let mut id_pool: std::collections::BTreeSet<String> = path_entry_ids.clone();
        for (target_id, label, label_timestamp) in labels_to_write {
            let id = generate_id_with(&id_pool);
            id_pool.insert(id.clone());
            label_entries.push(Entry::Label(LabelEntry {
                base: SessionEntryBase {
                    id,
                    parent_id: parent_id.clone(),
                    timestamp: label_timestamp,
                },
                target_id,
                label: Some(label),
            }));
            parent_id = label_entries.last().map(|e| e.id().to_string());
        }

        self.file_entries = vec![FileEntry::Header(header)];
        for entry in path_without_labels {
            self.file_entries.push(FileEntry::Entry(entry));
        }
        for entry in label_entries {
            self.file_entries.push(FileEntry::Entry(entry));
        }
        self.session_id = new_session_id.clone();
        self.session_file = Some(new_session_file.clone());
        self.build_index();

        // Only write the file now if it contains an assistant message;
        // otherwise defer to persist_entry.
        let has_assistant = self.file_entries.iter().any(|e| {
            matches!(
                e,
                FileEntry::Entry(Entry::Message(message_entry))
                    if matches!(
                        &message_entry.message,
                        CodingAgentMessage::Base(Message::Assistant(_))
                    )
            )
        });
        if has_assistant {
            self.rewrite_file()?;
            self.flushed = true;
        } else {
            self.flushed = false;
        }

        Ok(Some(new_session_file))
    }
}

fn sort_tree_children(nodes: &mut [SessionTreeNode]) {
    for node in nodes.iter_mut() {
        node.children
            .sort_by_key(|child| child.entry.base().timestamp);
        sort_tree_children(&mut node.children);
    }
}

fn iso_now() -> String {
    let now_ms = pillar_ai::models::now_ms();
    let secs = (now_ms / 1000) as i64;
    let millis = (now_ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The default session directory path for a cwd (upstream
/// `getDefaultSessionDirPath`): `~/.pi/agent/sessions/--<encoded-cwd>--`.
pub fn default_session_dir_path(cwd: &str) -> PathBuf {
    let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| PathBuf::from(home).join(".pi").join("agent"))
                .unwrap_or_else(|_| PathBuf::from(".").join(".pi").join("agent"))
        });
    let resolved_cwd = cwd.to_string();
    let safe_path = format!(
        "--{}--",
        resolved_cwd
            .trim_start_matches(['/', '\\'])
            .replace(['/', '\\', ':'], "-")
    );
    agent_dir.join("sessions").join(safe_path)
}

impl FileEntry {
    fn entry_clone(&self) -> &Entry {
        match self {
            FileEntry::Entry(entry) => entry,
            FileEntry::Header(_) => unreachable!("header entry_clone"),
        }
    }
}

impl Entry {
    fn base_mut(&mut self) -> &mut SessionEntryBase {
        match self {
            Entry::Message(e) => &mut e.base,
            Entry::ThinkingLevelChange(e) => &mut e.base,
            Entry::ModelChange(e) => &mut e.base,
            Entry::Compaction(e) => &mut e.base,
            Entry::BranchSummary(e) => &mut e.base,
            Entry::Custom(e) => &mut e.base,
            Entry::Label(e) => &mut e.base,
            Entry::SessionInfo(e) => &mut e.base,
            Entry::CustomMessage(e) => &mut e.base,
        }
    }
}

/// Validate a session id (upstream `assertValidSessionId`).
pub fn assert_valid_session_id(id: &str) {
    let bytes = id.as_bytes();
    let valid = !id.is_empty()
        && bytes.first().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.last().is_some_and(|b| b.is_ascii_alphanumeric())
        && id.len() == 1
        || (!id.is_empty()
            && bytes.first().is_some_and(|b| b.is_ascii_alphanumeric())
            && bytes.last().is_some_and(|b| b.is_ascii_alphanumeric())
            && bytes[1..bytes.len() - 1]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_' || *b == b'.'));
    if !valid {
        panic!(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
        );
    }
}
