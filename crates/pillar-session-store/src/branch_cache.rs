//! Port of packages/session-backends/sqlite-node/src/sqlite
//! storage/branch-entries.ts + branch-tips.ts + branch-cache.ts
//! (pi v0.84.3): the derived branch cache — root-to-tip membership,
//! boundary-aware branch queries, path materialization, and tip
//! bookkeeping.
//!
//! divergences: uuidv7 branch ids are host-supplied (upstream
//! generates them internally); SessionError becomes
//! [`StorageError`]; the tagged-template `sql` helper becomes
//! rusqlite parameter binding.

use rusqlite::Connection;

/// Storage-layer error (upstream `SessionError` kinds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    pub kind: &'static str,
    pub message: String,
}

impl StorageError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StorageError {}

/// Derived root-to-tip branch cache membership (upstream
/// `CachedBranch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBranch {
    pub branch_id: String,
    pub leaf_seq: i64,
}

/// A branch cache row joined with the canonical entry (upstream
/// `CachedBranchEntryRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedBranchEntryRow {
    pub session_id: String,
    pub id: String,
    pub entry_seq: i64,
    pub parent_id: Option<String>,
    pub entry_type: String,
    pub timestamp: i64,
    pub payload: String,
}

/// Branch query filters (upstream `CachedBranchQuery`).
#[derive(Debug, Clone, Default)]
pub struct CachedBranchQuery {
    pub entry_type: Option<String>,
    pub custom_type: Option<String>,
    pub stop_at_type: Option<String>,
    pub stop_at_id: Option<String>,
    pub cursor_after_seq: Option<i64>,
    pub oldest_first: bool,
    pub limit: Option<usize>,
}

/// Read the cache membership for a leaf entry (upstream
/// `readCachedBranch`): the branch containing it and its seq.
pub fn read_cached_branch(
    db: &Connection,
    session_id: &str,
    leaf_id: &str,
) -> Option<CachedBranch> {
    db.query_row(
        "SELECT branch_id, entry_seq
         FROM branch_entries
         WHERE session_id = ?1 AND entry_id = ?2
         ORDER BY branch_id
         LIMIT 1",
        rusqlite::params![session_id, leaf_id],
        |row| {
            Ok(CachedBranch {
                branch_id: row.get(0)?,
                leaf_seq: row.get(1)?,
            })
        },
    )
    .ok()
}

/// Query cached branch rows (upstream `queryCachedBranchRows`):
/// boundary predicates stop the scan at the first stop-at match,
/// cursor/typed filters apply, ordering and limit follow the query.
pub fn query_cached_branch_rows(
    db: &Connection,
    session_id: &str,
    branch: &CachedBranch,
    query: &CachedBranchQuery,
) -> rusqlite::Result<Vec<CachedBranchEntryRow>> {
    let oldest_first = query.oldest_first;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(session_id.to_string()),
        Box::new(branch.branch_id.clone()),
        Box::new(branch.leaf_seq),
    ];
    let mut stop_clause: Option<String> = None;
    if query.stop_at_type.is_some() || query.stop_at_id.is_some() {
        let aggregate = if oldest_first { "MIN" } else { "MAX" };
        let mut conditions = Vec::new();
        if let Some(stop_at_type) = &query.stop_at_type {
            params.push(Box::new(stop_at_type.clone()));
            conditions.push(format!("stop.entry_type = ?{}", params.len()));
        }
        if let Some(stop_at_id) = &query.stop_at_id {
            params.push(Box::new(stop_at_id.clone()));
            conditions.push(format!("stop.entry_id = ?{}", params.len()));
        }
        stop_clause = Some(format!(
            "SELECT {aggregate}(stop.entry_seq)
             FROM branch_entries AS stop
             WHERE stop.session_id = ?1
               AND stop.branch_id = ?2
               AND stop.entry_seq <= ?3
               AND ({})",
            conditions.join(" OR ")
        ));
    }

    let mut predicates = vec![
        "b.session_id = ?1".to_string(),
        "b.branch_id = ?2".to_string(),
        "b.entry_seq <= ?3".to_string(),
    ];
    if let Some(stop_clause) = stop_clause {
        // The boundary subquery is interpolated as SQL text (upstream
        // joinSqlFragments); its ?1/?2/?3 references align with the
        // top-level parameters already bound.
        let fallback = if oldest_first { branch.leaf_seq } else { 0 };
        predicates.push(format!(
            "b.entry_seq {} (COALESCE(({stop_clause}), {fallback}))",
            if oldest_first { "<=" } else { ">=" },
        ));
    }
    if let Some(cursor) = query.cursor_after_seq {
        params.push(Box::new(cursor));
        predicates.push(format!(
            "b.entry_seq {} ?{}",
            if oldest_first { ">" } else { "<" },
            params.len()
        ));
    }
    if let Some(entry_type) = &query.entry_type {
        params.push(Box::new(entry_type.clone()));
        predicates.push(format!("b.entry_type = ?{}", params.len()));
    }
    if let Some(custom_type) = &query.custom_type {
        params.push(Box::new(custom_type.clone()));
        predicates.push(format!("b.custom_type = ?{}", params.len()));
    }
    let limit = query
        .limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let direction = if oldest_first { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT e.session_id, e.id, e.seq AS entry_seq, e.parent_id, e.type, e.timestamp, e.payload
         FROM branch_entries AS b
         JOIN entries AS e ON e.session_id = b.session_id AND e.id = b.entry_id
         WHERE {}
         ORDER BY b.entry_seq {direction}{limit}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let parameter_references: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(parameter_references.as_slice(), |row| {
        Ok(CachedBranchEntryRow {
            session_id: row.get(0)?,
            id: row.get(1)?,
            entry_seq: row.get(2)?,
            parent_id: row.get(3)?,
            entry_type: row.get(4)?,
            timestamp: row.get(5)?,
            payload: row.get(6)?,
        })
    })?;
    rows.collect()
}

pub fn delete_branch_entries(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM branch_entries WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

pub fn insert_branch_entry(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    entry_id: &str,
    entry_seq: i64,
    entry_type: &str,
    custom_type: Option<&str>,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO branch_entries
            (session_id, branch_id, entry_id, entry_seq, entry_type, custom_type)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            session_id,
            branch_id,
            entry_id,
            entry_seq,
            entry_type,
            custom_type
        ],
    )
    .map(|_| ())
}

/// Extract the customType from a custom entry payload (upstream
/// `customTypeFromPayload`).
fn custom_type_from_payload(row: &BranchPathEntryRow) -> Result<Option<String>, StorageError> {
    if row.entry_type != "custom" {
        return Ok(None);
    }
    let payload: serde_json::Value = serde_json::from_str(&row.payload).map_err(|_| {
        StorageError::new(
            "invalid_entry",
            format!(
                "Invalid SQLite session entry {}: failed to decode entry {}",
                row.id, row.id
            ),
        )
    })?;
    let custom_type = payload.get("customType").and_then(|v| v.as_str());
    match custom_type {
        Some(custom_type) => Ok(Some(custom_type.to_string())),
        None => Err(StorageError::new(
            "invalid_entry",
            format!(
                "Invalid SQLite session entry {}: failed to decode entry {}",
                row.id, row.id
            ),
        )),
    }
}

struct BranchPathEntryRow {
    id: String,
    seq: i64,
    parent_id: Option<String>,
    entry_type: String,
    payload: String,
}

/// Materialize the root-to-tip path for a leaf into the cache (upstream
/// `insertBranchEntriesForPath`): walks parents (detecting cycles),
/// inserts in root-first order with per-entry custom types.
pub fn insert_branch_entries_for_path(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    leaf_id: &str,
) -> Result<(), StorageError> {
    let mut path: Vec<BranchPathEntryRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut entry_id: Option<String> = Some(leaf_id.to_string());
    while let Some(current) = entry_id {
        if seen.contains(&current) {
            return Err(StorageError::new(
                "invalid_entry",
                format!("Entry parent cycle at {current}"),
            ));
        }
        seen.insert(current.clone());
        let row = db
            .query_row(
                "SELECT id, seq, parent_id, type, payload
                 FROM entries
                 WHERE session_id = ?1 AND id = ?2",
                rusqlite::params![session_id, current],
                |row| {
                    Ok(BranchPathEntryRow {
                        id: row.get(0)?,
                        seq: row.get(1)?,
                        parent_id: row.get(2)?,
                        entry_type: row.get(3)?,
                        payload: row.get(4)?,
                    })
                },
            )
            .map_err(|_| {
                StorageError::new("invalid_entry", format!("Entry {current} not found"))
            })?;
        entry_id = row.parent_id.clone();
        path.push(row);
    }
    for row in path.iter().rev() {
        let custom_type = custom_type_from_payload(row)?;
        insert_branch_entry(
            db,
            session_id,
            branch_id,
            &row.id,
            row.seq,
            &row.entry_type,
            custom_type.as_deref(),
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    }
    Ok(())
}

/// Find a cached branch containing an entry (upstream
/// `readBranchContainingEntry`).
pub fn read_branch_containing_entry(
    db: &Connection,
    session_id: &str,
    entry_id: &str,
) -> Option<CachedBranch> {
    read_cached_branch(db, session_id, entry_id)
}

/// Copy cached entries up to a seq into a new branch (upstream
/// `copyBranchEntriesThroughSeq`).
pub fn copy_branch_entries_through_seq(
    db: &Connection,
    session_id: &str,
    target_branch_id: &str,
    source_branch_id: &str,
    through_seq: i64,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq, entry_type, custom_type)
         SELECT session_id, ?2, entry_id, entry_seq, entry_type, custom_type
         FROM branch_entries
         WHERE session_id = ?1 AND branch_id = ?3 AND entry_seq <= ?4",
        rusqlite::params![session_id, target_branch_id, source_branch_id, through_seq],
    )
    .map(|_| ())
}

// ============================================================================
// branch-tips.ts
// ============================================================================

/// Read tip ids for a session (upstream `readBranchTipIds`).
pub fn read_branch_tip_ids(db: &Connection, session_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement =
        db.prepare("SELECT tip_id FROM branch_tips WHERE session_id = ?1 ORDER BY tip_id")?;
    let rows = statement.query_map(rusqlite::params![session_id], |row| row.get(0))?;
    rows.collect()
}

/// Read the branch id for a tip (upstream `readBranchTipBranchId`).
pub fn read_branch_tip_branch_id(
    db: &Connection,
    session_id: &str,
    tip_id: &str,
) -> Option<String> {
    db.query_row(
        "SELECT branch_id FROM branch_tips WHERE session_id = ?1 AND tip_id = ?2",
        rusqlite::params![session_id, tip_id],
        |row| row.get(0),
    )
    .ok()
}

pub fn insert_branch_tip(
    db: &Connection,
    session_id: &str,
    tip_id: &str,
    branch_id: &str,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO branch_tips (session_id, tip_id, branch_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, tip_id, branch_id],
    )
    .map(|_| ())
}

/// Move a tip to a new entry (upstream `updateBranchTip`): returns
/// whether the expected old tip was still there.
pub fn update_branch_tip(
    db: &Connection,
    session_id: &str,
    branch_id: &str,
    old_tip_id: &str,
    new_tip_id: &str,
) -> rusqlite::Result<bool> {
    let changed = db.execute(
        "UPDATE branch_tips SET tip_id = ?4
         WHERE session_id = ?1 AND branch_id = ?2 AND tip_id = ?3",
        rusqlite::params![session_id, branch_id, old_tip_id, new_tip_id],
    )?;
    Ok(changed == 1)
}

pub fn delete_branch_tips(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM branch_tips WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
    .map(|_| ())
}

// ============================================================================
// branch-cache.ts
// ============================================================================

/// Drop both cache tables for a session (upstream `deleteBranchCache`).
pub fn delete_branch_cache(db: &Connection, session_id: &str) -> rusqlite::Result<()> {
    delete_branch_tips(db, session_id)?;
    delete_branch_entries(db, session_id)
}

/// Rebuild the whole cache from the canonical entries table (upstream
/// `rebuildBranchCache`): leaves are entries with no children.
pub fn rebuild_branch_cache(db: &Connection, session_id: &str) -> Result<(), StorageError> {
    let mut statement = db
        .prepare(
            "SELECT leaf.id
             FROM entries AS leaf
             WHERE leaf.session_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM entries AS child
                   WHERE child.session_id = leaf.session_id AND child.parent_id = leaf.id
               )
             ORDER BY leaf.seq",
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    let tips: Vec<String> = statement
        .query_map(rusqlite::params![session_id], |row| row.get(0))
        .map_err(|error| StorageError::new("storage", error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    drop(statement);
    delete_branch_cache(db, session_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
    for tip in tips {
        build_cached_branch(db, session_id, &tip, &format!("branch-{}", tip))?;
    }
    Ok(())
}

/// Build the cache for one leaf's path (upstream `buildCachedBranch`).
pub fn build_cached_branch(
    db: &Connection,
    session_id: &str,
    leaf_id: &str,
    branch_id: &str,
) -> Result<(), StorageError> {
    insert_branch_entries_for_path(db, session_id, branch_id, leaf_id)?;
    insert_branch_tip(db, session_id, leaf_id, branch_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))
}

/// Append an entry to the cache (upstream `appendEntryToBranchCache`):
/// a root entry starts a new branch; otherwise extend the tip branch
/// or fork from the branch containing the parent.
#[allow(clippy::too_many_arguments)]
pub fn append_entry_to_branch_cache(
    db: &Connection,
    session_id: &str,
    entry_id: &str,
    entry_seq: i64,
    entry_type: &str,
    custom_type: Option<&str>,
    parent_id: Option<&str>,
    new_branch_id: &str,
) -> Result<(), StorageError> {
    let Some(parent_id) = parent_id else {
        insert_branch_entry(
            db,
            session_id,
            new_branch_id,
            entry_id,
            entry_seq,
            entry_type,
            custom_type,
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
        insert_branch_tip(db, session_id, entry_id, new_branch_id)
            .map_err(|error| StorageError::new("storage", error.to_string()))?;
        return Ok(());
    };
    if let Some(tip_branch_id) = read_branch_tip_branch_id(db, session_id, parent_id) {
        // Extend the tip branch.
        insert_branch_entry(
            db,
            session_id,
            &tip_branch_id,
            entry_id,
            entry_seq,
            entry_type,
            custom_type,
        )
        .map_err(|error| StorageError::new("storage", error.to_string()))?;
        let moved = update_branch_tip(db, session_id, &tip_branch_id, parent_id, entry_id)
            .map_err(|error| StorageError::new("storage", error.to_string()))?;
        if !moved {
            return Err(StorageError::new(
                "invalid_entry",
                format!("Branch tip {parent_id} changed during append"),
            ));
        }
        return Ok(());
    }
    // Fork: copy the source branch up to the parent's seq.
    let Some(source) = read_branch_containing_entry(db, session_id, parent_id) else {
        return Err(StorageError::new(
            "invalid_entry",
            format!("Branch cache has no branch containing parent entry {parent_id}"),
        ));
    };
    copy_branch_entries_through_seq(
        db,
        session_id,
        new_branch_id,
        &source.branch_id,
        source.leaf_seq,
    )
    .map_err(|error| StorageError::new("storage", error.to_string()))?;
    insert_branch_entry(
        db,
        session_id,
        new_branch_id,
        entry_id,
        entry_seq,
        entry_type,
        custom_type,
    )
    .map_err(|error| StorageError::new("storage", error.to_string()))?;
    insert_branch_tip(db, session_id, entry_id, new_branch_id)
        .map_err(|error| StorageError::new("storage", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        crate::migrations::apply_migrations(&mut db).unwrap();
        db
    }

    fn insert_entry(
        db: &Connection,
        session: &str,
        seq: i64,
        id: &str,
        parent: Option<&str>,
        payload: &str,
    ) {
        db.execute(
            "INSERT INTO entries (session_id, seq, id, parent_id, type, timestamp, payload)
             VALUES (?1, ?2, ?3, ?4, 'message', 0, ?5)",
            rusqlite::params![session, seq, id, parent, payload],
        )
        .unwrap();
    }

    #[test]
    fn append_root_starts_new_branch() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        // The tip moved to e1.
        assert_eq!(
            read_branch_tip_branch_id(&db, "s", "e1"),
            Some("b1".to_string())
        );
        // The branch contains e1.
        assert_eq!(
            read_branch_containing_entry(&db, "s", "e1"),
            Some(CachedBranch {
                branch_id: "b1".to_string(),
                leaf_seq: 0
            })
        );
    }

    #[test]
    fn append_extends_tip_branch() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "e2", 1, "message", None, Some("e1"), "b2").unwrap();
        // The tip moved from e1 to e2 on the same branch.
        assert_eq!(
            read_branch_tip_branch_id(&db, "s", "e2"),
            Some("b1".to_string())
        );
        assert_eq!(
            read_cached_branch(&db, "s", "e2"),
            Some(CachedBranch {
                branch_id: "b1".to_string(),
                leaf_seq: 1
            })
        );
    }

    #[test]
    fn append_forks_from_branch_containing_parent() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        insert_entry(&db, "s", 2, "e2b", Some("e1"), r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "e2", 1, "message", None, Some("e1"), "b1").unwrap();
        // Fork from e1 (inside b1) onto a new branch.
        append_entry_to_branch_cache(&db, "s", "e2b", 2, "message", None, Some("e1"), "b2")
            .unwrap();
        // e2b lives on b2; e1 also in b2 (copied).
        assert_eq!(
            read_cached_branch(&db, "s", "e2b"),
            Some(CachedBranch {
                branch_id: "b2".to_string(),
                leaf_seq: 2
            })
        );
        assert_eq!(
            read_cached_branch(&db, "s", "e1"),
            Some(CachedBranch {
                branch_id: "b1".to_string(),
                leaf_seq: 0
            })
        );
    }

    #[test]
    fn missing_parent_branch_fails() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        let error =
            append_entry_to_branch_cache(&db, "s", "e2", 1, "message", None, Some("e1"), "b1")
                .unwrap_err();
        assert_eq!(error.kind, "invalid_entry");
        assert!(
            error
                .message
                .contains("no branch containing parent entry e1")
        );
    }

    #[test]
    fn rebuild_discovers_leaves() {
        let db = setup_db();
        // Two chains: e1→e2 and e3.
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        insert_entry(&db, "s", 2, "e3", None, r#"{"type":"message"}"#);
        rebuild_branch_cache(&db, "s").unwrap();
        let tips = read_branch_tip_ids(&db, "s").unwrap();
        assert_eq!(tips, vec!["e2".to_string(), "e3".to_string()]);
    }

    #[test]
    fn delete_branch_cache_clears_both_tables() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        delete_branch_cache(&db, "s").unwrap();
        assert!(read_branch_tip_ids(&db, "s").unwrap().is_empty());
        assert_eq!(read_cached_branch(&db, "s", "e1"), None);
    }

    #[test]
    fn cycle_detection() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", Some("e2"), r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        let error = insert_branch_entries_for_path(&db, "s", "b1", "e1").unwrap_err();
        assert_eq!(error.kind, "invalid_entry");
        assert!(error.message.contains("parent cycle at"));
    }

    #[test]
    fn custom_type_extracted_from_payload() {
        let db = setup_db();
        db.execute(
            "INSERT INTO entries (session_id, seq, id, parent_id, type, timestamp, payload)
             VALUES ('s', 0, 'c1', NULL, 'custom', 0, '{\"type\":\"custom\",\"customType\":\"mytype\"}')",
            [],
        )
        .unwrap();
        insert_branch_entries_for_path(&db, "s", "b1", "c1").unwrap();
        // Verify the cached row carries the custom type.
        let custom: String = db
            .query_row(
                "SELECT custom_type FROM branch_entries WHERE session_id='s' AND entry_id='c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(custom, "mytype");
    }

    #[test]
    fn boundary_query_stops_at_stop_at_id() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        insert_entry(&db, "s", 2, "e3", Some("e2"), r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "e2", 1, "message", None, Some("e1"), "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "e3", 2, "message", None, Some("e2"), "b1").unwrap();
        let branch = read_cached_branch(&db, "s", "e3").unwrap();
        // Newest-first with a stop at e2: rows e3, e2.
        let rows = query_cached_branch_rows(
            &db,
            "s",
            &branch,
            &CachedBranchQuery {
                stop_at_id: Some("e2".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "e3");
        assert_eq!(rows[1].id, "e2");
    }

    #[test]
    fn typed_query_filters_rows() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        db.execute(
            "INSERT INTO entries (session_id, seq, id, parent_id, type, timestamp, payload)
             VALUES ('s', 1, 'c1', 'e1', 'custom', 0, '{\"type\":\"custom\",\"customType\":\"t\"}')",
            [],
        )
        .unwrap();
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "c1", 1, "custom", Some("t"), Some("e1"), "b1")
            .unwrap();
        let branch = read_cached_branch(&db, "s", "c1").unwrap();
        let rows = query_cached_branch_rows(
            &db,
            "s",
            &branch,
            &CachedBranchQuery {
                entry_type: Some("custom".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "c1");
    }

    #[test]
    fn limit_applies() {
        let db = setup_db();
        insert_entry(&db, "s", 0, "e1", None, r#"{"type":"message"}"#);
        insert_entry(&db, "s", 1, "e2", Some("e1"), r#"{"type":"message"}"#);
        append_entry_to_branch_cache(&db, "s", "e1", 0, "message", None, None, "b1").unwrap();
        append_entry_to_branch_cache(&db, "s", "e2", 1, "message", None, Some("e1"), "b1").unwrap();
        let branch = read_cached_branch(&db, "s", "e2").unwrap();
        let rows = query_cached_branch_rows(
            &db,
            "s",
            &branch,
            &CachedBranchQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "e2");
    }
}
