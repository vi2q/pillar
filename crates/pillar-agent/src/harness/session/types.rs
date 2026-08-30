//! Port of packages/agent/src/harness/session/types.ts (pi v0.84.3) — the
//! v4 session tree types.
//!
//! divergence: upstream `Entry`/`LaneRecord` are discriminated TS unions
//! with storage-assigned `seq`/`timestamp` fields; the port models the
//! storage-assigned envelope as a struct and the payload as an enum
//! (`EntryPayload`/`RecordPayload`), which keeps serde serialization
//! tagged on `type`.

use pillar_ai::types::Usage;
use serde::{Deserialize, Serialize};

pub use crate::types::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage,
};

/// Stable session error codes (upstream `SessionErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorCode {
    NotFound,
    AlreadyExists,
    InvalidEntry,
    InvalidPayload,
    InvalidLane,
    InvalidQuery,
    InvalidForkTarget,
    Storage,
}

impl SessionErrorCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::InvalidEntry => "invalid_entry",
            Self::InvalidPayload => "invalid_payload",
            Self::InvalidLane => "invalid_lane",
            Self::InvalidQuery => "invalid_query",
            Self::InvalidForkTarget => "invalid_fork_target",
            Self::Storage => "storage",
        }
    }
}

/// Error returned by session operations (upstream `SessionError`).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

impl SessionError {
    pub fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Storage-assigned entry envelope: `type` discriminant plus the id chain
/// (upstream `EntryBase`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Upstream entry discriminant (`message`, `model_change`,
    /// `thinking_level_change`, `active_tools_change`, `compaction`,
    /// `branch_summary`, `custom`).
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    /// Shared sequence; read-side, storage-assigned.
    #[serde(default)]
    pub seq: u64,
    /// Storage-assigned: the appending lane's leaf.
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    /// Unix ms, storage-assigned.
    #[serde(default)]
    pub timestamp: u64,
    /// Typed payload. Serialized flattened into the entry object.
    #[serde(flatten)]
    pub payload: EntryPayload,
}

/// Typed entry payloads (upstream `MessageEntry` etc. minus the base
/// fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EntryPayload {
    #[serde(rename = "message")]
    Message { message: AgentMessage },
    #[serde(rename = "model_change")]
    ModelChange { provider: String, model_id: String },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange { thinking_level: String },
    #[serde(rename = "active_tools_change")]
    ActiveToolsChange { active_tool_names: Vec<String> },
    #[serde(rename = "compaction")]
    Compaction {
        summary: String,
        retained_tail: Vec<AgentMessage>,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    #[serde(rename = "custom")]
    Custom {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
}

/// Entry kind string for a payload variant (upstream `entry.type`).
impl EntryPayload {
    pub fn kind(&self) -> &'static str {
        match self {
            EntryPayload::Message { .. } => "message",
            EntryPayload::ModelChange { .. } => "model_change",
            EntryPayload::ThinkingLevelChange { .. } => "thinking_level_change",
            EntryPayload::ActiveToolsChange { .. } => "active_tools_change",
            EntryPayload::Compaction { .. } => "compaction",
            EntryPayload::BranchSummary { .. } => "branch_summary",
            EntryPayload::Custom { .. } => "custom",
        }
    }
}

/// Storage-assigned record envelope (upstream `RecordBase`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub seq: u64,
    pub lane: String,
    #[serde(default)]
    pub timestamp: u64,
    /// Typed payload, flattened.
    #[serde(flatten)]
    pub payload: RecordPayload,
}

/// Typed record payloads (upstream operation/step/queue/usage records).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RecordPayload {
    #[serde(rename = "operation_started")]
    OperationStarted {
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    },
    #[serde(rename = "abort_requested")]
    AbortRequested { run_id: String },
    #[serde(rename = "operation_finished")]
    OperationFinished {
        run_id: String,
        outcome: OperationOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RecordError>,
    },
    #[serde(rename = "step_attempt")]
    StepAttempt {
        run_id: String,
        /// `assistant` | `branch_summary` | `compaction`.
        step: String,
        attempt: u32,
        result_entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction_reason: Option<CompactionReason>,
    },
    #[serde(rename = "tool_started")]
    ToolStarted {
        run_id: String,
        assistant_entry_id: String,
        tool_index: u32,
        tool_call_id: String,
        tool_name: String,
        effective_args: serde_json::Value,
        result_entry_id: String,
        /// `never` | `safe`.
        replay: String,
    },
    #[serde(rename = "queue_enqueued")]
    QueueEnqueued {
        /// `steer` | `followUp` | `nextRun`.
        queue: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        target: ProvisionedEntry,
    },
    #[serde(rename = "queue_cancelled")]
    QueueCancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        entry_id: String,
    },
    #[serde(rename = "write_deferred")]
    WriteDeferred {
        run_id: String,
        target: ProvisionedEntry,
    },
    #[serde(rename = "usage")]
    UsageRecord {
        usage: Usage,
        /// `assistant` | `compaction` | `branch_summary` | `deferred_fetch` |
        /// `tool` | `hook` | `adjustment`.
        cause: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
}

/// Operation intent payloads (upstream `OperationStartedRecord.intent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OperationIntent {
    #[serde(rename = "run")]
    Run {
        original_prompt: Vec<AgentMessage>,
        initial_messages: Vec<ProvisionedEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt_override: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_data: Option<serde_json::Value>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        result_entry_id: String,
    },
    #[serde(rename = "navigation")]
    Navigation {
        target_id: Option<String>,
        summarize: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_entry_id: Option<String>,
    },
}

/// Terminal operation outcomes (upstream `operation_finished.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    Completed,
    Aborted,
    Failed,
    Declined,
}

/// Why compaction started (upstream `CompactionReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Record error detail (upstream `operation_finished.error`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordError {
    pub code: String,
    pub message: String,
}

/// An entry with storage-assigned fields omitted (upstream
/// `ProvisionedEntry`): the caller supplies id + payload, storage fills
/// parentId/seq/timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionedEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(flatten)]
    pub payload: EntryPayload,
}

/// Lane pointer (upstream `LanePointer`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanePointer {
    pub lane: String,
    #[serde(rename = "leafId")]
    pub leaf_id: Option<String>,
}

/// Session metadata (upstream `SessionMetadata`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

/// Running token/cost totals (upstream `SessionStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: f64,
    pub uncached_tokens: f64,
    pub total_tokens: f64,
    pub cost_total: f64,
}

/// Entry query filters (upstream `EntryQuery`).
#[derive(Debug, Clone, Default)]
pub struct EntryQuery {
    /// Entry kind filter (`entry.type`).
    pub kind: Option<String>,
    /// For kind "custom": the customType.
    pub custom_type: Option<String>,
    /// Sequence order. Default newestFirst.
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
    pub after_seq: Option<u64>,
    /// Branch query start (upstream merges `start` into the storage query).
    pub start: Option<String>,
}

/// Sequence order (upstream `EntryOrder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrder {
    NewestFirst,
    OldestFirst,
}

/// Branch scan bounds (upstream `BranchBounds`). Default: the whole path,
/// leaf to root.
#[derive(Debug, Clone, Default)]
pub struct BranchBounds {
    /// Scan ends after the first match, inclusive.
    pub stop_at_kind: Option<String>,
    pub stop_at_id: Option<String>,
}

/// Record query filters (upstream `RecordQuery`).
#[derive(Debug, Clone, Default)]
pub struct RecordQuery {
    pub lane: Option<String>,
    pub kind: Option<String>,
    pub run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub after_seq: Option<u64>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
}

/// Log item (upstream `LogItem`).
#[derive(Debug, Clone, PartialEq)]
pub enum LogItem {
    Entry {
        seq: u64,
        entry: Entry,
    },
    Record {
        seq: u64,
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    Name {
        seq: u64,
        name: Option<String>,
    },
    Label {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

impl LogItem {
    pub fn seq(&self) -> u64 {
        match self {
            LogItem::Entry { seq, .. }
            | LogItem::Record { seq, .. }
            | LogItem::Lane { seq, .. }
            | LogItem::Name { seq, .. }
            | LogItem::Label { seq, .. } => *seq,
        }
    }
}

/// Log query options (upstream `LogOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

/// Fork scope options (upstream `ForkOptions`).
#[derive(Debug, Clone)]
pub enum ForkOptions {
    /// Branch scope: copy from an entry (or main leaf) with position
    /// semantics.
    Branch {
        entry_id: Option<String>,
        position: Option<ForkPosition>,
    },
    /// Tree scope: copy everything.
    Tree,
}

/// Fork position relative to the selected entry (upstream
/// `ForkOptions.position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}
