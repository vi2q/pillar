//! Port of packages/agent/src/search (pi v0.84.3) — the session search
//! query contract plus the shared scanning implementation.
//!
//! divergence: upstream `SessionSearch.search` returns an `AsyncIterable`
//! and throws plain `Error`s; the port returns a
//! `futures::stream::BoxStream` and reports failures as [`SearchError`]
//! (the port's `AbortSignal` carries no `reason` object, so abort always
//! reports `SearchError::Aborted`). Storage reads are synchronous in the
//! port, so only the readable source is async — laziness and per-entry
//! abort checks are preserved.

mod scanning;

pub use scanning::{
    CreateHitFn, MatchFn, ProjectTextFn, ScanningReadable, ScanningReadableOptions,
    ScanningReadableSource, ScanningReadableSourceFn, ScanningSessionSearch,
    ScanningSessionSearchHit, ScanningSessionSearchOptions, SessionSearchCandidate,
    scanning_entries,
};

use futures::stream::BoxStream;

/// Search options (upstream `SessionSearchOptions`).
#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    /// Restrict results to specific canonical entry types.
    pub entry_types: Option<Vec<String>>,
    /// Maximum number of hits to return. Upstream treats `limit <= 0` as
    /// "no results"; the port's `usize` has no negative values.
    pub limit: Option<usize>,
    /// Abort signal for cancellation, e.g. search-as-you-type.
    pub signal: Option<crate::abort::AbortSignal>,
}

/// Base hit identity (upstream `SessionSearchHit`). Intentionally minimal:
/// `(sessionId, entryId)` is the portable identity across backends;
/// snippets, timestamps, and scores belong to concrete implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchHit {
    /// Logical identifier of the session that owns the entry.
    pub session_id: String,
    /// Logical identifier of the entry within that session.
    pub entry_id: String,
}

/// Errors surfaced by a search stream (upstream throws plain `Error`s).
/// The stream ends after the first error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// Upstream `AbortError` (upstream threads `signal.reason` through; the
    /// port's AbortSignal has no reason payload).
    #[error("The operation was aborted")]
    Aborted,
    /// Scanning sources fail fast on duplicate session ids because base
    /// hit identity is `(sessionId, entryId)` (upstream `Duplicate
    /// sessionId: ...`).
    #[error("Duplicate sessionId: {0}")]
    DuplicateSessionId(String),
    /// Storage read failure (upstream propagates the backend error).
    #[error("{0}")]
    Storage(String),
}

/// Search contract over committed session entries (upstream
/// `SessionSearch<T extends SessionSearchHit>`). Implementations may extend
/// hits with backend-specific display data.
pub trait SessionSearch<H = ScanningSessionSearchHit>: Send + Sync {
    /// Search committed entries (upstream `search`).
    fn search<'a>(
        &'a self,
        text: &'a str,
        options: SessionSearchOptions,
    ) -> BoxStream<'a, Result<H, SearchError>>;
}
