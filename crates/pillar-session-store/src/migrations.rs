//! Port of packages/session-backends/sqlite-node/src/sqlite/migrations.ts
//! (pi v0.84.3): the migrations table, ordered migration application,
//! and the embedded schema SQL.

use rusqlite::Connection;

/// A schema migration (upstream `SqliteMigration`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteMigration {
    pub id: &'static str,
    pub order: u32,
    pub sql: &'static str,
}

/// The initial schema (upstream migrations/001_initial.sql).
pub const INITIAL_SCHEMA: &str = include_str!("migrations/001_initial.sql");

/// The ordered migration list (upstream `loadMigrations`).
pub const MIGRATIONS: &[SqliteMigration] = &[SqliteMigration {
    id: "001_initial.sql",
    order: 1,
    sql: INITIAL_SCHEMA,
}];

/// Create the migrations table (upstream `ensureMigrationsTable`).
fn ensure_migrations_table(db: &Connection) -> rusqlite::Result<()> {
    db.execute(
        "CREATE TABLE IF NOT EXISTS migrations (
            id TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
        [],
    )
    .map(|_| ())
}

/// Already-applied migration ids, ordered (upstream the SELECT id FROM
/// migrations query).
pub fn applied_migrations(db: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = db.prepare("SELECT id FROM migrations ORDER BY applied_at, id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// Apply pending migrations in order inside transactions (upstream
/// `applyMigrations`): each migration runs in a transaction with its
/// id recorded; already-applied ids are skipped.
pub fn apply_migrations(db: &mut Connection) -> rusqlite::Result<()> {
    ensure_migrations_table(db)?;
    let applied = applied_migrations(db)?;
    for migration in MIGRATIONS {
        if applied.iter().any(|id| id == migration.id) {
            continue;
        }
        let transaction = db.transaction()?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO migrations (id, applied_at) VALUES (?, ?)",
            rusqlite::params![migration.id, now_iso()],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

/// ISO-8601 timestamp without external crates (upstream
/// `new Date().toISOString()` shape: YYYY-MM-DDTHH:MM:SSZ). The
/// migrations table orders by this string, so a second-resolution
/// monotonic value keeps ordering stable within a process.
fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z",
        h = time_of_day / 3600,
        m = (time_of_day % 3600) / 60,
        s = time_of_day % 60,
    )
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_gets_all_migrations() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        let applied = applied_migrations(&db).unwrap();
        assert_eq!(applied, vec!["001_initial.sql".to_string()]);
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        apply_migrations(&mut db).unwrap();
        let applied = applied_migrations(&db).unwrap();
        assert_eq!(applied.len(), MIGRATIONS.len());
    }

    #[test]
    fn schema_tables_exist_after_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        for table in [
            "sessions",
            "entries",
            "session_sequences",
            "session_stats",
            "branch_entries",
            "lanes",
            "branch_tips",
            "writer_leases",
            "migrations",
        ] {
            let exists: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "table {table} should exist");
        }
    }

    #[test]
    fn schema_indexes_exist_after_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        for index in [
            "idx_sessions_created_at",
            "idx_entries_session_parent",
            "idx_branch_entries_session_branch_seq",
            "idx_facts_session_kind_key_seq",
        ] {
            let exists: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?",
                    rusqlite::params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "index {index} should exist");
        }
    }

    #[test]
    fn sessions_table_rejects_duplicate_ids() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        db.execute(
            "INSERT INTO sessions (id, created_at, cwd) VALUES ('a', 1, '/tmp')",
            [],
        )
        .unwrap();
        assert!(db
            .execute(
                "INSERT INTO sessions (id, created_at, cwd) VALUES ('a', 2, '/tmp')",
                [],
            )
            .is_err());
    }

    #[test]
    fn entries_seq_is_unique_per_session() {
        let mut db = Connection::open_in_memory().unwrap();
        apply_migrations(&mut db).unwrap();
        let insert = |seq: i64, id: &str| {
            db.execute(
                "INSERT INTO entries (session_id, seq, id, parent_id, type, timestamp, payload)
                 VALUES ('s', ?, ?, NULL, 'user', 0, '{}')",
                rusqlite::params![seq, id],
            )
        };
        insert(0, "e1").unwrap();
        insert(1, "e2").unwrap();
        // Same seq for a different session is fine.
        db.execute(
            "INSERT INTO entries (session_id, seq, id, parent_id, type, timestamp, payload)
             VALUES ('t', 0, 'e1', NULL, 'user', 0, '{}')",
            [],
        )
        .unwrap();
        // Duplicate seq within the session fails.
        assert!(insert(0, "e3").is_err());
    }

    #[test]
    fn civil_date_matches_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
    }
}
