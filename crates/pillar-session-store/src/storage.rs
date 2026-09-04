//! Port of packages/session-backends/sqlite-node/src/sqlite/storage
//! entries.ts + lanes.ts + facts.ts + records.ts + writer-leases.ts +
//! session-sequences.ts + session-stats.ts + sessions.ts (pi v0.84.3,
//! 633 lines): the canonical per-table SQL layer behind
//! [`crate::branch_cache`].
//!
//! divergences: SessionError becomes [`StorageError`] (same kind
//! strings); `INDEXED BY` hints are dropped (SQLite may pick
//! equivalent plans); upstream `assertJsonSerializable` becomes a
//! serde_json round-trip check; usage stats take explicit component
//! fields instead of the full Usage struct.

use rusqlite::Connection;

use crate::branch_cache::StorageError;

/// A canonical entry row (upstream `EntryRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct EntryRow {
    pub session_id: String,
    pub seq: i64,
    pub id: String,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

/// Insert payload for a new entry (upstream `NewEntryRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewEntryRow {
    pub seq: i64,
    pub id: String,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

/// Upstream `entryPayload`: the entry object minus the base fields.
/// The base fields are carried in dedicated columns, so callers pass
/// the remaining payload JSON here.
pub fn insert_entry_row(
    db: &Connection,
    session_id: &str,
    entry: &NewEntryRow,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO entries (session_id, id, seq, parent_id, type, timestamp, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            session_id,
            entry.id,
            entry.seq,
            entry.parent_id,
            entry.entry_type,
            entry.timestamp,
            entry.payload
        ],
    )
    .map(|_| ())
}

pub fn read_entry_row(db: &Connection, session_id: &str, entry_id: &str) -> Option<EntryRow> {
    read_entry_rows(
        db,
        session_id,
        &EntryRowsQuery {
            id: Some(entry_id.to_string()),
            ..Default::default()
        },
    )
    .ok()
    .and_then(|rows| rows.into_iter().next())
}

/// Filters for [`read_entry_rows`] (upstream the inline options of
/// `readEntryRows` plus the single-id lookup).
#[derive(Debug, Clone, Default)]
pub struct EntryRowsQuery {
    pub id: Option<String>,
    pub after_seq: Option<i64>,
    pub cursor_after_seq: Option<i64>,
    pub entry_type: Option<String>,
    pub oldest_first: bool,
    pub limit: Option<usize>,
}

pub fn read_entry_rows(
    db: &Connection,
    session_id: &str,
    query: &EntryRowsQuery,
) -> rusqlite::Result<Vec<EntryRow>> {
    let oldest_first = query.oldest_first;
    let mut predicates = vec!["session_id = ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(session_id.to_string())];
    if let Some(id) = &query.id {
        params.push(Box::new(id.clone()));
        predicates.push(format!("id = ?{}", params.len()));
    }
    if let Some(after_seq) = query.after_seq {
        params.push(Box::new(after_seq));
        predicates.push(format!("seq > ?{}", params.len()));
    }
    if let Some(cursor) = query.cursor_after_seq {
        params.push(Box::new(cursor));
        predicates.push(format!(
            "seq {} ?{}",
            if oldest_first { ">" } else { "<" },
            params.len()
        ));
    }
    if let Some(entry_type) = &query.entry_type {
        params.push(Box::new(entry_type.clone()));
        predicates.push(format!("type = ?{}", params.len()));
    }
    let direction = if oldest_first { "ASC" } else { "DESC" };
    let limit = query
        .limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT session_id, seq, id, parent_id, type, timestamp, payload
         FROM entries
         WHERE {}
         ORDER BY seq {direction}{limit}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let references: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(references.as_slice(), map_entry_row)?;
    rows.collect()
}

fn map_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        id: row.get(2)?,
        parent_id: row.get(3)?,
        entry_type: row.get(4)?,
        timestamp: row.get(5)?,
        payload: row.get(6)?,
    })
}

pub fn id_exists_in_entries(db: &Connection, session_id: &str, id: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM entries WHERE session_id = ?1 AND id = ?2 LIMIT 1",
        rusqlite::params![session_id, id],
        |_| Ok(()),
    )
    .is_ok()
}

pub fn delete_entry_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM entries WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// lanes.ts
// ============================================================================

/// A lane row (upstream `LaneRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneRow {
    pub session_id: String,
    pub lane: String,
    pub leaf_id: Option<String>,
    pub open_operation_id: Option<String>,
}

/// A lane-move history row (upstream `LaneMoveRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneMoveRow {
    pub session_id: String,
    pub seq: i64,
    pub lane: String,
    pub leaf_id: Option<String>,
}

pub fn create_initial_lane(
    db: &Connection,
    session_id: &str,
    lane: &str,
    leaf_id: Option<&str>,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO lanes (session_id, lane, leaf_id, open_operation_id) VALUES (?1, ?2, ?3, NULL)",
        rusqlite::params![session_id, lane, leaf_id],
    )
    .map(|_| ())
}

/// Read all lanes, failing when a leaf pointer dangles (upstream
/// `readLanes`).
pub fn read_lanes(db: &Connection, session_id: &str) -> Result<Vec<LaneRow>, StorageError> {
    let mut statement = db
        .prepare(
            "SELECT l.session_id, l.lane, l.leaf_id, l.open_operation_id,
                (l.leaf_id IS NULL OR EXISTS (
                    SELECT 1 FROM entries AS e WHERE e.session_id = l.session_id AND e.id = l.leaf_id
                )) AS leaf_exists
            FROM lanes AS l
            WHERE l.session_id = ?1
            ORDER BY l.lane",
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    let rows = statement
        .query_map(rusqlite::params![session_id], |row| {
            Ok((
                LaneRow {
                    session_id: row.get(0)?,
                    lane: row.get(1)?,
                    leaf_id: row.get(2)?,
                    open_operation_id: row.get(3)?,
                },
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| StorageError::new("storage", error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    for (row, leaf_exists) in &rows {
        if *leaf_exists == 0 {
            return Err(StorageError::new(
                "storage",
                format!(
                    "Lane {} points at missing entry {}",
                    row.lane,
                    row.leaf_id.clone().unwrap_or_default()
                ),
            ));
        }
    }
    Ok(rows.into_iter().map(|(row, _)| row).collect())
}

pub fn read_lane(db: &Connection, session_id: &str, lane: &str) -> Option<LaneRow> {
    db.query_row(
        "SELECT session_id, lane, leaf_id, open_operation_id
         FROM lanes
         WHERE session_id = ?1 AND lane = ?2",
        rusqlite::params![session_id, lane],
        |row| {
            Ok(LaneRow {
                session_id: row.get(0)?,
                lane: row.get(1)?,
                leaf_id: row.get(2)?,
                open_operation_id: row.get(3)?,
            })
        },
    )
    .ok()
}

/// Read a lane's leaf, validating the lane and its pointer (upstream
/// `readLaneHead`).
pub fn read_lane_head(
    db: &Connection,
    session_id: &str,
    lane: &str,
) -> Result<Option<String>, StorageError> {
    let row = db
        .query_row(
            "SELECT l.leaf_id,
                (l.leaf_id IS NULL OR EXISTS (
                    SELECT 1 FROM entries AS e WHERE e.session_id = l.session_id AND e.id = l.leaf_id
                )) AS leaf_exists
            FROM lanes AS l
            WHERE l.session_id = ?1 AND l.lane = ?2",
            rusqlite::params![session_id, lane],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|_| StorageError::new("invalid_lane", format!("Lane not found: {lane}")))?;
    let (leaf_id, leaf_exists) = row;
    if leaf_exists == 0 {
        return Err(StorageError::new(
            "storage",
            format!("Entry {} not found", leaf_id.clone().unwrap_or_default()),
        ));
    }
    Ok(leaf_id)
}

pub fn create_lane(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> Result<(), StorageError> {
    create_initial_lane(db, session_id, lane, leaf_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    append_lane_move(db, session_id, seq, lane, leaf_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))
}

pub fn move_lane(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> Result<(), StorageError> {
    let changed = db
        .execute(
            "UPDATE lanes SET leaf_id = ?3 WHERE session_id = ?1 AND lane = ?2",
            rusqlite::params![session_id, lane, leaf_id],
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    if changed != 1 {
        return Err(StorageError::new(
            "invalid_lane",
            format!("Lane not found: {lane}"),
        ));
    }
    append_lane_move(db, session_id, seq, lane, leaf_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))
}

pub fn set_lane_leaf(
    db: &Connection,
    session_id: &str,
    lane: &str,
    leaf_id: Option<&str>,
) -> Result<(), StorageError> {
    let changed = db
        .execute(
            "UPDATE lanes SET leaf_id = ?3 WHERE session_id = ?1 AND lane = ?2",
            rusqlite::params![session_id, lane, leaf_id],
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    if changed != 1 {
        return Err(StorageError::new(
            "invalid_lane",
            format!("Lane not found: {lane}"),
        ));
    }
    Ok(())
}

/// Claim a lane for a run (upstream `startLaneOperation`): fails when
/// the lane is missing or already has an open operation.
pub fn start_lane_operation(
    db: &Connection,
    session_id: &str,
    lane: &str,
    run_id: &str,
) -> Result<(), StorageError> {
    let changed = db
        .execute(
            "UPDATE lanes SET open_operation_id = ?3
             WHERE session_id = ?1 AND lane = ?2 AND open_operation_id IS NULL",
            rusqlite::params![session_id, lane, run_id],
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    if changed == 1 {
        return Ok(());
    }
    let current = read_lane(db, session_id, lane)
        .ok_or_else(|| StorageError::new("invalid_lane", format!("Lane not found: {lane}")))?;
    Err(StorageError::new(
        "storage",
        format!(
            "Lane {lane} already has an open operation {}",
            current.open_operation_id.unwrap_or_default()
        ),
    ))
}

pub fn finish_lane_operation(
    db: &Connection,
    session_id: &str,
    lane: &str,
    run_id: &str,
) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE lanes SET open_operation_id = NULL
         WHERE session_id = ?1 AND lane = ?2 AND open_operation_id = ?3",
        rusqlite::params![session_id, lane, run_id],
    )
    .map(|_| ())
}

pub fn read_lane_move_rows(
    db: &Connection,
    session_id: &str,
    after_seq: Option<i64>,
    limit: Option<usize>,
) -> rusqlite::Result<Vec<LaneMoveRow>> {
    let mut predicates = vec!["session_id = ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(session_id.to_string())];
    if let Some(after_seq) = after_seq {
        params.push(Box::new(after_seq));
        predicates.push(format!("seq > ?{}", params.len()));
    }
    let limit = limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT session_id, seq, lane, leaf_id
         FROM lane_moves
         WHERE {}
         ORDER BY seq{limit}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let references: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(references.as_slice(), |row| {
        Ok(LaneMoveRow {
            session_id: row.get(0)?,
            seq: row.get(1)?,
            lane: row.get(2)?,
            leaf_id: row.get(3)?,
        })
    })?;
    rows.collect()
}

pub fn delete_lane_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM lane_moves WHERE session_id = ?1",
        rusqlite::params![session_id],
    )?;
    db.execute(
        "DELETE FROM lanes WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

fn append_lane_move(
    db: &Connection,
    session_id: &str,
    seq: i64,
    lane: &str,
    leaf_id: Option<&str>,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO lane_moves (session_id, seq, lane, leaf_id) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session_id, seq, lane, leaf_id],
    )
    .map(|_| ())
}

// ============================================================================
// facts.ts
// ============================================================================

/// A fact row (upstream `FactRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct FactRow {
    pub session_id: String,
    pub seq: i64,
    pub kind: String,
    pub key: Option<String>,
    pub value: Option<String>,
}

pub fn append_fact(
    db: &Connection,
    session_id: &str,
    seq: i64,
    kind: &str,
    key: Option<&str>,
    value: Option<&str>,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO facts (session_id, seq, kind, key, value) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![session_id, seq, kind, key, value],
    )
    .map(|_| ())
}

/// Read the newest fact for a kind/key, where a NULL key matches only
/// NULL keys (upstream `readLatestFact` — `key IS ${key}`).
pub fn read_latest_fact(
    db: &Connection,
    session_id: &str,
    kind: &str,
    key: Option<&str>,
) -> Option<FactRow> {
    db.query_row(
        "SELECT session_id, seq, kind, key, value
         FROM facts
         WHERE session_id = ?1 AND kind = ?2 AND key IS ?3
         ORDER BY seq DESC
         LIMIT 1",
        rusqlite::params![session_id, kind, key],
        |row| {
            Ok(FactRow {
                session_id: row.get(0)?,
                seq: row.get(1)?,
                kind: row.get(2)?,
                key: row.get(3)?,
                value: row.get(4)?,
            })
        },
    )
    .ok()
}

/// Read the latest non-null label fact per key (upstream
/// `readLatestLabelFacts`).
pub fn read_latest_label_facts(
    db: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = db.prepare(
        "SELECT f.key, f.value
         FROM facts AS f
         WHERE f.session_id = ?1
             AND f.kind = 'label'
             AND f.value IS NOT NULL
             AND f.seq = (
                SELECT MAX(candidate.seq)
                FROM facts AS candidate
                WHERE candidate.session_id = f.session_id
                    AND candidate.kind = f.kind
                    AND candidate.key IS f.key
             )
         ORDER BY f.key",
    )?;
    let rows = statement.query_map(rusqlite::params![session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

pub fn read_fact_rows(
    db: &Connection,
    session_id: &str,
    after_seq: Option<i64>,
    limit: Option<usize>,
) -> rusqlite::Result<Vec<FactRow>> {
    let mut predicates = vec!["session_id = ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(session_id.to_string())];
    if let Some(after_seq) = after_seq {
        params.push(Box::new(after_seq));
        predicates.push(format!("seq > ?{}", params.len()));
    }
    let limit = limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT session_id, seq, kind, key, value
         FROM facts
         WHERE {}
         ORDER BY seq{limit}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let references: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(references.as_slice(), |row| {
        Ok(FactRow {
            session_id: row.get(0)?,
            seq: row.get(1)?,
            kind: row.get(2)?,
            key: row.get(3)?,
            value: row.get(4)?,
        })
    })?;
    rows.collect()
}

pub fn delete_fact_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM facts WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// records.ts
// ============================================================================

/// A lane record row (upstream `RecordRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordRow {
    pub session_id: String,
    pub seq: i64,
    pub id: String,
    pub lane: String,
    pub run_id: Option<String>,
    pub record_type: String,
    pub op_kind: Option<String>,
    pub timestamp: i64,
    pub payload: String,
}

/// Insert payload for a new record (upstream `NewRecordRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewRecordRow {
    pub seq: i64,
    pub id: String,
    pub lane: String,
    pub run_id: Option<String>,
    pub record_type: String,
    pub op_kind: Option<String>,
    pub timestamp: i64,
    pub payload: String,
}

pub fn append_record_row(
    db: &Connection,
    session_id: &str,
    record: &NewRecordRow,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO records
            (session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            session_id,
            record.seq,
            record.id,
            record.lane,
            record.run_id,
            record.record_type,
            record.op_kind,
            record.timestamp,
            record.payload
        ],
    )
    .map(|_| ())
}

pub fn id_exists_in_records(db: &Connection, session_id: &str, id: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM records WHERE session_id = ?1 AND id = ?2 LIMIT 1",
        rusqlite::params![session_id, id],
        |_| Ok(()),
    )
    .is_ok()
}

pub fn delete_record_rows(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM records WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

/// Filters for [`read_record_rows`] (upstream the inline query of
/// `readRecordRows`).
#[derive(Debug, Clone, Default)]
pub struct RecordRowsQuery {
    pub lane: Option<String>,
    pub record_type: Option<String>,
    pub run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub after_seq: Option<i64>,
    pub oldest_first: bool,
    pub limit: Option<usize>,
}

pub fn read_record_rows(
    db: &Connection,
    session_id: &str,
    query: &RecordRowsQuery,
) -> rusqlite::Result<Vec<RecordRow>> {
    let mut predicates = vec!["session_id = ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(session_id.to_string())];
    if let Some(lane) = &query.lane {
        params.push(Box::new(lane.clone()));
        predicates.push(format!("lane = ?{}", params.len()));
    }
    if let Some(record_type) = &query.record_type {
        params.push(Box::new(record_type.clone()));
        predicates.push(format!("type = ?{}", params.len()));
    }
    if let Some(run_id) = &query.run_id {
        params.push(Box::new(run_id.clone()));
        predicates.push(format!("run_id = ?{}", params.len()));
    }
    if let Some(operation_kind) = &query.operation_kind {
        params.push(Box::new(operation_kind.clone()));
        predicates.push(format!("op_kind = ?{}", params.len()));
    }
    if let Some(after_seq) = query.after_seq {
        params.push(Box::new(after_seq));
        predicates.push(format!("seq > ?{}", params.len()));
    }
    let direction = if query.oldest_first { "ASC" } else { "DESC" };
    let limit = query
        .limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload
         FROM records
         WHERE {}
         ORDER BY seq {direction}{limit}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let references: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(references.as_slice(), map_record_row)?;
    rows.collect()
}

fn map_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordRow> {
    Ok(RecordRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        id: row.get(2)?,
        lane: row.get(3)?,
        run_id: row.get(4)?,
        record_type: row.get(5)?,
        op_kind: row.get(6)?,
        timestamp: row.get(7)?,
        payload: row.get(8)?,
    })
}

/// Read the open operation record for a lane, if any (upstream
/// `readOpenOperationRows`): validates the record's lane and type.
pub fn read_open_operation_rows(
    db: &Connection,
    session_id: &str,
    lane: &str,
) -> Result<Vec<RecordRow>, StorageError> {
    let open_operation_id: Option<Option<String>> = db
        .query_row(
            "SELECT open_operation_id FROM lanes WHERE session_id = ?1 AND lane = ?2",
            rusqlite::params![session_id, lane],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            error => Err(error),
        })
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    let Some(Some(open_operation_id)) = open_operation_id else {
        return Ok(Vec::new());
    };
    let record = db
        .query_row(
            "SELECT session_id, seq, id, lane, run_id, type, op_kind, timestamp, payload
             FROM records
             WHERE session_id = ?1 AND id = ?2",
            rusqlite::params![session_id, open_operation_id],
            map_record_row,
        )
        .map_err(|_| {
            StorageError::new(
                "storage",
                format!("Lane {lane} points at missing open operation {open_operation_id}"),
            )
        })?;
    if record.lane != lane || record.record_type != "operation_started" {
        return Err(StorageError::new(
            "storage",
            format!("Lane {lane} points at invalid open operation {open_operation_id}"),
        ));
    }
    Ok(vec![record])
}

// ============================================================================
// writer-leases.ts
// ============================================================================

/// An acquired writer lease (upstream `WriterLease`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLease {
    pub owner_id: String,
    pub fence: i64,
    pub expires_at_ms: i64,
}

/// Acquire the session's writer lease, fencing out expired holders
/// (upstream `acquireWriterLease`): the conditional upsert bumps the
/// fence only when the previous lease expired.
pub fn acquire_writer_lease(
    db: &Connection,
    session_id: &str,
    owner_id: &str,
    now: i64,
    expires_at_ms: i64,
) -> Option<WriterLease> {
    let mut statement = db
        .prepare(
            "INSERT INTO writer_leases (session_id, owner_id, fence, expires_at_ms)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                owner_id = excluded.owner_id,
                fence = writer_leases.fence + 1,
                expires_at_ms = excluded.expires_at_ms
             WHERE writer_leases.expires_at_ms <= ?4
             RETURNING owner_id, fence, expires_at_ms",
        )
        .ok()?;
    statement
        .query_row(
            rusqlite::params![session_id, owner_id, expires_at_ms, now],
            |row| {
                Ok(WriterLease {
                    owner_id: row.get(0)?,
                    fence: row.get(1)?,
                    expires_at_ms: row.get(2)?,
                })
            },
        )
        .ok()
}

/// Renew a lease (upstream `renewWriterLease`): only the current
/// owner at the current fence can extend, and only while unexpired.
/// Returns whether the renewal landed; on success the lease's expiry
/// is updated in place.
pub fn renew_writer_lease(
    db: &Connection,
    session_id: &str,
    lease: &mut WriterLease,
    now: i64,
    expires_at_ms: i64,
) -> bool {
    let changed = db
        .execute(
            "UPDATE writer_leases
             SET expires_at_ms = ?4
             WHERE session_id = ?1
                AND owner_id = ?2
                AND fence = ?3
                AND expires_at_ms > ?5",
            rusqlite::params![session_id, lease.owner_id, lease.fence, expires_at_ms, now],
        )
        .unwrap_or(0);
    if changed == 1 {
        lease.expires_at_ms = expires_at_ms;
    }
    changed == 1
}

pub fn release_writer_lease(
    db: &Connection,
    session_id: &str,
    lease: &WriterLease,
) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM writer_leases
         WHERE session_id = ?1 AND owner_id = ?2 AND fence = ?3",
        rusqlite::params![session_id, lease.owner_id, lease.fence],
    )
    .map(|_| ())
}

pub fn delete_writer_lease(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM writer_leases WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// session-sequences.ts
// ============================================================================

pub fn create_sequence(db: &Connection, session_id: &str, next_seq: i64) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO session_sequences (session_id, next_seq) VALUES (?1, ?2)",
        rusqlite::params![session_id, next_seq],
    )
    .map(|_| ())
}

pub fn get_next_sequence(db: &Connection, session_id: &str) -> Result<i64, StorageError> {
    db.query_row(
        "SELECT next_seq FROM session_sequences WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| row.get(0),
    )
    .map_err(|_| {
        StorageError::new(
            "storage",
            format!("Missing sequence row for session {session_id}"),
        )
    })
}

pub fn set_next_sequence(db: &Connection, session_id: &str, next_seq: i64) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE session_sequences SET next_seq = ?2 WHERE session_id = ?1",
        rusqlite::params![session_id, next_seq],
    )
    .map(|_| ())
}

pub fn advance_sequence(db: &Connection, session_id: &str, seq: i64) -> rusqlite::Result<()> {
    set_next_sequence(db, session_id, seq + 1)
}

pub fn delete_sequence(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM session_sequences WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// session-stats.ts
// ============================================================================

/// A stats row (upstream `SessionStatsRow`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStatsRow {
    pub session_id: String,
    pub message_count: i64,
    pub cached_tokens: f64,
    pub uncached_tokens: f64,
    pub total_tokens: f64,
    pub cost_total: f64,
}

pub fn create_stats(db: &Connection, session_id: &str, message_count: i64) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO session_stats
            (session_id, message_count, cached_tokens, uncached_tokens, total_tokens, cost_total)
            VALUES (?1, ?2, 0, 0, 0, 0)",
        rusqlite::params![session_id, message_count],
    )
    .map(|_| ())
}

/// Read stats as decoded component fields (upstream `readStats`
/// returning `SessionStats`).
pub fn read_stats(db: &Connection, session_id: &str) -> Result<SessionStatsRow, StorageError> {
    db.query_row(
        "SELECT session_id, message_count, cached_tokens, uncached_tokens, total_tokens, cost_total
         FROM session_stats
         WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| {
            Ok(SessionStatsRow {
                session_id: row.get(0)?,
                message_count: row.get(1)?,
                cached_tokens: row.get(2)?,
                uncached_tokens: row.get(3)?,
                total_tokens: row.get(4)?,
                cost_total: row.get(5)?,
            })
        },
    )
    .map_err(|_| {
        StorageError::new(
            "storage",
            format!("Missing stats row for session {session_id}"),
        )
    })
}

pub fn increment_message_count(db: &Connection, session_id: &str) -> Result<(), StorageError> {
    let changed = db
        .execute(
            "UPDATE session_stats SET message_count = message_count + 1 WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    if changed != 1 {
        return Err(StorageError::new(
            "storage",
            format!("Missing stats row for session {session_id}"),
        ));
    }
    Ok(())
}

/// Add usage components to the stats totals (upstream `addUsageToStats`
/// with a Usage argument).
pub fn add_usage_to_stats(
    db: &Connection,
    session_id: &str,
    cache_read: u64,
    uncached_tokens: u64,
    total_tokens: u64,
    cost_total: f64,
) -> Result<(), StorageError> {
    let changed = db
        .execute(
            "UPDATE session_stats
             SET cached_tokens = cached_tokens + ?2,
                uncached_tokens = uncached_tokens + ?3,
                total_tokens = total_tokens + ?4,
                cost_total = cost_total + ?5
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                cache_read,
                uncached_tokens,
                total_tokens,
                cost_total
            ],
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    if changed != 1 {
        return Err(StorageError::new(
            "storage",
            format!("Missing stats row for session {session_id}"),
        ));
    }
    Ok(())
}

pub fn delete_stats(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM session_stats WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// sessions.ts
// ============================================================================

/// A session row (upstream `SessionRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub created_at: i64,
    pub metadata: Option<String>,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub has_session_name: bool,
    pub session_name: Option<String>,
}

/// Insert payload for a new session (upstream `NewSessionRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewSessionRow {
    pub id: String,
    pub created_at: i64,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Parse metadata JSON, requiring an object (upstream
/// `parseMetadata`).
fn parse_metadata(
    metadata: Option<&str>,
    session_id: &str,
) -> Result<Option<serde_json::Value>, StorageError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let parsed: serde_json::Value = serde_json::from_str(metadata).map_err(|_| {
        StorageError::new(
            "storage",
            format!("Invalid SQLite session {session_id}: metadata is not valid JSON"),
        )
    })?;
    if !parsed.is_object() {
        return Err(StorageError::new(
            "storage",
            format!("Invalid SQLite session {session_id}: metadata must be an object"),
        ));
    }
    Ok(Some(parsed))
}

pub fn session_exists(db: &Connection, session_id: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Serialize metadata, requiring an object (upstream
/// `serializeMetadata` + `assertJsonSerializable`).
fn serialize_metadata(
    metadata: Option<&serde_json::Value>,
) -> Result<Option<String>, StorageError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    if !metadata.is_object() {
        return Err(StorageError::new(
            "invalid_payload",
            "SQLite session metadata must be an object",
        ));
    }
    // Round-trip as the JSON-serializability check.
    let text = serde_json::to_string(metadata).map_err(|_| {
        StorageError::new(
            "invalid_payload",
            "SQLite session metadata is not JSON-serializable",
        )
    })?;
    Ok(Some(text))
}

pub fn insert_session_row(db: &Connection, session: &NewSessionRow) -> Result<(), StorageError> {
    let metadata = serialize_metadata(session.metadata.as_ref())?;
    db.execute(
        "INSERT INTO sessions (id, created_at, metadata, cwd, parent_session_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session.id,
            session.created_at,
            metadata,
            session.cwd,
            session.parent_session_id
        ],
    )
    .map_err(|error| StorageError::new("storage", error.to_string()))?;
    Ok(())
}

const SESSION_ROW_SELECT: &str =
    "SELECT s.id, s.created_at, s.metadata, s.cwd, s.parent_session_id,
        name_fact.seq IS NOT NULL AS has_session_name,
        name_fact.value AS session_name
    FROM sessions AS s
    LEFT JOIN facts AS name_fact
        ON name_fact.session_id = s.id
        AND name_fact.kind = 'name'
        AND name_fact.key IS NULL
        AND name_fact.seq = (
            SELECT MAX(f.seq)
            FROM facts AS f
            WHERE f.session_id = s.id AND f.kind = 'name' AND f.key IS NULL
        )";

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        created_at: row.get(1)?,
        metadata: row.get(2)?,
        cwd: row.get(3)?,
        parent_session_id: row.get(4)?,
        has_session_name: row.get::<_, i64>(5)? != 0,
        session_name: row.get(6)?,
    })
}

pub fn read_session_row(db: &Connection, session_id: &str) -> Option<SessionRow> {
    let sql = format!("{SESSION_ROW_SELECT} WHERE s.id = ?1");
    db.query_row(&sql, rusqlite::params![session_id], map_session_row)
        .ok()
}

/// Read all sessions, newest first, optionally scoped to a cwd
/// (upstream `readSessionRows`).
pub fn read_session_rows(db: &Connection, cwd: Option<&str>) -> rusqlite::Result<Vec<SessionRow>> {
    let sql = match cwd {
        Some(_) => format!("{SESSION_ROW_SELECT} WHERE s.cwd = ?1 ORDER BY s.created_at DESC"),
        None => format!("{SESSION_ROW_SELECT} ORDER BY s.created_at DESC"),
    };
    let mut statement = db.prepare(&sql)?;
    let cwd_owned = cwd.map(|value| value.to_string());
    let references: Vec<&dyn rusqlite::ToSql> = match &cwd_owned {
        Some(cwd) => vec![cwd as &dyn rusqlite::ToSql],
        None => vec![],
    };
    let rows = statement.query_map(references.as_slice(), map_session_row)?;
    rows.collect()
}

pub fn delete_session_row(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

/// Parse a name fact value, requiring a JSON string (upstream
/// `parseSessionName`).
fn parse_session_name(
    value: Option<&str>,
    session_id: &str,
) -> Result<Option<String>, StorageError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| {
        StorageError::new(
            "storage",
            format!("Invalid SQLite session {session_id}: name is not valid JSON"),
        )
    })?;
    let Some(parsed) = parsed.as_str() else {
        return Err(StorageError::new(
            "storage",
            format!("Invalid SQLite session {session_id}: name must be a string"),
        ));
    };
    Ok(Some(parsed.to_string()))
}

/// Decoded session metadata (upstream `SqliteSessionMetadata`).
#[derive(Debug, Clone, PartialEq)]
pub struct SqliteSessionMetadata {
    pub id: String,
    pub created_at: i64,
    pub name: Option<String>,
    pub cwd: String,
    pub path: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Decode a session row (upstream `decodeSessionMetadata`).
pub fn decode_session_metadata(
    row: &SessionRow,
    path: &str,
) -> Result<SqliteSessionMetadata, StorageError> {
    let metadata = parse_metadata(row.metadata.as_deref(), &row.id)?;
    let name = if row.has_session_name {
        parse_session_name(row.session_name.as_deref(), &row.id)?
    } else {
        None
    };
    Ok(SqliteSessionMetadata {
        id: row.id.clone(),
        created_at: row.created_at,
        name,
        cwd: row.cwd.clone(),
        path: path.to_string(),
        parent_session_id: row.parent_session_id.clone(),
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        crate::migrations::apply_migrations(&mut db).unwrap();
        db
    }

    #[test]
    fn entries_round_trip_and_filters() {
        let db = setup_db();
        db.execute(
            "INSERT INTO sessions (id, created_at, cwd) VALUES ('s', 1, '/tmp')",
            [],
        )
        .unwrap();
        for (seq, id, parent, kind) in [
            (0, "e1", None, "message"),
            (1, "e2", Some("e1"), "compaction"),
            (2, "e3", Some("e2"), "message"),
        ] {
            insert_entry_row(
                &db,
                "s",
                &NewEntryRow {
                    seq,
                    id: id.to_string(),
                    parent_id: parent.map(|p| p.to_string()),
                    entry_type: kind.to_string(),
                    timestamp: 100 + seq,
                    payload: "{}".to_string(),
                },
            )
            .unwrap();
        }
        assert_eq!(
            read_entry_row(&db, "s", "e2").unwrap().entry_type,
            "compaction"
        );
        // Newest first by default; typed filter.
        let newest = read_entry_rows(&db, "s", &EntryRowsQuery::default()).unwrap();
        assert_eq!(newest[0].id, "e3");
        let typed = read_entry_rows(
            &db,
            "s",
            &EntryRowsQuery {
                entry_type: Some("message".to_string()),
                oldest_first: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            typed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["e1", "e3"]
        );
        // Cursor on newest-first keeps only e1.
        let cursor = read_entry_rows(
            &db,
            "s",
            &EntryRowsQuery {
                cursor_after_seq: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cursor.len(), 2);
        // Id existence and deletion.
        assert!(id_exists_in_entries(&db, "s", "e1"));
        assert!(!id_exists_in_entries(&db, "s", "zz"));
        delete_entry_rows(&db, "s").unwrap();
        assert_eq!(
            read_entry_rows(&db, "s", &EntryRowsQuery::default())
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn lanes_validate_pointers_and_history() {
        let db = setup_db();
        db.execute(
            "INSERT INTO sessions (id, created_at, cwd) VALUES ('s', 1, '/tmp')",
            [],
        )
        .unwrap();
        insert_entry_row(
            &db,
            "s",
            &NewEntryRow {
                seq: 0,
                id: "e1".to_string(),
                parent_id: None,
                entry_type: "message".to_string(),
                timestamp: 1,
                payload: "{}".to_string(),
            },
        )
        .unwrap();
        create_initial_lane(&db, "s", "main", Some("e1")).unwrap();
        assert_eq!(
            read_lane_head(&db, "s", "main").unwrap(),
            Some("e1".to_string())
        );
        // Move records history.
        move_lane(&db, "s", 1, "main", None).unwrap();
        let moves = read_lane_move_rows(&db, "s", None, None).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].seq, 1);
        assert_eq!(moves[0].leaf_id, None);
        // Missing lane fails on move and head.
        assert_eq!(
            move_lane(&db, "s", 2, "nope", None).unwrap_err().kind,
            "invalid_lane"
        );
        assert_eq!(
            read_lane_head(&db, "s", "nope").unwrap_err().kind,
            "invalid_lane"
        );
        // Dangling leaf fails on readLanes and readLaneHead.
        db.execute("UPDATE lanes SET leaf_id='gone' WHERE lane='main'", [])
            .unwrap();
        assert_eq!(read_lanes(&db, "s").unwrap_err().kind, "storage");
        assert_eq!(
            read_lane_head(&db, "s", "main").unwrap_err().kind,
            "storage"
        );
        // Operations.
        start_lane_operation(&db, "s", "main", "r1").unwrap();
        assert_eq!(
            start_lane_operation(&db, "s", "main", "r2")
                .unwrap_err()
                .kind,
            "storage"
        );
        finish_lane_operation(&db, "s", "main", "r1").unwrap();
        start_lane_operation(&db, "s", "main", "r2").unwrap();
        delete_lane_rows(&db, "s").unwrap();
        assert_eq!(read_lanes(&db, "s").unwrap().len(), 0);
    }

    #[test]
    fn facts_latest_and_labels() {
        let db = setup_db();
        append_fact(&db, "s", 0, "name", None, Some(r#""first""#)).unwrap();
        append_fact(&db, "s", 1, "name", None, Some(r#""second""#)).unwrap();
        append_fact(&db, "s", 2, "label", Some("a"), Some(r#""one""#)).unwrap();
        append_fact(&db, "s", 3, "label", Some("a"), Some(r#""two""#)).unwrap();
        append_fact(&db, "s", 4, "label", Some("a"), None).unwrap();
        append_fact(&db, "s", 5, "label", Some("b"), Some(r#""bee""#)).unwrap();
        // Latest by kind/key (NULL key matches only NULL).
        let latest = read_latest_fact(&db, "s", "name", None).unwrap();
        assert_eq!(latest.seq, 1);
        assert!(read_latest_fact(&db, "s", "name", Some("x")).is_none());
        // Labels: latest non-null per key; key a's latest value is null so
        // only b remains.
        let labels = read_latest_label_facts(&db, "s").unwrap();
        assert_eq!(labels, vec![("b".to_string(), r#""bee""#.to_string())]);
        // afterSeq + delete.
        let after = read_fact_rows(&db, "s", Some(3), None).unwrap();
        assert_eq!(after.len(), 2);
        delete_fact_rows(&db, "s").unwrap();
        assert_eq!(read_fact_rows(&db, "s", None, None).unwrap().len(), 0);
    }

    #[test]
    fn records_filters_and_open_operations() {
        let db = setup_db();
        db.execute(
            "INSERT INTO sessions (id, created_at, cwd) VALUES ('s', 1, '/tmp')",
            [],
        )
        .unwrap();
        create_initial_lane(&db, "s", "main", None).unwrap();
        for (index, record) in [
            ("r1", "main", "operation_started", Some("run.turn")),
            ("r2", "main", "operation_finished", None),
        ]
        .into_iter()
        .enumerate()
        {
            append_record_row(
                &db,
                "s",
                &NewRecordRow {
                    seq: index as i64,
                    id: record.0.to_string(),
                    lane: record.1.to_string(),
                    run_id: Some("run1".to_string()),
                    record_type: record.2.to_string(),
                    op_kind: record.3.map(|k| k.to_string()),
                    timestamp: 1,
                    payload: "{}".to_string(),
                },
            )
            .unwrap();
        }
        let typed = read_record_rows(
            &db,
            "s",
            &RecordRowsQuery {
                record_type: Some("operation_started".to_string()),
                oldest_first: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(typed[0].id, "r1");
        // No open operation until the lane claims one.
        assert!(
            read_open_operation_rows(&db, "s", "main")
                .unwrap()
                .is_empty()
        );
        start_lane_operation(&db, "s", "main", "r1").unwrap();
        let open = read_open_operation_rows(&db, "s", "main").unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "r1");
        // A lane pointing at a non-started record fails.
        db.execute(
            "UPDATE lanes SET open_operation_id='r2' WHERE lane='main'",
            [],
        )
        .unwrap();
        assert_eq!(
            read_open_operation_rows(&db, "s", "main").unwrap_err().kind,
            "storage"
        );
        // Id existence and delete.
        assert!(id_exists_in_records(&db, "s", "r2"));
        delete_record_rows(&db, "s").unwrap();
        assert_eq!(
            read_record_rows(&db, "s", &RecordRowsQuery::default())
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn writer_leases_fence_expired_holders() {
        let db = setup_db();
        // First acquisition creates fence 1.
        let lease = acquire_writer_lease(&db, "s", "a", 100, 200).unwrap();
        assert_eq!(lease.fence, 1);
        // A second owner cannot steal an unexpired lease.
        assert!(acquire_writer_lease(&db, "s", "b", 150, 300).is_none());
        // Renewal only works for the current owner/fence while unexpired.
        let mut lease = lease;
        assert!(renew_writer_lease(&db, "s", &mut lease, 150, 250));
        assert_eq!(lease.expires_at_ms, 250);
        let mut stale = WriterLease {
            owner_id: "a".to_string(),
            fence: 99,
            expires_at_ms: 250,
        };
        assert!(!renew_writer_lease(&db, "s", &mut stale, 150, 300));
        // After expiry a new owner takes over with a bumped fence.
        let stolen = acquire_writer_lease(&db, "s", "b", 300, 400).unwrap();
        assert_eq!(stolen.fence, 2);
        assert_eq!(stolen.owner_id, "b");
        // The old owner can no longer renew or release.
        assert!(!renew_writer_lease(&db, "s", &mut lease, 310, 500));
        release_writer_lease(&db, "s", &stolen).unwrap();
        // A fresh lease starts again at fence 1.
        let fresh = acquire_writer_lease(&db, "s", "c", 310, 500).unwrap();
        assert_eq!(fresh.fence, 1);
        delete_writer_lease(&db, "s").unwrap();
        assert!(acquire_writer_lease(&db, "s", "d", 320, 600).unwrap().fence == 1);
    }

    #[test]
    fn sequences_and_stats() {
        let db = setup_db();
        create_sequence(&db, "s", 1).unwrap();
        assert_eq!(get_next_sequence(&db, "s").unwrap(), 1);
        advance_sequence(&db, "s", 1).unwrap();
        assert_eq!(get_next_sequence(&db, "s").unwrap(), 2);
        set_next_sequence(&db, "s", 10).unwrap();
        assert_eq!(get_next_sequence(&db, "s").unwrap(), 10);
        assert_eq!(
            get_next_sequence(&db, "missing").unwrap_err().kind,
            "storage"
        );
        delete_sequence(&db, "s").unwrap();

        create_stats(&db, "s", 3).unwrap();
        increment_message_count(&db, "s").unwrap();
        add_usage_to_stats(&db, "s", 7, 5, 12, 0.25).unwrap();
        let stats = read_stats(&db, "s").unwrap();
        assert_eq!(stats.message_count, 4);
        assert_eq!(stats.cached_tokens, 7.0);
        assert_eq!(stats.uncached_tokens, 5.0);
        assert_eq!(stats.total_tokens, 12.0);
        assert_eq!(stats.cost_total, 0.25);
        assert_eq!(read_stats(&db, "missing").unwrap_err().kind, "storage");
        delete_stats(&db, "s").unwrap();
    }

    #[test]
    fn sessions_rows_and_name_facts() {
        let db = setup_db();
        insert_session_row(
            &db,
            &NewSessionRow {
                id: "s1".to_string(),
                created_at: 10,
                cwd: "/tmp".to_string(),
                parent_session_id: None,
                metadata: Some(serde_json::json!({"custom": 1})),
            },
        )
        .unwrap();
        insert_session_row(
            &db,
            &NewSessionRow {
                id: "s2".to_string(),
                created_at: 20,
                cwd: "/tmp".to_string(),
                parent_session_id: Some("s1".to_string()),
                metadata: None,
            },
        )
        .unwrap();
        assert!(session_exists(&db, "s1"));
        // Name facts join through the latest kind='name', key IS NULL row.
        append_fact(&db, "s2", 0, "name", None, Some(r#""alpha""#)).unwrap();
        append_fact(&db, "s2", 1, "name", None, Some(r#""beta""#)).unwrap();
        let rows = read_session_rows(&db, Some("/tmp")).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "s2"); // newest first
        assert!(rows[0].has_session_name);
        assert_eq!(rows[0].session_name.as_deref(), Some(r#""beta""#));
        // Parsed name: the fact value is a JSON string.

        assert!(!rows[1].has_session_name);
        // Metadata decodes as an object; non-object metadata fails.
        let metadata = decode_session_metadata(&rows[0], "/store/s2").unwrap();
        assert_eq!(metadata.name.as_deref(), Some("beta"));
        assert_eq!(metadata.parent_session_id.as_deref(), Some("s1"));
        assert!(metadata.metadata.is_none());
        // s1 carries custom metadata.
        let older = decode_session_metadata(&rows[1], "/store/s1").unwrap();
        assert_eq!(older.metadata.as_ref().unwrap()["custom"], 1);
        // Invalid metadata/name JSON fails at decode; non-object metadata
        // fails at insert.
        assert_eq!(
            insert_session_row(
                &db,
                &NewSessionRow {
                    id: "s3".to_string(),
                    created_at: 30,
                    cwd: "/tmp".to_string(),
                    parent_session_id: None,
                    metadata: Some(serde_json::json!([1])),
                },
            )
            .unwrap_err()
            .kind,
            "invalid_payload"
        );
        delete_session_row(&db, "s2").unwrap();
        assert!(!session_exists(&db, "s2"));
    }
}
