//! Port of packages/session-backends/sqlite-node/src/sqlite
//! search-backend.ts (pi v0.84.3): SQLite FTS5 search over the
//! co-located canonical session database — a trigram external-content
//! FTS table maintained by triggers on `entries`, queried with bm25
//! ranking and joined back to sessions and name facts.
//!
//! divergences: the async generator becomes `Vec<SessionSearchHit>`;
//! AbortSignal handling is dropped (the port is synchronous); the
//! upstream FileSystem env (absolutePath/createDir/exists) collapses
//! into opening a path directly.

use rusqlite::Connection;

use crate::storage::{SqliteSessionMetadata, decode_session_metadata};

/// A search hit (upstream `SqliteSessionSearchHit`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub metadata: SqliteSessionMetadata,
    pub entry_id: String,
    pub timestamp: i64,
    /// Lower is better (upstream bm25 sign convention).
    pub score: f64,
}

/// Options for [`search_sessions`] (upstream `SessionSearchOptions`).
#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    /// Entry kind filter; an empty list yields no hits (upstream
    /// `options.entryTypes?.length === 0` early return).
    pub entry_types: Option<Vec<String>>,
    pub limit: Option<usize>,
}

/// Ensure the FTS schema exists and backfill when freshly created
/// (upstream `ensureSearchSchema`): an external-content fts5 table
/// with trigram tokenization plus insert/delete/update triggers.
/// apply_migrations variant over &Connection (upstream passes a
/// mutable handle; the port uses an unchecked transaction).
fn apply_migrations_on(db: &Connection) -> Result<(), crate::branch_cache::StorageError> {
    crate::migrations::ensure_migrations_table(db)
        .map_err(|error| crate::branch_cache::StorageError::new("storage", error.to_string()))?;
    let applied = crate::migrations::applied_migrations(db)
        .map_err(|error| crate::branch_cache::StorageError::new("storage", error.to_string()))?;
    for migration in crate::migrations::MIGRATIONS {
        if applied.iter().any(|id| id == migration.id) {
            continue;
        }
        let transaction = db.unchecked_transaction().map_err(|error| {
            crate::branch_cache::StorageError::new("storage", error.to_string())
        })?;
        transaction.execute_batch(migration.sql).map_err(|error| {
            crate::branch_cache::StorageError::new("storage", error.to_string())
        })?;
        transaction
            .execute(
                "INSERT INTO migrations (id, applied_at) VALUES (?, ?)",
                rusqlite::params![migration.id, crate::migrations::now_timestamp()],
            )
            .map_err(|error| {
                crate::branch_cache::StorageError::new("storage", error.to_string())
            })?;
        transaction.commit().map_err(|error| {
            crate::branch_cache::StorageError::new("storage", error.to_string())
        })?;
    }
    Ok(())
}

fn ensure_search_schema(db: &Connection) -> rusqlite::Result<()> {
    let fts_exists: bool = db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_search_fts' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok();
    let entries_exist: bool = db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'entries' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok();
    let transaction = db.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS session_search_fts USING fts5(
            payload,
            content = 'entries',
            content_rowid = 'rowid',
            tokenize = 'trigram remove_diacritics 1'
        );
        CREATE TRIGGER IF NOT EXISTS session_search_fts_ai AFTER INSERT ON entries BEGIN
            INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
        END;
        CREATE TRIGGER IF NOT EXISTS session_search_fts_ad AFTER DELETE ON entries BEGIN
            INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
        END;
        CREATE TRIGGER IF NOT EXISTS session_search_fts_au AFTER UPDATE OF payload ON entries BEGIN
            INSERT INTO session_search_fts(session_search_fts, rowid, payload) VALUES('delete', old.rowid, old.payload);
            INSERT INTO session_search_fts(rowid, payload) VALUES (new.rowid, new.payload);
        END;",
    )?;
    if !fts_exists && entries_exist {
        transaction.execute(
            "INSERT INTO session_search_fts(session_search_fts) VALUES('rebuild')",
            [],
        )?;
    }
    transaction.commit()
}

/// Search sessions by entry payload text (upstream
/// `SqliteSessionSearch.search`): FTS MATCH against the trigram
/// index, joined to sessions and their latest name fact, ranked by
/// bm25 (ascending score = best first).
pub fn search_sessions(
    db: &Connection,
    text: &str,
    options: &SessionSearchOptions,
) -> Result<Vec<SessionSearchHit>, rusqlite::Error> {
    let query_text = text.trim();
    if query_text.is_empty() || options.limit.is_some_and(|limit| limit == 0) {
        return Ok(Vec::new());
    }
    if options
        .entry_types
        .as_ref()
        .is_some_and(|types| types.is_empty())
    {
        return Ok(Vec::new());
    }
    apply_migrations_on(db).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
    })?;
    ensure_search_schema(db)?;

    let query = format!("\"{}\"", query_text.replace('"', "\"\""));
    let mut predicates = vec!["session_search_fts MATCH ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query)];
    if let Some(entry_types) = &options.entry_types {
        let placeholders = (0..entry_types.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(", ");
        predicates.push(format!("se.type IN ({placeholders})"));
        for entry_type in entry_types {
            params.push(Box::new(entry_type.clone()));
        }
    }
    params.push(Box::new(
        options.limit.map(|limit| limit as i64).unwrap_or(-1),
    ));
    let limit_param = params.len();
    let sql = format!(
        "SELECT s.id, s.created_at, s.metadata, s.cwd, s.parent_session_id,
            name_fact.seq IS NOT NULL AS has_session_name,
            name_fact.value AS session_name,
            se.id AS entry_id, se.timestamp, bm25(session_search_fts) AS score
        FROM session_search_fts
        JOIN entries AS se ON se.rowid = session_search_fts.rowid
        JOIN sessions AS s ON s.id = se.session_id
        LEFT JOIN facts AS name_fact
            ON name_fact.session_id = s.id
            AND name_fact.kind = 'name'
            AND name_fact.key IS NULL
            AND name_fact.seq = (
                SELECT MAX(f.seq)
                FROM facts AS f
                WHERE f.session_id = s.id AND f.kind = 'name' AND f.key IS NULL
            )
        WHERE {}
        ORDER BY score
        LIMIT ?{limit_param}",
        predicates.join(" AND ")
    );
    let mut statement = db.prepare(&sql)?;
    let references: Vec<&dyn rusqlite::ToSql> = params.iter().map(|param| param.as_ref()).collect();
    let rows = statement.query_map(references.as_slice(), |row| {
        let session_row = crate::storage::SessionRow {
            id: row.get(0)?,
            created_at: row.get(1)?,
            metadata: row.get(2)?,
            cwd: row.get(3)?,
            parent_session_id: row.get(4)?,
            has_session_name: row.get::<_, i64>(5)? != 0,
            session_name: row.get(6)?,
        };
        let metadata = decode_session_metadata(&session_row, "").map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
        })?;
        Ok(SessionSearchHit {
            session_id: session_row.id,
            metadata,
            entry_id: row.get(7)?,
            timestamp: row.get(8)?,
            score: row.get(9)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::SqliteSessionRepository;
    use pillar_agent::harness::session::memory::SessionStorage;
    use pillar_agent::harness::session::types::{AgentMessage, EntryPayload, ProvisionedEntry};
    use pillar_ai::types::{Content, Message, UserContent};

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::Message(Message::User {
            content: UserContent::Blocks(vec![Content::text(text)]),
            timestamp: 1,
        })
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

    #[test]
    fn search_finds_entries_by_payload_text() {
        let repository = SqliteSessionRepository::open_in_memory(Default::default()).unwrap();
        let alpha = repository.create("s1", "/tmp", None, None).unwrap();
        alpha
            .append_entry(message_entry("e1", "the quick brown fox"), "main")
            .unwrap();
        let beta = repository.create("s2", "/tmp", None, None).unwrap();
        beta.append_entry(message_entry("e2", "something else entirely"), "main")
            .unwrap();
        alpha.release().unwrap();
        beta.release().unwrap();
        let guard = repository.db.lock().unwrap();
        let db = guard.as_ref().unwrap();
        let hits = search_sessions(db, "quick brown", &SessionSearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].entry_id, "e1");
        // No match: empty.
        let none = search_sessions(db, "zebra", &SessionSearchOptions::default()).unwrap();
        assert!(none.is_empty());
        // Empty query: empty.
        assert!(
            search_sessions(db, "   ", &SessionSearchOptions::default())
                .unwrap()
                .is_empty()
        );
        // Typed filter: only compaction entries -> none here.
        let typed = search_sessions(
            db,
            "quick brown",
            &SessionSearchOptions {
                entry_types: Some(vec!["compaction".to_string()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(typed.is_empty());
        // Empty entry type list yields no hits (upstream early return).
        let empty_types = search_sessions(
            db,
            "quick brown",
            &SessionSearchOptions {
                entry_types: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(empty_types.is_empty());
    }

    #[test]
    fn search_decodes_session_metadata_and_name() {
        let repository = SqliteSessionRepository::open_in_memory(Default::default()).unwrap();
        let storage = repository.create("s1", "/work", None, None).unwrap();
        storage.set_name(Some("named session".to_string())).unwrap();
        storage
            .append_entry(message_entry("e1", "searchable haystack"), "main")
            .unwrap();
        storage.release().unwrap();
        let guard = repository.db.lock().unwrap();
        let db = guard.as_ref().unwrap();
        let hits = search_sessions(db, "haystack", &SessionSearchOptions::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.name.as_deref(), Some("named session"));
        assert_eq!(hits[0].metadata.cwd, "/work");
    }

    #[test]
    fn search_after_new_writes_without_rebuild() {
        // Triggers keep the index current for post-rebuild inserts.
        let repository = SqliteSessionRepository::open_in_memory(Default::default()).unwrap();
        let storage = repository.create("s1", "/tmp", None, None).unwrap();
        storage
            .append_entry(message_entry("e1", "first entry text"), "main")
            .unwrap();
        storage.release().unwrap();
        let guard = repository.db.lock().unwrap();
        let db = guard.as_ref().unwrap();
        // First search creates the FTS schema (rebuild picks up e1).
        assert_eq!(
            search_sessions(db, "first entry", &SessionSearchOptions::default())
                .unwrap()
                .len(),
            1
        );
        // A later entry is indexed by trigger.
        drop(guard);
        let storage = repository.open(&repository.list(None).unwrap()[0]).unwrap();
        storage
            .append_entry(message_entry("e2", "second later note"), "main")
            .unwrap();
        storage.release().unwrap();
        let guard = repository.db.lock().unwrap();
        let db = guard.as_ref().unwrap();
        assert_eq!(
            search_sessions(db, "later note", &SessionSearchOptions::default())
                .unwrap()
                .len(),
            1
        );
    }
}
