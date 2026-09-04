//! Port of packages/session-backends/sqlite-node/src/sqlite/repo.ts
//! (pi v0.84.3): `SqliteSessionStorage` (the per-session write/read
//! facade over the storage + branch-cache layers, with lease renewal
//! on every transactional write) and `SqliteSessionRepository`
//! (create/open/list/delete/fork/repair/close with a serial operation
//! queue).
//!
//! divergences: the async SerialOperationQueue becomes a std Mutex
//! (rusqlite Connections are not Sync; all operations are synchronous
//! and mutual exclusion is what the queue provided); heartbeats are
//! renewed lazily on each write instead of a timer thread — every
//! write still verifies ownership transactionally; uuidv7 ids are
//! host-supplied; SessionError is the pillar-agent shape.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::branch_cache::{
    CachedBranchEntryRow, CachedBranchQuery, StorageError, append_entry_to_branch_cache,
    build_cached_branch, delete_branch_cache, query_cached_branch_rows, read_cached_branch,
    rebuild_branch_cache,
};
use crate::migrations::apply_migrations;
use crate::storage::{
    EntryRow, EntryRowsQuery, NewEntryRow, NewRecordRow, NewSessionRow, RecordRowsQuery,
    SessionRow, SqliteSessionMetadata, WriterLease, acquire_writer_lease, add_usage_to_stats,
    advance_sequence, append_fact, append_record_row, create_initial_lane, create_lane,
    create_sequence, create_stats, decode_session_metadata, delete_entry_rows, delete_fact_rows,
    delete_lane_rows, delete_record_rows, delete_sequence, delete_session_row, delete_stats,
    delete_writer_lease, finish_lane_operation, get_next_sequence, id_exists_in_entries,
    id_exists_in_records, increment_message_count, insert_entry_row, insert_session_row, move_lane,
    read_entry_row, read_entry_rows, read_fact_rows, read_lane, read_lane_head,
    read_lane_move_rows, read_lanes, read_latest_fact, read_latest_label_facts,
    read_open_operation_rows, read_record_rows, read_session_row, read_session_rows, read_stats,
    release_writer_lease, renew_writer_lease, session_exists, set_lane_leaf, set_next_sequence,
    start_lane_operation,
};
use pillar_agent::harness::session::memory::{ProvisionedRecord, SessionStorage};
use pillar_agent::harness::session::types::{
    BranchBounds, Entry, EntryOrder, EntryPayload, EntryQuery, ForkOptions, ForkPosition,
    LanePointer, LaneRecord, LogItem, LogOptions, OperationIntent, ProvisionedEntry, RecordPayload,
    RecordQuery, SessionError, SessionErrorCode, SessionMetadata, SessionStats,
};

/// Writer lease tuning (upstream `SqliteWriterLeaseOptions`).
#[derive(Debug, Clone, Copy)]
pub struct WriterLeaseOptions {
    /// Time without a successful renewal before another writer may take
    /// over. Default: 30 seconds.
    pub ttl_ms: i64,
}

impl Default for WriterLeaseOptions {
    fn default() -> Self {
        Self { ttl_ms: 30_000 }
    }
}

fn require_session_row(db: &Connection, session_id: &str) -> Result<SessionRow, SessionError> {
    read_session_row(db, session_id).ok_or_else(|| {
        SessionError::new(
            SessionErrorCode::NotFound,
            format!("Session not found: {session_id}"),
        )
    })
}

fn session_error(storage: &StorageError) -> SessionError {
    let code = match storage.kind {
        "not_found" => SessionErrorCode::NotFound,
        "already_exists" => SessionErrorCode::AlreadyExists,
        "invalid_entry" => SessionErrorCode::InvalidEntry,
        "invalid_payload" => SessionErrorCode::InvalidPayload,
        "invalid_lane" => SessionErrorCode::InvalidLane,
        "invalid_fork_target" => SessionErrorCode::InvalidForkTarget,
        _ => SessionErrorCode::Storage,
    };
    SessionError::new(code, storage.message.clone())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// The entry object minus the base fields (upstream `entryPayload`):
/// the base fields live in dedicated columns, the payload keeps its
/// own `type` discriminant.
fn entry_payload_json(entry: &Entry) -> Result<String, SessionError> {
    serde_json::to_value(&entry.payload)
        .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
        .and_then(|value| {
            serde_json::to_string(&value)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
        })
}

/// Decode an entry row (upstream `decodeEntry`): the payload JSON
/// carries the `type` discriminant plus the payload fields.
fn decode_entry(row: &EntryRow) -> Result<Entry, SessionError> {
    let payload: EntryPayload = serde_json::from_str(&row.payload).map_err(|error| {
        SessionError::new(
            SessionErrorCode::InvalidEntry,
            format!(
                "Invalid SQLite session entry {}: failed to decode entry {} ({error})",
                row.id, row.id
            ),
        )
    })?;
    Ok(Entry {
        id: row.id.clone(),
        seq: row.seq as u64,
        parent_id: row.parent_id.clone(),
        timestamp: row.timestamp as u64,
        payload,
    })
}

fn entry_row_from_cached(row: &CachedBranchEntryRow) -> EntryRow {
    EntryRow {
        session_id: row.session_id.clone(),
        seq: row.entry_seq,
        id: row.id.clone(),
        parent_id: row.parent_id.clone(),
        entry_type: row.entry_type.clone(),
        timestamp: row.timestamp,
        payload: row.payload.clone(),
    }
}

/// Upstream `recordRunId`: operation_started owns the id; other
/// records carry a runId when present.
fn record_run_id(record: &ProvisionedRecord) -> Option<String> {
    match &record.payload {
        RecordPayload::OperationStarted { .. } => Some(record.id.clone()),
        payload => match payload {
            RecordPayload::AbortRequested { run_id } => Some(run_id.clone()),
            RecordPayload::OperationFinished { run_id, .. } => Some(run_id.clone()),
            RecordPayload::StepAttempt { run_id, .. } => Some(run_id.clone()),
            RecordPayload::ToolStarted { run_id, .. } => Some(run_id.clone()),
            RecordPayload::QueueEnqueued { run_id, .. } => run_id.clone(),
            RecordPayload::QueueCancelled { run_id, .. } => run_id.clone(),
            RecordPayload::WriteDeferred { run_id, .. } => Some(run_id.clone()),
            RecordPayload::UsageRecord { run_id, .. } => run_id.clone(),
            RecordPayload::OperationStarted { .. } => None,
        },
    }
}

/// Upstream `recordOpKind`: the intent kind of an operation_started
/// record.
fn record_op_kind(record: &ProvisionedRecord) -> Option<String> {
    match &record.payload {
        RecordPayload::OperationStarted { intent, .. } => Some(
            match intent {
                OperationIntent::Run { .. } => "run",
                OperationIntent::Compaction { .. } => "compaction",
                OperationIntent::Navigation { .. } => "navigation",
            }
            .to_string(),
        ),
        _ => None,
    }
}

/// Decode a record row (upstream `decodeRecord`): the payload JSON is
/// the full LaneRecord shape minus seq/timestamp.
fn decode_record(
    record_id: &str,
    lane: &str,
    seq: i64,
    timestamp: i64,
    payload: &str,
) -> Result<LaneRecord, SessionError> {
    let payload_value: RecordPayload = serde_json::from_str(payload).map_err(|error| {
        SessionError::new(
            SessionErrorCode::Storage,
            format!(
                "Invalid SQLite session record at sequence {seq}: failed to decode payload ({error})"
            ),
        )
    })?;
    let mut record = LaneRecord {
        id: record_id.to_string(),
        seq: seq as u64,
        lane: lane.to_string(),
        timestamp: timestamp as u64,
        payload: payload_value,
    };
    let _ = &mut record;
    Ok(record)
}

/// Upstream `matchesEntryQuery` (cursor semantics are handled by the
/// SQL layer).
fn matches_entry_query(
    entry: &Entry,
    kind: Option<&str>,
    custom_type: Option<&str>,
    cursor_after_seq: Option<u64>,
) -> bool {
    if let Some(kind) = kind
        && entry.kind() != kind
    {
        return false;
    }
    if let Some(custom_type) = custom_type {
        let EntryPayload::Custom {
            custom_type: entry_custom_type,
            ..
        } = &entry.payload
        else {
            return false;
        };
        if entry_custom_type != custom_type {
            return false;
        }
    }
    if let Some(cursor) = cursor_after_seq {
        // The caller applies cursor semantics per order.
        let _ = cursor;
    }
    true
}

/// Shared connection handle (the repository owns the Connection;
/// storages borrow it through this alias).
type SharedDb = Arc<Mutex<Option<Connection>>>;

fn with_shared_db<T>(
    db: &SharedDb,
    operation: impl FnOnce(&mut Connection) -> Result<T, SessionError>,
) -> Result<T, SessionError> {
    let mut guard = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let connection = guard
        .as_mut()
        .ok_or_else(|| SessionError::new(SessionErrorCode::Storage, "Repository is closed"))?;
    operation(connection)
}

/// Per-session storage facade (upstream `SqliteSessionStorage`).
pub struct SqliteSessionStorage {
    db: SharedDb,
    /// Callback clearing this storage's active-session registration
    /// (upstream `onRelease`).
    on_release: Box<dyn Fn() + Send + Sync>,
    metadata: SqliteSessionMetadata,
    lease: Mutex<WriterLease>,
    released: std::sync::atomic::AtomicBool,
    lease_options: WriterLeaseOptions,
}

impl SqliteSessionStorage {
    /// Transactional write wrapper (upstream `enqueueWrite`): renew
    /// the lease inside the transaction; a lost lease poisons every
    /// subsequent write.
    fn enqueue_write<T>(
        &self,
        operation: impl FnOnce(&Connection) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        with_shared_db(&self.db, |db| {
            let transaction = db
                .transaction()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let now = now_ms();
            let mut lease = self
                .lease
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !renew_writer_lease(
                &transaction,
                &self.metadata.id,
                &mut lease,
                now,
                now + self.lease_options.ttl_ms,
            ) {
                return Err(SessionError::new(
                    SessionErrorCode::Storage,
                    format!("SQLite session {} writer lease was lost", self.metadata.id),
                ));
            }
            drop(lease);
            let result = operation(&transaction)?;
            transaction
                .commit()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(result)
        })
    }

    pub fn is_for_session(&self, session_id: &str) -> bool {
        self.metadata.id == session_id
    }

    /// Release the lease (upstream `release`): idempotent.
    pub fn release(&self) -> Result<(), SessionError> {
        if self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        let result = with_shared_db(&self.db, |db| {
            release_writer_lease(
                db,
                &self.metadata.id,
                &self
                    .lease
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
        });
        (self.on_release)();
        result
    }

    fn assert_unused_id(db: &Connection, session_id: &str, id: &str) -> Result<(), SessionError> {
        if id_exists_in_entries(db, session_id, id) || id_exists_in_records(db, session_id, id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("ID already exists: {id}"),
            ));
        }
        Ok(())
    }
}

impl SessionStorage for SqliteSessionStorage {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        with_shared_db(&self.db, |db| {
            let row = require_session_row(db, &self.metadata.id)?;
            let metadata = decode_session_metadata(&row, &self.metadata.path)
                .map_err(|error| session_error(&error))?;
            Ok(SessionMetadata {
                id: metadata.id,
                created_at: metadata.created_at as u64,
                parent_session_id: metadata.parent_session_id,
            })
        })
    }

    fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        with_shared_db(&self.db, |db| {
            Ok(read_lanes(db, &self.metadata.id)
                .map_err(|error| session_error(&error))?
                .into_iter()
                .map(|row| LanePointer {
                    lane: row.lane,
                    leaf_id: row.leaf_id,
                })
                .collect())
        })
    }

    fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        self.enqueue_write(|db| {
            if read_lane(db, &self.metadata.id, lane).is_some() {
                return Err(SessionError::new(
                    SessionErrorCode::AlreadyExists,
                    format!("Lane already exists: {lane}"),
                ));
            }
            if at.is_some_and(|at| read_entry_row(db, &self.metadata.id, at).is_none()) {
                return Err(SessionError::new(
                    SessionErrorCode::NotFound,
                    format!("Entry not found: {}", at.unwrap_or_default()),
                ));
            }
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            create_lane(db, &self.metadata.id, seq, lane, at)
                .map_err(|error| session_error(&error))?;
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(())
        })
    }

    fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        self.enqueue_write(|db| {
            if read_lane(db, &self.metadata.id, lane).is_none() {
                return Err(SessionError::new(
                    SessionErrorCode::InvalidLane,
                    format!("Lane not found: {lane}"),
                ));
            }
            if to.is_some_and(|to| read_entry_row(db, &self.metadata.id, to).is_none()) {
                return Err(SessionError::new(
                    SessionErrorCode::NotFound,
                    format!("Entry not found: {}", to.unwrap_or_default()),
                ));
            }
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            move_lane(db, &self.metadata.id, seq, lane, to)
                .map_err(|error| session_error(&error))?;
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(())
        })
    }

    fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError> {
        self.enqueue_write(|db| {
            let parent_id = read_lane_head(db, &self.metadata.id, lane)
                .map_err(|error| session_error(&error))?;
            Self::assert_unused_id(db, &self.metadata.id, &entry.id)?;
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            let committed = Entry {
                id: entry.id.clone(),
                seq: seq as u64,
                parent_id: parent_id.clone(),
                timestamp: now_ms() as u64,
                payload: entry.payload,
            };
            let payload = entry_payload_json(&committed)?;
            insert_entry_row(
                db,
                &self.metadata.id,
                &NewEntryRow {
                    seq,
                    id: committed.id.clone(),
                    parent_id: committed.parent_id.clone(),
                    entry_type: committed.kind().to_string(),
                    timestamp: committed.timestamp as i64,
                    payload,
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            set_lane_leaf(db, &self.metadata.id, lane, Some(&committed.id))
                .map_err(|error| session_error(&error))?;
            let custom_type = match &committed.payload {
                EntryPayload::Custom { custom_type, .. } => Some(custom_type.as_str()),
                _ => None,
            };
            append_entry_to_branch_cache(
                db,
                &self.metadata.id,
                &committed.id,
                seq,
                committed.kind(),
                custom_type,
                committed.parent_id.as_deref(),
                &format!("branch-{}", committed.id),
            )
            .map_err(|error| session_error(&error))?;
            if committed.kind() == "message" {
                increment_message_count(db, &self.metadata.id)
                    .map_err(|error| session_error(&error))?;
            }
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(committed)
        })
    }

    fn append_record(&self, record: ProvisionedRecord) -> Result<LaneRecord, SessionError> {
        self.enqueue_write(|db| {
            if read_lane(db, &self.metadata.id, &record.lane).is_none() {
                return Err(SessionError::new(
                    SessionErrorCode::InvalidLane,
                    format!("Lane not found: {}", record.lane),
                ));
            }
            Self::assert_unused_id(db, &self.metadata.id, &record.id)?;
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            // Store the payload in the LaneRecord serde shape (flattened,
            // upstream JSON.stringify(record) on the NewRecord): the
            // payload's serde tag already emits `type`, the base fields
            // ride alongside on read.
            let payload = serde_json::to_string(&record.payload)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            if matches!(record.payload, RecordPayload::OperationStarted { .. }) {
                start_lane_operation(db, &self.metadata.id, &record.lane, &record.id)
                    .map_err(|error| session_error(&error))?;
            }
            append_record_row(
                db,
                &self.metadata.id,
                &NewRecordRow {
                    seq,
                    id: record.id.clone(),
                    lane: record.lane.clone(),
                    run_id: record_run_id(&record),
                    record_type: record.payload.kind().to_string(),
                    op_kind: record_op_kind(&record),
                    timestamp: now_ms(),
                    payload,
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            if let RecordPayload::OperationFinished { run_id, .. } = &record.payload {
                finish_lane_operation(db, &self.metadata.id, &record.lane, run_id.as_str())
                    .map_err(|error| {
                        SessionError::new(SessionErrorCode::Storage, error.to_string())
                    })?;
            }
            if let RecordPayload::UsageRecord { usage, .. } = &record.payload {
                add_usage_to_stats(
                    db,
                    &self.metadata.id,
                    usage.cache_read,
                    usage.input + usage.cache_write,
                    usage.total_tokens,
                    usage.cost.total,
                )
                .map_err(|error| session_error(&error))?;
            }
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(LaneRecord {
                id: record.id,
                seq: seq as u64,
                lane: record.lane,
                timestamp: now_ms() as u64,
                payload: record.payload,
            })
        })
    }

    fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        with_shared_db(&self.db, |db| {
            match read_entry_row(db, &self.metadata.id, id) {
                Some(row) => Ok(Some(decode_entry(&row)?)),
                None => Ok(None),
            }
        })
    }

    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        with_shared_db(&self.db, |db| {
            let sql_kind = query
                .kind
                .clone()
                .or_else(|| query.custom_type.clone().map(|_| "custom".to_string()));
            let sql_limit = if query.custom_type.is_none() {
                query.limit
            } else {
                None
            };
            let oldest_first = query.order == Some(EntryOrder::OldestFirst);
            let rows = read_entry_rows(
                db,
                &self.metadata.id,
                &EntryRowsQuery {
                    cursor_after_seq: query.after_seq.map(|seq| seq as i64),
                    limit: sql_limit,
                    oldest_first,
                    entry_type: sql_kind,
                    ..Default::default()
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let mut entries = Vec::new();
            for row in rows {
                let entry = decode_entry(&row)?;
                if matches_entry_query(
                    &entry,
                    query.kind.as_deref(),
                    query.custom_type.as_deref(),
                    None,
                ) {
                    entries.push(entry);
                }
            }
            Ok(match query.limit {
                Some(limit) => entries.into_iter().take(limit).collect(),
                None => entries,
            })
        })
    }

    fn find_entries_on_branch(
        &self,
        start: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        with_shared_db(&self.db, |db| {
            let cached = read_cached_branch(db, &self.metadata.id, start);
            let Some(cached) = cached else {
                if read_entry_row(db, &self.metadata.id, start).is_none() {
                    return Err(SessionError::new(
                        SessionErrorCode::NotFound,
                        format!("Entry not found: {start}"),
                    ));
                }
                return Err(SessionError::new(
                    SessionErrorCode::InvalidEntry,
                    format!("Branch cache missing entry {start}"),
                ));
            };
            let oldest_first = query.order != Some(EntryOrder::NewestFirst);
            let rows = query_cached_branch_rows(
                db,
                &self.metadata.id,
                &cached,
                &CachedBranchQuery {
                    entry_type: query.kind.clone(),
                    custom_type: query.custom_type.clone(),
                    stop_at_type: bounds.stop_at_kind.clone(),
                    stop_at_id: bounds.stop_at_id.clone(),
                    cursor_after_seq: query.after_seq.map(|seq| seq as i64),
                    oldest_first,
                    limit: query.limit,
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let mut entries = Vec::new();
            for row in rows {
                let entry = decode_entry(&entry_row_from_cached(&row))?;
                if matches_entry_query(
                    &entry,
                    query.kind.as_deref(),
                    query.custom_type.as_deref(),
                    None,
                ) {
                    entries.push(entry);
                }
            }
            Ok(match query.limit {
                Some(limit) => entries.into_iter().take(limit).collect(),
                None => entries,
            })
        })
    }

    fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        with_shared_db(&self.db, |db| {
            let rows = read_record_rows(
                db,
                &self.metadata.id,
                &RecordRowsQuery {
                    lane: query.lane.clone(),
                    record_type: query.kind.clone(),
                    run_id: query.run_id.clone(),
                    operation_kind: query.operation_kind.clone(),
                    after_seq: query.after_seq.map(|seq| seq as i64),
                    oldest_first: query.order == Some(EntryOrder::OldestFirst),
                    limit: query.limit,
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            rows.iter()
                .map(|row| decode_record(&row.id, &row.lane, row.seq, row.timestamp, &row.payload))
                .collect()
        })
    }

    fn find_open_operations(
        &self,
        lane: &str,
        _limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        with_shared_db(&self.db, |db| {
            let rows = read_open_operation_rows(db, &self.metadata.id, lane)
                .map_err(|error| session_error(&error))?;
            rows.iter()
                .map(|row| {
                    let record =
                        decode_record(&row.id, &row.lane, row.seq, row.timestamp, &row.payload)?;
                    if record.kind() != "operation_started" {
                        return Err(SessionError::new(
                            SessionErrorCode::Storage,
                            "Expected operation_started record",
                        ));
                    }
                    Ok(record)
                })
                .collect()
        })
    }

    fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        with_shared_db(&self.db, |db| {
            let after_seq = options.after_seq.map(|seq| seq as i64).unwrap_or(0);
            let limit = options.limit;
            let entry_rows = read_entry_rows(
                db,
                &self.metadata.id,
                &EntryRowsQuery {
                    after_seq: Some(after_seq),
                    oldest_first: true,
                    limit,
                    ..Default::default()
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let record_rows = read_record_rows(
                db,
                &self.metadata.id,
                &RecordRowsQuery {
                    after_seq: Some(after_seq),
                    oldest_first: true,
                    limit,
                    ..Default::default()
                },
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let lane_rows = read_lane_move_rows(db, &self.metadata.id, Some(after_seq), limit)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let fact_rows = read_fact_rows(db, &self.metadata.id, Some(after_seq), limit)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;

            let mut log_rows: Vec<(i64, LogItem)> = Vec::new();
            for row in &entry_rows {
                log_rows.push((
                    row.seq,
                    LogItem::Entry {
                        seq: row.seq as u64,
                        entry: decode_entry(row)?,
                    },
                ));
            }
            for row in &record_rows {
                log_rows.push((
                    row.seq,
                    LogItem::Record {
                        seq: row.seq as u64,
                        record: decode_record(
                            &row.id,
                            &row.lane,
                            row.seq,
                            row.timestamp,
                            &row.payload,
                        )?,
                    },
                ));
            }
            for row in &lane_rows {
                log_rows.push((
                    row.seq,
                    LogItem::Lane {
                        seq: row.seq as u64,
                        lane: row.lane.clone(),
                        leaf_id: row.leaf_id.clone(),
                    },
                ));
            }
            for row in &fact_rows {
                match row.kind.as_str() {
                    "name" => log_rows.push((
                        row.seq,
                        LogItem::Name {
                            seq: row.seq as u64,
                            name: row.value.as_deref().map(|value| {
                                serde_json::from_str(value).unwrap_or_else(|_| value.to_string())
                            }),
                        },
                    )),
                    _ => log_rows.push((
                        row.seq,
                        LogItem::Label {
                            seq: row.seq as u64,
                            target_id: row.key.clone().unwrap_or_default(),
                            label: row.value.as_deref().map(|value| {
                                serde_json::from_str(value).unwrap_or_else(|_| value.to_string())
                            }),
                        },
                    )),
                }
            }
            log_rows.sort_by_key(|(seq, _)| *seq);
            Ok(log_rows.into_iter().map(|(_, item)| item).collect())
        })
    }

    fn get_name(&self) -> Result<Option<String>, SessionError> {
        with_shared_db(&self.db, |db| {
            let row = read_latest_fact(db, &self.metadata.id, "name", None);
            match row.and_then(|row| row.value) {
                Some(value) => Ok(Some(serde_json::from_str(&value).unwrap_or(value))),
                None => Ok(None),
            }
        })
    }

    fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        self.enqueue_write(|db| {
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            append_fact(
                db,
                &self.metadata.id,
                seq,
                "name",
                None,
                name.map(|name| serde_json::to_string(&name).unwrap_or_default())
                    .as_deref(),
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(())
        })
    }

    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        with_shared_db(&self.db, |db| {
            let row = read_latest_fact(db, &self.metadata.id, "label", Some(id));
            match row.and_then(|row| row.value) {
                Some(value) => Ok(Some(serde_json::from_str(&value).unwrap_or(value))),
                None => Ok(None),
            }
        })
    }

    fn set_label(&self, id: &str, label: Option<String>) -> Result<(), SessionError> {
        self.enqueue_write(|db| {
            if read_entry_row(db, &self.metadata.id, id).is_none() {
                return Err(SessionError::new(
                    SessionErrorCode::NotFound,
                    format!("Entry not found: {id}"),
                ));
            }
            let seq =
                get_next_sequence(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            append_fact(
                db,
                &self.metadata.id,
                seq,
                "label",
                Some(id),
                label
                    .map(|label| serde_json::to_string(&label).unwrap_or_default())
                    .as_deref(),
            )
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            advance_sequence(db, &self.metadata.id, seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            Ok(())
        })
    }

    fn get_stats(&self) -> Result<SessionStats, SessionError> {
        with_shared_db(&self.db, |db| {
            let row = read_stats(db, &self.metadata.id).map_err(|error| session_error(&error))?;
            Ok(SessionStats {
                message_count: row.message_count as u64,
                cached_tokens: row.cached_tokens,
                uncached_tokens: row.uncached_tokens,
                total_tokens: row.total_tokens,
                cost_total: row.cost_total,
            })
        })
    }
}

/// A claimed lease or the error that prevented claiming (upstream
/// `claimWriterLease`).
fn claim_writer_lease(
    db: &Connection,
    session_id: &str,
    options: &WriterLeaseOptions,
    owner_id: &str,
) -> Result<WriterLease, SessionError> {
    let now = now_ms();
    acquire_writer_lease(db, session_id, owner_id, now, now + options.ttl_ms).ok_or_else(|| {
        SessionError::new(
            SessionErrorCode::Storage,
            format!("SQLite session {session_id} already has an active writer"),
        )
    })
}

/// Session repository (upstream `SqliteSessionRepository`): create /
/// open / list / delete / fork / repair / close. Single-threaded by
/// construction — the upstream SerialOperationQueue serialized async
/// operations, the Mutex-guarded Connection provides the same
/// exclusion here.
pub struct SqliteSessionRepository {
    db: SharedDb,
    lease_options: WriterLeaseOptions,
    owner_counter: std::sync::atomic::AtomicU64,
    /// Ids of storages currently handed out (upstream
    /// `activeStorages`; active storages release on claim conflicts).
    active_sessions: Arc<Mutex<HashSet<String>>>,
}

impl SqliteSessionRepository {
    pub fn new(
        mut db: Connection,
        lease_options: WriterLeaseOptions,
    ) -> Result<Self, SessionError> {
        apply_migrations(&mut db)
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
        Ok(Self {
            db: Arc::new(Mutex::new(Some(db))),
            lease_options,
            owner_counter: std::sync::atomic::AtomicU64::new(1),
            active_sessions: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn open_in_memory(lease_options: WriterLeaseOptions) -> Result<Self, SessionError> {
        Self::new(
            Connection::open_in_memory()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?,
            lease_options,
        )
    }

    pub fn open_path(path: &str, lease_options: WriterLeaseOptions) -> Result<Self, SessionError> {
        Self::new(
            Connection::open(path)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?,
            lease_options,
        )
    }

    fn with_db<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        with_shared_db(&self.db, operation)
    }

    fn next_owner(&self) -> String {
        let counter = self
            .owner_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("owner-{counter}")
    }

    /// Open an existing session (upstream `open`).
    pub fn open(
        &self,
        metadata: &SqliteSessionMetadata,
    ) -> Result<SqliteSessionStorage, SessionError> {
        self.claim_session(metadata)
    }

    fn claim_session(
        &self,
        metadata: &SqliteSessionMetadata,
    ) -> Result<SqliteSessionStorage, SessionError> {
        self.with_db(|db| {
            require_session_row(db, &metadata.id)?;
            // An already-active storage for the same session is reused
            // upstream; the Mutex facade means a second claim here would
            // deadlock on release, so an active writer is refused.
            let mut active = self
                .active_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if active.contains(&metadata.id) {
                return Err(SessionError::new(
                    SessionErrorCode::Storage,
                    format!(
                        "SQLite session {} already has an active writer",
                        metadata.id
                    ),
                ));
            }
            let lease =
                claim_writer_lease(db, &metadata.id, &self.lease_options, &self.next_owner())?;
            read_lanes(db, &metadata.id).map_err(|error| session_error(&error))?;
            let row = require_session_row(db, &metadata.id)?;
            let decoded = decode_session_metadata(&row, &metadata.path)
                .map_err(|error| session_error(&error))?;
            active.insert(metadata.id.clone());
            let db = self.db.clone();
            let active_ids = self.active_sessions.clone();
            let session_id = decoded.id.clone();
            Ok(SqliteSessionStorage {
                db,
                on_release: Box::new(move || {
                    active_ids
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session_id);
                }),
                metadata: decoded,
                lease: Mutex::new(lease),
                released: std::sync::atomic::AtomicBool::new(false),
                lease_options: self.lease_options,
            })
        })
    }

    /// Create a session (upstream `create`).
    pub fn create(
        &self,
        id: &str,
        cwd: &str,
        parent_session_id: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<SqliteSessionStorage, SessionError> {
        self.with_db(|db| {
            if session_exists(db, id) {
                return Err(SessionError::new(
                    SessionErrorCode::AlreadyExists,
                    format!("Session already exists: {id}"),
                ));
            }
            let created_at = now_ms();
            let transaction = db
                .transaction()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            insert_session_row(
                &transaction,
                &NewSessionRow {
                    id: id.to_string(),
                    created_at,
                    cwd: cwd.to_string(),
                    parent_session_id: parent_session_id.map(|value| value.to_string()),
                    metadata,
                },
            )
            .map_err(|error| session_error(&error))?;
            create_sequence(&transaction, id, 1)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            create_stats(&transaction, id, 0)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            create_initial_lane(&transaction, id, "main", None)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let lease =
                claim_writer_lease(&transaction, id, &self.lease_options, &self.next_owner())?;
            transaction
                .commit()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let row = require_session_row(db, id)?;
            let decoded =
                decode_session_metadata(&row, "").map_err(|error| session_error(&error))?;
            self.active_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.to_string());
            let db = self.db.clone();
            let active_ids = self.active_sessions.clone();
            let session_id = decoded.id.clone();
            Ok(SqliteSessionStorage {
                db,
                on_release: Box::new(move || {
                    active_ids
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session_id);
                }),
                metadata: decoded,
                lease: Mutex::new(lease),
                released: std::sync::atomic::AtomicBool::new(false),
                lease_options: self.lease_options,
            })
        })
    }

    /// Read the session catalog without writer leases (upstream
    /// `list`).
    pub fn list(&self, cwd: Option<&str>) -> Result<Vec<SqliteSessionMetadata>, SessionError> {
        self.with_db(|db| {
            let rows = read_session_rows(db, cwd)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            rows.iter()
                .map(|row| decode_session_metadata(row, "").map_err(|error| session_error(&error)))
                .collect()
        })
    }

    /// Release the active storage for a session, then rebuild its
    /// branch cache (upstream `repairBranchCache`).
    pub fn repair_branch_cache(&self, session_id: &str) -> Result<(), SessionError> {
        self.with_db(|db| {
            self.active_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id);
            let transaction = db
                .transaction()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let lease = claim_writer_lease(
                &transaction,
                session_id,
                &self.lease_options,
                &self.next_owner(),
            )?;
            require_session_row(&transaction, session_id)?;
            rebuild_branch_cache(&transaction, session_id)
                .map_err(|error| session_error(&error))?;
            release_writer_lease(&transaction, session_id, &lease)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            transaction
                .commit()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
        })
    }

    /// Delete a session and all derived rows (upstream `delete`).
    pub fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        self.with_db(|db| {
            self.active_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id);
            let transaction = db
                .transaction()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            if !session_exists(&transaction, session_id) {
                delete_writer_lease(&transaction, session_id).map_err(|error| {
                    SessionError::new(SessionErrorCode::Storage, error.to_string())
                })?;
                transaction.commit().map_err(|error| {
                    SessionError::new(SessionErrorCode::Storage, error.to_string())
                })?;
                return Ok(());
            }
            let lease = claim_writer_lease(
                &transaction,
                session_id,
                &self.lease_options,
                &self.next_owner(),
            )?;
            delete_branch_cache(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_fact_rows(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_lane_rows(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_record_rows(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_entry_rows(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_writer_lease(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_stats(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_sequence(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            delete_session_row(&transaction, session_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            drop(lease);
            transaction
                .commit()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
        })
    }

    /// Fork a session (upstream `fork`): tree scope copies everything;
    /// branch scope copies a main-lane branch up to the target entry
    /// with at/before position semantics.
    #[allow(clippy::too_many_arguments)]
    pub fn fork(
        &self,
        source_id: &str,
        new_id: &str,
        scope: &ForkOptions,
        cwd: &str,
        parent_session_id: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<SqliteSessionStorage, SessionError> {
        self.with_db(|db| {
            let source_row = require_session_row(db, source_id)?;
            let source_metadata =
                decode_session_metadata(&source_row, "").map_err(|error| session_error(&error))?;
            if session_exists(db, new_id) {
                return Err(SessionError::new(
                    SessionErrorCode::AlreadyExists,
                    format!("Session already exists: {new_id}"),
                ));
            }

            let mut entries: Vec<EntryRow> = Vec::new();
            let mut lanes: Vec<(String, Option<String>)> = Vec::new();
            let mut branch_tips: Vec<String> = Vec::new();
            let mut branch_fork_target_id: Option<String> = None;

            match scope {
                ForkOptions::Tree => {
                    entries = read_entry_rows(
                        db,
                        source_id,
                        &EntryRowsQuery {
                            oldest_first: true,
                            ..Default::default()
                        },
                    )
                    .map_err(|error| {
                        SessionError::new(SessionErrorCode::Storage, error.to_string())
                    })?;
                    lanes = read_lanes(db, source_id)
                        .map_err(|error| session_error(&error))?
                        .into_iter()
                        .map(|row| (row.lane, row.leaf_id))
                        .collect();
                    branch_tips = crate::branch_cache::read_branch_tip_ids(db, source_id).map_err(
                        |error| SessionError::new(SessionErrorCode::Storage, error.to_string()),
                    )?;
                }
                ForkOptions::Branch { entry_id, position } => {
                    let main = read_lane(db, source_id, "main").ok_or_else(|| {
                        SessionError::new(SessionErrorCode::InvalidLane, "Lane not found: main")
                    })?;
                    let selected_entry_id = entry_id.clone().or_else(|| main.leaf_id.clone());
                    if let Some(selected) = &selected_entry_id {
                        let target = read_entry_row(db, source_id, selected);
                        if target
                            .as_ref()
                            .is_none_or(|target| target.entry_type != "message")
                        {
                            return Err(SessionError::new(
                                SessionErrorCode::InvalidForkTarget,
                                format!("Fork target is not a message entry: {selected}"),
                            ));
                        }
                        let target = target.unwrap();
                        let at = matches!(position, Some(ForkPosition::At))
                            || (position.is_none() && entry_id.is_none());
                        branch_fork_target_id = if at {
                            Some(target.id.clone())
                        } else {
                            target.parent_id.clone()
                        };
                    }
                    lanes.push(("main".to_string(), branch_fork_target_id.clone()));
                    if let Some(target_id) = &branch_fork_target_id {
                        let cached =
                            read_cached_branch(db, source_id, target_id).ok_or_else(|| {
                                SessionError::new(
                                    SessionErrorCode::InvalidForkTarget,
                                    format!("Fork target is not on a cached branch: {target_id}"),
                                )
                            })?;
                        let rows = query_cached_branch_rows(
                            db,
                            source_id,
                            &cached,
                            &CachedBranchQuery {
                                oldest_first: true,
                                ..Default::default()
                            },
                        )
                        .map_err(|error| {
                            SessionError::new(SessionErrorCode::Storage, error.to_string())
                        })?;
                        entries.extend(rows.iter().map(entry_row_from_cached));
                        branch_tips.push(target_id.clone());
                    }
                }
            }

            let copied_ids: HashSet<String> =
                entries.iter().map(|entry| entry.id.clone()).collect();
            let latest_name = read_latest_fact(db, source_id, "name", None);
            let latest_labels = read_latest_label_facts(db, source_id)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            let labels_to_copy: Vec<_> = latest_labels
                .into_iter()
                .filter(|(key, _)| scope_is_tree(scope) || copied_ids.contains(key))
                .collect();
            let created_at = now_ms();
            let metadata = metadata.or(source_metadata.metadata);
            let message_count = entries
                .iter()
                .filter(|entry| entry.entry_type == "message")
                .count() as i64;

            let transaction = db
                .transaction()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            insert_session_row(
                &transaction,
                &NewSessionRow {
                    id: new_id.to_string(),
                    created_at,
                    cwd: cwd.to_string(),
                    parent_session_id: Some(parent_session_id.unwrap_or(source_id).to_string()),
                    metadata,
                },
            )
            .map_err(|error| session_error(&error))?;
            create_sequence(&transaction, new_id, 1)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            create_stats(&transaction, new_id, message_count)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;

            let mut next_seq: i64 = 1;
            for entry in &entries {
                insert_entry_row(
                    &transaction,
                    new_id,
                    &NewEntryRow {
                        seq: next_seq,
                        id: entry.id.clone(),
                        parent_id: entry.parent_id.clone(),
                        entry_type: entry.entry_type.clone(),
                        timestamp: entry.timestamp,
                        payload: entry.payload.clone(),
                    },
                )
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
                next_seq += 1;
            }

            if scope_is_tree(scope) {
                for (lane, leaf_id) in &lanes {
                    create_lane(&transaction, new_id, next_seq, lane, leaf_id.as_deref())
                        .map_err(|error| session_error(&error))?;
                    next_seq += 1;
                }
            } else {
                create_initial_lane(
                    &transaction,
                    new_id,
                    "main",
                    branch_fork_target_id.as_deref(),
                )
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            }

            if let Some(name) = latest_name.and_then(|row| row.value) {
                append_fact(&transaction, new_id, next_seq, "name", None, Some(&name)).map_err(
                    |error| SessionError::new(SessionErrorCode::Storage, error.to_string()),
                )?;
                next_seq += 1;
            }
            for (key, value) in &labels_to_copy {
                append_fact(
                    &transaction,
                    new_id,
                    next_seq,
                    "label",
                    Some(key),
                    Some(value),
                )
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
                next_seq += 1;
            }
            set_next_sequence(&transaction, new_id, next_seq)
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
            for tip in &branch_tips {
                build_cached_branch(&transaction, new_id, tip, &format!("branch-{tip}"))
                    .map_err(|error| session_error(&error))?;
            }
            let lease = claim_writer_lease(
                &transaction,
                new_id,
                &self.lease_options,
                &self.next_owner(),
            )?;
            transaction
                .commit()
                .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;

            let row = require_session_row(db, new_id)?;
            let decoded =
                decode_session_metadata(&row, "").map_err(|error| session_error(&error))?;
            self.active_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(new_id.to_string());
            let db = self.db.clone();
            let active_ids = self.active_sessions.clone();
            let session_id = decoded.id.clone();
            Ok(SqliteSessionStorage {
                db,
                on_release: Box::new(move || {
                    active_ids
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session_id);
                }),
                metadata: decoded,
                lease: Mutex::new(lease),
                released: std::sync::atomic::AtomicBool::new(false),
                lease_options: self.lease_options,
            })
        })
    }

    /// Close the repository, dropping the connection (upstream
    /// `close`; active storages release their own leases).
    pub fn close(&self) -> Result<(), SessionError> {
        *self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }
}

fn scope_is_tree(scope: &ForkOptions) -> bool {
    matches!(scope, ForkOptions::Tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_agent::harness::session::types::AgentMessage;
    use pillar_ai::types::{Content, Message, UserContent};
    use serde_json::json;

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::Message(Message::User {
            content: UserContent::Blocks(vec![Content::text(text)]),
            timestamp: 1,
        })
    }

    fn repo() -> SqliteSessionRepository {
        SqliteSessionRepository::open_in_memory(WriterLeaseOptions::default()).unwrap()
    }

    fn message_entry(id: &str, text: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.to_string(),
            payload: EntryPayload::Message {
                message: user_message(text),
                terminate: false,
            },
        }
    }

    fn started_record(id: &str, lane: &str) -> ProvisionedRecord {
        ProvisionedRecord {
            id: id.to_string(),
            lane: lane.to_string(),
            payload: RecordPayload::OperationStarted {
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: vec![],
                    initial_messages: vec![],
                    system_prompt_override: None,
                    resume_data: None,
                },
            },
        }
    }

    #[test]
    fn create_and_metadata_round_trip() {
        let repository = repo();
        let storage = repository
            .create("s1", "/tmp", None, Some(json!({"custom": 5})))
            .unwrap();
        let metadata = storage.get_metadata().unwrap();
        assert_eq!(metadata.id, "s1");
        assert!(metadata.parent_session_id.is_none());
        assert!(repository.list(None).unwrap().iter().any(|m| m.id == "s1"));
        // Duplicate create fails.
        assert_eq!(
            match repository.create("s1", "/tmp", None, None) {
                Ok(_) => panic!("duplicate create should fail"),
                Err(error) => error.code,
            },
            SessionErrorCode::AlreadyExists
        );
        storage.release().unwrap();
        repository.close().unwrap();
    }

    #[test]
    fn append_entry_assigns_chain_and_updates_cache() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        let first = storage
            .append_entry(message_entry("e1", "hello"), "main")
            .unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(first.parent_id, None);
        let second = storage
            .append_entry(message_entry("e2", "world"), "main")
            .unwrap();
        assert_eq!(second.parent_id.as_deref(), Some("e1"));
        assert_eq!(second.seq, 2);
        // Duplicate id rejected.
        assert_eq!(
            storage
                .append_entry(message_entry("e1", "again"), "main")
                .unwrap_err()
                .code,
            SessionErrorCode::AlreadyExists
        );
        // Branch reads see the path.
        let branch = storage
            .find_entries_on_branch("e2", &EntryQuery::default(), &BranchBounds::default())
            .unwrap();
        assert_eq!(
            branch.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["e1", "e2"]
        );
        // Message count in stats.
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.message_count, 2);
        storage.release().unwrap();
    }

    #[test]
    fn lanes_create_move_and_entries_on_fork() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        storage
            .append_entry(message_entry("e2", "b"), "main")
            .unwrap();
        storage.create_lane("topic", Some("e1")).unwrap();
        let lanes = storage.get_lanes().unwrap();
        assert_eq!(lanes.len(), 2);
        let topic = lanes.iter().find(|l| l.lane == "topic").unwrap();
        assert_eq!(topic.leaf_id.as_deref(), Some("e1"));
        // Appending on the forked lane branches from e1.
        let forked = storage
            .append_entry(message_entry("e3", "c"), "topic")
            .unwrap();
        assert_eq!(forked.parent_id.as_deref(), Some("e1"));
        // Move lane to a missing entry fails.
        assert_eq!(
            storage
                .move_lane("topic", Some("nope"))
                .map_err(|error| error.code)
                .expect_err("expected error"),
            SessionErrorCode::NotFound
        );
        storage.move_lane("topic", None).unwrap();
        // Duplicate lane creation fails.
        assert_eq!(
            storage
                .create_lane("main", None)
                .map_err(|error| error.code)
                .expect_err("expected error"),
            SessionErrorCode::AlreadyExists
        );
        storage.release().unwrap();
    }

    #[test]
    fn records_flow_lifecycle_and_usage_stats() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage.append_record(started_record("r1", "main")).unwrap();
        let open = storage.find_open_operations("main", None).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "r1");
        // Duplicate record id rejected.
        assert_eq!(
            storage
                .append_record(started_record("r1", "main"))
                .unwrap_err()
                .code,
            SessionErrorCode::AlreadyExists
        );
        storage
            .append_record(ProvisionedRecord {
                id: "r2".to_string(),
                lane: "main".to_string(),
                payload: RecordPayload::UsageRecord {
                    usage: Default::default(),
                    cause: "assistant".to_string(),
                    run_id: Some("r1".to_string()),
                    entry_id: None,
                    attempt: None,
                    tool_call_id: None,
                    stop_reason: None,
                    details: None,
                },
            })
            .unwrap();
        storage
            .append_record(ProvisionedRecord {
                id: "r3".to_string(),
                lane: "main".to_string(),
                payload: RecordPayload::OperationFinished {
                    run_id: "r1".to_string(),
                    outcome: pillar_agent::harness::session::types::OperationOutcome::Completed,
                    error: None,
                },
            })
            .unwrap();
        // Open operation cleared by finish.
        assert!(
            storage
                .find_open_operations("main", None)
                .unwrap()
                .is_empty()
        );
        let records = storage
            .find_records(&RecordQuery {
                lane: Some("main".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(records.len(), 3);
        // Unknown lane fails.
        assert_eq!(
            storage
                .append_record(started_record("r4", "nope"))
                .unwrap_err()
                .code,
            SessionErrorCode::InvalidLane
        );
        storage.release().unwrap();
    }

    #[test]
    fn log_merges_all_streams_in_seq_order() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage.set_name(Some("session-name".to_string())).unwrap();
        storage
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        storage.set_label("e1", Some("pinned".to_string())).unwrap();
        let log = storage.get_log(&LogOptions::default()).unwrap();
        // Sequence order: name fact (1), entry (2), label (3).
        assert_eq!(log.len(), 3);
        assert!(
            matches!(&log[0], LogItem::Name { name: Some(name), .. } if name == "session-name")
        );
        assert!(matches!(&log[1], LogItem::Entry { entry, .. } if entry.id == "e1"));
        assert!(
            matches!(&log[2], LogItem::Label { target_id, label, .. } if target_id == "e1" && label.as_deref() == Some("pinned"))
        );
        // Name/label getters decode the latest fact.
        assert_eq!(storage.get_name().unwrap().as_deref(), Some("session-name"));
        assert_eq!(storage.get_label("e1").unwrap().as_deref(), Some("pinned"));
        // Label on a missing entry fails.
        assert_eq!(
            storage
                .set_label("zz", Some("x".to_string()))
                .map_err(|error| error.code)
                .expect_err("expected error"),
            SessionErrorCode::NotFound
        );
        storage.release().unwrap();
    }

    #[test]
    fn fork_tree_copies_everything() {
        let repository = repo();
        let source = repository.create("s1", "/tmp", None, None).unwrap();
        source.set_name(Some("origin".to_string())).unwrap();
        source
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        source
            .append_entry(message_entry("e2", "b"), "main")
            .unwrap();
        source.release().unwrap();
        let forked = repository
            .fork("s1", "s2", &ForkOptions::Tree, "/tmp", None, None)
            .unwrap();
        let metadata = forked.get_metadata().unwrap();
        assert_eq!(metadata.parent_session_id.as_deref(), Some("s1"));
        let entries = forked.find_entries(&EntryQuery::default()).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["e2", "e1"]
        );
        assert_eq!(forked.get_name().unwrap().as_deref(), Some("origin"));
        // Stats carried the message count.
        assert_eq!(forked.get_stats().unwrap().message_count, 2);
        forked.release().unwrap();
    }

    #[test]
    fn fork_branch_copies_up_to_target_with_positions() {
        let repository = repo();
        let source = repository.create("s1", "/tmp", None, None).unwrap();
        source
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        source
            .append_entry(message_entry("e2", "b"), "main")
            .unwrap();
        source.release().unwrap();
        // Default branch fork from main leaf "at" e2 copies both entries.
        let forked = repository
            .fork(
                "s1",
                "s2",
                &ForkOptions::Branch {
                    entry_id: None,
                    position: None,
                },
                "/tmp",
                None,
                None,
            )
            .unwrap();
        let entries = forked.find_entries(&EntryQuery::default()).unwrap();
        assert_eq!(entries.len(), 2);
        forked.release().unwrap();
        // "before" e2 copies only e1; non-message targets fail.
        let before = repository
            .fork(
                "s1",
                "s3",
                &ForkOptions::Branch {
                    entry_id: Some("e2".to_string()),
                    position: Some(ForkPosition::Before),
                },
                "/tmp",
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            before
                .find_entries(&EntryQuery::default())
                .unwrap()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["e1"]
        );
        before.release().unwrap();
    }

    #[test]
    fn delete_removes_all_rows_and_missing_is_noop() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        storage.set_name(Some("n".to_string())).unwrap();
        storage.release().unwrap();
        repository.delete("s1").unwrap();
        assert!(repository.list(None).unwrap().is_empty());
        // Deleting a missing session is a no-op.
        repository.delete("gone").unwrap();
    }

    #[test]
    fn second_writer_is_refused_until_release() {
        let repository = repo();
        let first = repository.create("s1", "/tmp", None, None).unwrap();
        let metadata = first.get_metadata().unwrap();
        assert_eq!(
            match repository.open(&repository.list(None).unwrap()[0]) {
                Ok(_) => panic!("second writer should be refused"),
                Err(error) => error.code,
            },
            SessionErrorCode::Storage
        );
        first.release().unwrap();
        let _second = repository
            .open(&SqliteSessionMetadata {
                id: metadata.id,
                created_at: metadata.created_at as i64,
                name: None,
                cwd: "/tmp".to_string(),
                path: String::new(),
                parent_session_id: None,
                metadata: None,
            })
            .unwrap();
    }

    #[test]
    fn repair_branch_cache_rebuilds_from_entries() {
        let repository = repo();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage
            .append_entry(message_entry("e1", "a"), "main")
            .unwrap();
        storage
            .append_entry(message_entry("e2", "b"), "main")
            .unwrap();
        storage.release().unwrap();
        // Wipe the cache behind the facade, then repair.
        {
            let guard = repository.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            crate::branch_cache::delete_branch_cache(db, "s1").unwrap();
        }
        repository.repair_branch_cache("s1").unwrap();
        let reopened = repository.open(&repository.list(None).unwrap()[0]).unwrap();
        let branch = reopened
            .find_entries_on_branch("e2", &EntryQuery::default(), &BranchBounds::default())
            .unwrap();
        assert_eq!(
            branch.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["e1", "e2"]
        );
        reopened.release().unwrap();
    }
}
