//! Port of packages/session-backends/sqlite-node/src/sqlite
//! (pi v0.84.3): the SQLite session storage schema, migration
//! application, and branch cache maintenance.
//!
//! divergences: the upstream uses better-sqlite3; the port uses
//! rusqlite with the bundled SQLite. The SessionError taxonomy maps to
//! the pillar-agent session error shape via a local error type.

pub mod migrations;

/// Re-exported for convenience.
pub use migrations::{MIGRATIONS, apply_migrations};
