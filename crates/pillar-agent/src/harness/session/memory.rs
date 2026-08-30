//! Port of packages/agent/src/harness/session/session.ts and memory.ts
//! (pi v0.84.3) — the `Session` facade over a storage backend plus the
//! in-memory backend and repo.
//!
//! divergence: upstream `SessionStorage` is a TS interface with many async
//! methods; the port defines it as a trait. `assertJsonSerializable`
//! becomes a structural check over the serde-json value (cycles are
//! impossible in Rust ownership, accessor/sparse-array checks are
//! JS-runtime specific and collapse).

use pillar_ai::uuid::uuidv7;

use super::state::{SessionMutation, SessionState};
use super::types::{
    BranchBounds, Entry, EntryPayload, EntryQuery, ForkOptions, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationIntent, OperationOutcome, RecordPayload, RecordQuery, SessionError,
    SessionErrorCode, SessionMetadata, SessionStats,
};
use crate::types::AgentMessage;

/// Storage backend contract (upstream `SessionStorage`).
pub trait SessionStorage: Send + Sync {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError>;

    // Lanes
    fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError>;
    fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError>;
    fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError>;

    // Entries and records
    fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError>;
    fn append_record(&self, record: ProvisionedRecord) -> Result<LaneRecord, SessionError>;

    // Reads
    fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError>;
    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError>;
    fn find_entries_on_branch(
        &self,
        start: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError>;
    fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError>;
    fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError>;
    fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError>;

    // Global facts
    fn get_name(&self) -> Result<Option<String>, SessionError>;
    fn set_name(&self, name: Option<String>) -> Result<(), SessionError>;
    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError>;
    fn set_label(&self, id: &str, label: Option<String>) -> Result<(), SessionError>;
    fn get_stats(&self) -> Result<SessionStats, SessionError>;
}

/// Entry with storage-assigned fields omitted (upstream `ProvisionedEntry`
/// shape threaded through the storage boundary).
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionedEntry {
    pub id: String,
    pub payload: EntryPayload,
}

/// Record with storage-assigned fields omitted (upstream `NewRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionedRecord {
    pub id: String,
    pub lane: String,
    pub payload: RecordPayload,
}

fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
    if limit == Some(0) {
        return Err(SessionError::new(
            SessionErrorCode::InvalidQuery,
            "limit must be a positive integer",
        ));
    }
    Ok(())
}

fn assert_valid_cursor(after_seq: Option<u64>) -> Result<(), SessionError> {
    let _ = after_seq;
    Ok(())
}

/// Durable payload validation (upstream `assertJsonSerializable`). Rust
/// values are always finite/acyclic; NaN/Infinity cannot appear in
/// serde_json::Value, so the remaining check is structural.
pub fn assert_json_serializable(payload: &EntryPayload) -> Result<(), SessionError> {
    let value = match serde_json::to_value(payload) {
        Ok(value) => value,
        Err(_) => {
            return Err(SessionError::new(
                SessionErrorCode::InvalidPayload,
                "Durable payload is not serializable",
            ));
        }
    };
    // serde_json::Value cannot contain non-finite numbers by construction,
    // but keep the guard for explicitness.
    fn check(value: &serde_json::Value) -> Result<(), SessionError> {
        match value {
            serde_json::Value::Number(number) => {
                if !number.is_f64() && !number.is_u64() && !number.is_i64() {
                    return Err(SessionError::new(
                        SessionErrorCode::InvalidPayload,
                        "Durable payload contains a non-finite number",
                    ));
                }
                Ok(())
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    check(item)?;
                }
                Ok(())
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    check(item)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    check(&value)
}

/// Session facade over a storage backend (upstream `Session`).
pub struct Session {
    storage: Box<dyn SessionStorage>,
    id_generator: Box<dyn Fn() -> String + Send + Sync>,
}

impl Session {
    pub fn new(storage: Box<dyn SessionStorage>) -> Self {
        Self {
            storage,
            id_generator: Box::new(uuidv7),
        }
    }

    /// Override the id generator (upstream `options.idGenerator`).
    pub fn with_id_generator(
        mut self,
        id_generator: Box<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        self.id_generator = id_generator;
        self
    }

    pub fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        self.storage.get_metadata()
    }

    pub fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.get_leaf_id_for_lane("main")
    }

    pub fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        self.storage.get_entry(id)
    }

    pub fn get_stats(&self) -> Result<SessionStats, SessionError> {
        self.storage.get_stats()
    }

    pub fn get_name(&self) -> Result<Option<String>, SessionError> {
        self.storage.get_name()
    }

    pub fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        self.storage.set_name(name)
    }

    pub fn get_label(&self, target_id: &str) -> Result<Option<String>, SessionError> {
        self.storage.get_label(target_id)
    }

    pub fn set_label(&self, target_id: &str, label: Option<String>) -> Result<(), SessionError> {
        self.storage.set_label(target_id, label)
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        self.storage.find_entries(query)
    }

    pub fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(self.storage.find_entries(&query)?.into_iter().next())
    }

    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.query_branch_entries("main", query, bounds)
    }

    pub fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(self
            .query_branch_entries("main", &query, bounds)?
            .into_iter()
            .next())
    }

    pub fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        self.append_message_to_lane("main", message)
    }

    pub fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        self.append_custom_entry_to_lane("main", custom_type, data)
    }

    pub fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        self.storage.get_lanes()
    }

    pub fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.storage.create_lane(lane, at)
    }

    pub fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.storage.move_lane(lane, to)
    }

    pub fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError> {
        self.commit_entry(entry, lane)
    }

    pub fn append_record(&self, record: ProvisionedRecord) -> Result<LaneRecord, SessionError> {
        self.commit_record(record)
    }

    pub fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        if query.operation_kind.is_some() && query.kind.as_deref() != Some("operation_started") {
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "operationKind requires type \"operation_started\"",
            ));
        }
        self.storage.find_records(query)
    }

    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(limit)?;
        self.storage.find_open_operations(lane, limit)
    }

    pub fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        assert_valid_limit(options.limit)?;
        assert_valid_cursor(options.after_seq)?;
        self.storage.get_log(options)
    }

    /// Returns the lane's current leaf, or `None` when empty (upstream
    /// `getLeafIdForLane`).
    fn get_leaf_id_for_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        let pointer = self
            .get_lanes()?
            .into_iter()
            .find(|candidate| candidate.lane == lane);
        let Some(pointer) = pointer else {
            return Err(SessionError::new(
                SessionErrorCode::InvalidLane,
                format!("Lane not found: {lane}"),
            ));
        };
        Ok(pointer.leaf_id)
    }

    fn query_branch_entries(
        &self,
        default_lane: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        let start = match query.start.clone() {
            Some(start) => start,
            None => match self.get_leaf_id_for_lane(default_lane)? {
                Some(start) => start,
                None => return Ok(Vec::new()),
            },
        };
        self.storage.find_entries_on_branch(&start, query, bounds)
    }

    fn append_message_to_lane(
        &self,
        lane: &str,
        message: AgentMessage,
    ) -> Result<String, SessionError> {
        let entry = self.commit_entry(
            ProvisionedEntry {
                id: (self.id_generator)(),
                payload: EntryPayload::Message {
                    message,
                    terminate: false,
                },
            },
            lane,
        )?;
        Ok(entry.id)
    }

    fn append_custom_entry_to_lane(
        &self,
        lane: &str,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let entry = self.commit_entry(
            ProvisionedEntry {
                id: (self.id_generator)(),
                payload: EntryPayload::Custom {
                    custom_type: custom_type.to_owned(),
                    data,
                },
            },
            lane,
        )?;
        Ok(entry.id)
    }

    fn commit_entry(&self, entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError> {
        assert_json_serializable(&entry.payload)?;
        self.storage.append_entry(entry, lane)
    }

    fn commit_record(&self, record: ProvisionedRecord) -> Result<LaneRecord, SessionError> {
        assert_json_serializable(&record_payload_as_entry(&record.payload))?;
        self.storage.append_record(record)
    }
}

/// Reuse the entry-level JSON check for record payloads by wrapping the
/// record payload in a synthetic entry payload (upstream validates the
/// whole record object; the port's payload subset carries the same data).
fn record_payload_as_entry(payload: &RecordPayload) -> EntryPayload {
    // Operation intents carry the deep structures; validate via the run
    // intent path.
    match payload {
        RecordPayload::OperationStarted {
            intent:
                OperationIntent::Run {
                    original_prompt,
                    initial_messages,
                    system_prompt_override,
                    resume_data,
                },
            source_leaf_id,
        } => EntryPayload::Compaction {
            summary: String::new(),
            retained_tail: original_prompt.clone(),
            tokens_before: 0,
            details: Some(serde_json::json!({
                "initialMessages": initial_messages,
                "systemPromptOverride": system_prompt_override,
                "resumeData": resume_data,
                "sourceLeafId": source_leaf_id,
            })),
            usage: None,
        },
        RecordPayload::OperationStarted { .. } => EntryPayload::Custom {
            custom_type: String::new(),
            data: None,
        },
        _ => EntryPayload::Custom {
            custom_type: String::new(),
            data: None,
        },
    }
}

/// In-memory storage backend (upstream `InMemorySessionStorage`).
/// Cloning shares the underlying state (repo returns shared handles).
#[derive(Clone)]
pub struct InMemorySessionStorage {
    inner: std::sync::Arc<InMemorySessionStorageInner>,
}

struct InMemorySessionStorageInner {
    metadata: SessionMetadata,
    state: std::sync::Mutex<SessionState>,
}

impl InMemorySessionStorage {
    pub fn new(metadata: SessionMetadata) -> Self {
        Self {
            inner: std::sync::Arc::new(InMemorySessionStorageInner {
                metadata,
                state: std::sync::Mutex::new(SessionState::new()),
            }),
        }
    }

    /// Share the same storage state under another handle.
    pub fn share(&self) -> Self {
        self.clone()
    }

    pub fn fork(
        &self,
        metadata: SessionMetadata,
        options: &ForkOptions,
    ) -> Result<Self, SessionError> {
        let storage = Self::new(metadata);
        {
            let source = self.inner.state.lock().expect("session state lock");
            let mutations = source.create_fork_mutations(options)?;
            let mut target = storage.inner.state.lock().expect("session state lock");
            for mutation in mutations {
                target.apply_mutation(mutation)?;
            }
        }
        Ok(storage)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.inner.state.lock().expect("session state lock")
    }
}

impl SessionStorage for InMemorySessionStorage {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        Ok(self.inner.metadata.clone())
    }

    fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        Ok(self.lock().get_lanes())
    }

    fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.validate_new_lane(lane)?;
        state.validate_target(at)?;
        let seq = state.next_sequence();
        state.apply_mutation(SessionMutation::Lane {
            seq,
            lane: lane.to_owned(),
            leaf_id: at.map(str::to_owned),
        })
    }

    fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.require_lane(lane)?;
        state.validate_target(to)?;
        let seq = state.next_sequence();
        state.apply_mutation(SessionMutation::Lane {
            seq,
            lane: lane.to_owned(),
            leaf_id: to.map(str::to_owned),
        })
    }

    fn append_entry(&self, new_entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError> {
        let mut state = self.lock();
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&new_entry.id)?;
        let entry = Entry {
            kind: new_entry.payload.kind().to_owned(),
            id: new_entry.id,
            seq: state.next_sequence(),
            parent_id,
            timestamp: now_millis(),
            payload: new_entry.payload,
        };
        state.apply_mutation(SessionMutation::Entry {
            lane: Some(lane.to_owned()),
            entry: entry.clone(),
        })?;
        Ok(entry)
    }

    fn append_record(&self, new_record: ProvisionedRecord) -> Result<LaneRecord, SessionError> {
        let mut state = self.lock();
        state.require_lane(&new_record.lane)?;
        state.validate_unused_id(&new_record.id)?;
        let current_open_operation_id = state
            .find_open_operations(&new_record.lane, Some(1))?
            .into_iter()
            .next()
            .map(|record| record.id);
        if matches!(new_record.payload, RecordPayload::OperationStarted { .. })
            && current_open_operation_id.is_some()
        {
            return Err(SessionError::new(
                SessionErrorCode::Storage,
                format!(
                    "Lane {} already has an open operation {}",
                    new_record.lane,
                    current_open_operation_id.unwrap_or_default()
                ),
            ));
        }
        let record = LaneRecord {
            kind: record_kind_str(&new_record.payload),
            id: new_record.id,
            seq: state.next_sequence(),
            lane: new_record.lane,
            timestamp: now_millis(),
            payload: new_record.payload,
        };
        state.apply_mutation(SessionMutation::Record {
            record: record.clone(),
        })?;
        Ok(record)
    }

    fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        Ok(self.lock().get_entry(id).cloned())
    }

    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.lock().find_entries(query)
    }

    fn find_entries_on_branch(
        &self,
        start: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.lock().find_entries_on_branch(start, query, bounds)
    }

    fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        self.lock().find_records(query)
    }

    fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        self.lock().find_open_operations(lane, limit)
    }

    fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        self.lock().get_log(options.after_seq, options.limit)
    }

    fn get_name(&self) -> Result<Option<String>, SessionError> {
        Ok(self.lock().get_name().map(str::to_owned))
    }

    fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        let mut state = self.lock();
        let seq = state.next_sequence();
        state.apply_mutation(SessionMutation::Name { seq, name })
    }

    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        Ok(self.lock().get_label(id).map(str::to_owned))
    }

    fn set_label(&self, id: &str, label: Option<String>) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.validate_target(Some(id))?;
        let seq = state.next_sequence();
        state.apply_mutation(SessionMutation::Label {
            seq,
            target_id: id.to_owned(),
            label,
        })
    }

    fn get_stats(&self) -> Result<SessionStats, SessionError> {
        Ok(*self.lock().get_stats())
    }
}

fn record_kind_str(payload: &RecordPayload) -> String {
    match payload {
        RecordPayload::OperationStarted { .. } => "operation_started".to_owned(),
        RecordPayload::AbortRequested { .. } => "abort_requested".to_owned(),
        RecordPayload::OperationFinished { .. } => "operation_finished".to_owned(),
        RecordPayload::StepAttempt { .. } => "step_attempt".to_owned(),
        RecordPayload::ToolStarted { .. } => "tool_started".to_owned(),
        RecordPayload::QueueEnqueued { .. } => "queue_enqueued".to_owned(),
        RecordPayload::QueueCancelled { .. } => "queue_cancelled".to_owned(),
        RecordPayload::WriteDeferred { .. } => "write_deferred".to_owned(),
        RecordPayload::UsageRecord { .. } => "usage".to_owned(),
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// In-memory repo (upstream `InMemorySessionRepo`).
pub struct InMemorySessionRepo {
    sessions: std::sync::Mutex<std::collections::BTreeMap<String, InMemorySessionStorage>>,
}

impl Default for InMemorySessionRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySessionRepo {
    pub fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn create(&self, options: Option<SessionCreateOptions>) -> Result<Session, SessionError> {
        let options = options.unwrap_or_default();
        let id = options.id.unwrap_or_else(uuidv7);
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }
        let storage = InMemorySessionStorage::new(SessionMetadata {
            id: id.clone(),
            created_at: now_millis(),
            parent_session_id: options.parent_session_id,
        });
        sessions.insert(id.clone(), storage);
        Ok(Session::new(Box::new(
            sessions.get(&id).expect("just inserted").share(),
        )))
    }

    pub fn open(&self, metadata: &SessionMetadata) -> Result<Session, SessionError> {
        let sessions = self.sessions.lock().expect("sessions lock");
        let storage = sessions
            .get(&metadata.id)
            .ok_or_else(|| {
                SessionError::new(
                    SessionErrorCode::NotFound,
                    format!("Session not found: {}", metadata.id),
                )
            })?
            .share();
        Ok(Session::new(Box::new(storage)))
    }

    pub fn list(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        let sessions = self.sessions.lock().expect("sessions lock");
        sessions
            .values()
            .map(|storage| storage.get_metadata())
            .collect()
    }

    pub fn delete(&self, metadata: &SessionMetadata) -> Result<(), SessionError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(&metadata.id);
        Ok(())
    }

    pub fn fork(
        &self,
        source: &SessionMetadata,
        options: Option<(ForkOptions, SessionCreateOptions)>,
    ) -> Result<Session, SessionError> {
        let (fork_options, create_options) = match options {
            Some((fork, create)) => (Some(fork), create),
            None => (None, SessionCreateOptions::default()),
        };
        let id = create_options.id.unwrap_or_else(uuidv7);
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }
        let source_storage = sessions.get(&source.id).ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::NotFound,
                format!("Session not found: {}", source.id),
            )
        })?;
        let default_fork = ForkOptions::Branch {
            entry_id: None,
            position: None,
        };
        let storage = source_storage.fork(
            SessionMetadata {
                id: id.clone(),
                created_at: now_millis(),
                parent_session_id: create_options
                    .parent_session_id
                    .or_else(|| Some(source.id.clone())),
            },
            fork_options.as_ref().unwrap_or(&default_fork),
        )?;
        sessions.insert(id.clone(), storage);
        Ok(Session::new(Box::new(
            sessions.get(&id).expect("just inserted").share(),
        )))
    }
}

/// Session creation options (upstream `SessionCreateOptions`).
#[derive(Debug, Clone, Default)]
pub struct SessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
}

// OperationOutcome reference for the record outcome enum (upstream
// operation_finished.outcome).
#[allow(dead_code)]
fn _outcome_witness(outcome: OperationOutcome) -> OperationOutcome {
    outcome
}

#[allow(dead_code)]
fn _agent_message_witness(_: AgentMessage) {}
