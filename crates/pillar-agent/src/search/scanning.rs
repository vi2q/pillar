//! Port of packages/agent/src/search/scanning.ts (pi v0.84.3) — the shared
//! scanner that adapts session-like readables (`getMetadata`, `findEntries`,
//! `getLabel`) into projected search candidates and hits.
//!
//! divergence: upstream is generic over `TMetadata extends SessionMetadata`
//! and over the hit type via an unchecked cast of the default hit; the port
//! fixes the metadata shape to `SessionMetadata` (richer backend metadata is
//! reachable through `Session::metadata_json`) and requires
//! `H: From<ScanningSessionSearchHit>` instead of the cast. Upstream
//! `sourceOptions` folds into the source closure, which receives the
//! normalized query text and search options directly.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream};

use super::{SearchError, SessionSearchOptions};
use crate::abort::AbortSignal;
use crate::harness::session::memory::{Session, SessionStorage};
use crate::harness::session::types::{
    Entry, EntryOrder, EntryQuery, SessionError, SessionMetadata,
};

/// Page size when neither the caller nor the options set one (upstream
/// `scanReadableEntries` default).
const DEFAULT_PAGE_SIZE: usize = 100;

/// Session-like read access used by the scanner (upstream `ScanningReadable`
/// is a Pick of `SessionStorage`). Blanket-implemented for every
/// [`SessionStorage`] backend and for the [`Session`] facade.
pub trait ScanningReadable: Send + Sync {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError>;
    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError>;
    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError>;
}

impl<T: SessionStorage + ?Sized> ScanningReadable for T {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        SessionStorage::get_metadata(self)
    }

    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        SessionStorage::find_entries(self, query)
    }

    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        SessionStorage::get_label(self, id)
    }
}

impl ScanningReadable for Session {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        Session::get_metadata(self)
    }

    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Session::find_entries(self, query)
    }

    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        Session::get_label(self, id)
    }
}

/// Searchable-text projection (upstream `ScanningSearchTextProjector`).
pub type ProjectTextFn =
    Arc<dyn Fn(&SessionMetadata, &Entry, Option<&str>) -> String + Send + Sync>;

/// Match predicate (upstream `ScanningSessionSearchOptions.match`). Receives
/// the normalized query text.
pub type MatchFn =
    Arc<dyn Fn(&str, &SessionSearchCandidate, &SessionMetadata) -> bool + Send + Sync>;

/// Hit constructor (upstream `ScanningSessionSearchOptions.createHit`).
pub type CreateHitFn<H> = Arc<dyn Fn(&SessionMetadata, &SessionSearchCandidate) -> H + Send + Sync>;

/// Async readable source (upstream `ScanningReadableSource`, the function
/// arm of `readablesFor`). Receives the normalized query text and the search
/// options; upstream `sourceOptions` folds into this closure. The future is
/// not polled until the search stream is pulled, so discovery stays lazy
/// like the upstream async generator.
pub type ScanningReadableSourceFn =
    Arc<dyn Fn(&str, &SessionSearchOptions) -> ScanningReadableFuture + Send + Sync>;

/// Resolved future of one source pull (upstream awaiting the generator).
pub type ScanningReadableFuture = BoxFuture<
    'static,
    Result<BoxStream<'static, Result<Arc<dyn ScanningReadable>, SearchError>>, SearchError>,
>;

/// Search source (upstream `readonly ScanningReadable[] |
/// ScanningReadableSource` union).
pub enum ScanningReadableSource {
    /// Already-opened readables (upstream array arm).
    Readables(Vec<Arc<dyn ScanningReadable>>),
    /// Lazily produced readables (upstream async-generator arm).
    SourceFn(ScanningReadableSourceFn),
}

impl ScanningReadableSource {
    pub fn readables(readables: Vec<Arc<dyn ScanningReadable>>) -> Self {
        Self::Readables(readables)
    }

    pub fn source_fn(source: ScanningReadableSourceFn) -> Self {
        Self::SourceFn(source)
    }
}

/// Per-readable scanner options (upstream `ScanningReadableOptions`).
#[derive(Clone, Default)]
pub struct ScanningReadableOptions {
    pub project_text: Option<ProjectTextFn>,
    pub page_size: Option<usize>,
}

/// Scanning search options (upstream `ScanningSessionSearchOptions`, which
/// extends `ScanningReadableOptions`).
pub struct ScanningSessionSearchOptions<H = ScanningSessionSearchHit> {
    pub project_text: Option<ProjectTextFn>,
    pub page_size: Option<usize>,
    /// Upstream `match`.
    pub matcher: Option<MatchFn>,
    pub create_hit: Option<CreateHitFn<H>>,
}

impl<H> Default for ScanningSessionSearchOptions<H> {
    fn default() -> Self {
        Self {
            project_text: None,
            page_size: None,
            matcher: None,
            create_hit: None,
        }
    }
}

/// Pre-match scanner input (upstream `SessionSearchCandidate`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchCandidate {
    pub entry_id: String,
    pub seq: u64,
    /// Canonical entry type (upstream `type`).
    pub entry_type: String,
    pub timestamp: u64,
    pub text: String,
    /// Optional projected fields; the default projector holds `{ label }`
    /// when the entry has a label.
    pub fields: Option<serde_json::Value>,
}

/// Scanning hit (upstream `ScanningSessionSearchHit extends
/// SessionSearchHit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanningSessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: u64,
    pub snippet: String,
}

impl From<ScanningSessionSearchHit> for super::SessionSearchHit {
    fn from(hit: ScanningSessionSearchHit) -> Self {
        Self {
            session_id: hit.session_id,
            entry_id: hit.entry_id,
        }
    }
}

/// Upstream `defaultSearchText`: the serialized entry plus its label when
/// one is set.
fn default_search_text(_metadata: &SessionMetadata, entry: &Entry, label: Option<&str>) -> String {
    let serialized = serde_json::to_string(entry).unwrap_or_default();
    match label {
        Some(label) => format!("{serialized} {label}"),
        None => serialized,
    }
}

fn resolve_project_text(project_text: Option<ProjectTextFn>) -> ProjectTextFn {
    project_text.unwrap_or_else(|| {
        Arc::new(
            |metadata: &SessionMetadata, entry: &Entry, label: Option<&str>| {
                default_search_text(metadata, entry, label)
            },
        )
    })
}

/// Lazy candidate scan over one readable (upstream `scanReadableEntries`).
/// Entries are paged oldest-first through the storage cursor so memory use
/// stays bounded like the upstream async generator.
pub struct ScanReadableEntries {
    readable: Arc<dyn ScanningReadable>,
    metadata: SessionMetadata,
    project_text: ProjectTextFn,
    page_size: usize,
    after_seq: u64,
    entry_types: Option<HashSet<String>>,
    /// Upstream pushes a single-type filter into the storage query.
    single_kind: Option<String>,
    page: std::vec::IntoIter<Entry>,
    /// No more pages; the current page still drains first.
    done: bool,
}

impl ScanReadableEntries {
    fn new(
        readable: Arc<dyn ScanningReadable>,
        metadata: SessionMetadata,
        project_text: ProjectTextFn,
        page_size: Option<usize>,
        entry_types: Option<Vec<String>>,
    ) -> Self {
        let entry_types = entry_types.map(|types| types.into_iter().collect::<HashSet<_>>());
        let single_kind = match &entry_types {
            Some(types) if types.len() == 1 => types.iter().next().cloned(),
            _ => None,
        };
        Self {
            readable,
            metadata,
            project_text,
            page_size: page_size.unwrap_or(DEFAULT_PAGE_SIZE),
            after_seq: 0,
            entry_types,
            single_kind,
            page: Vec::new().into_iter(),
            done: false,
        }
    }
}

impl Iterator for ScanReadableEntries {
    type Item = Result<SessionSearchCandidate, SearchError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.page.next() {
                if let Some(types) = &self.entry_types {
                    if !types.contains(entry.kind()) {
                        continue;
                    }
                }
                let label = match self.readable.get_label(&entry.id) {
                    Ok(label) => label,
                    Err(error) => {
                        self.done = true;
                        return Some(Err(SearchError::Storage(error.to_string())));
                    }
                };
                let entry_kind = entry.kind().to_owned();
                let text = (self.project_text)(&self.metadata, &entry, label.as_deref());
                return Some(Ok(SessionSearchCandidate {
                    entry_id: entry.id,
                    seq: entry.seq,
                    entry_type: entry_kind,
                    timestamp: entry.timestamp,
                    text,
                    fields: label.map(|label| serde_json::json!({ "label": label })),
                }));
            }
            if self.done {
                return None;
            }
            let query = EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                limit: Some(self.page_size),
                after_seq: Some(self.after_seq),
                kind: self.single_kind.clone(),
                ..EntryQuery::default()
            };
            match self.readable.find_entries(&query) {
                Err(error) => {
                    self.done = true;
                    return Some(Err(SearchError::Storage(error.to_string())));
                }
                Ok(entries) => {
                    if entries.is_empty() {
                        self.done = true;
                        return None;
                    }
                    self.after_seq = entries
                        .last()
                        .map(|entry| entry.seq)
                        .unwrap_or(self.after_seq);
                    if entries.len() < self.page_size {
                        // Upstream breaks after yielding the short page.
                        self.done = true;
                    }
                    self.page = entries.into_iter();
                }
            }
        }
    }
}

/// Scan one readable into candidates (upstream `scanningEntries`). The
/// metadata is fetched eagerly here (upstream awaits it on the first pull).
pub fn scanning_entries(
    readable: Arc<dyn ScanningReadable>,
    options: ScanningReadableOptions,
) -> Result<ScanReadableEntries, SearchError> {
    let metadata = readable
        .get_metadata()
        .map_err(|error| SearchError::Storage(error.to_string()))?;
    Ok(ScanReadableEntries::new(
        readable,
        metadata,
        resolve_project_text(options.project_text),
        options.page_size,
        None,
    ))
}

/// Shared scanning search (upstream `createScanningSessionSearch`).
pub struct ScanningSessionSearch<H = ScanningSessionSearchHit> {
    source: ScanningReadableSource,
    project_text: Option<ProjectTextFn>,
    page_size: Option<usize>,
    matcher: Option<MatchFn>,
    create_hit: Option<CreateHitFn<H>>,
}

impl<H> ScanningSessionSearch<H> {
    pub fn new(source: ScanningReadableSource) -> Self {
        Self::with_options(source, ScanningSessionSearchOptions::default())
    }

    pub fn with_options(
        source: ScanningReadableSource,
        options: ScanningSessionSearchOptions<H>,
    ) -> Self {
        Self {
            source,
            project_text: options.project_text,
            page_size: options.page_size,
            matcher: options.matcher,
            create_hit: options.create_hit,
        }
    }
}

impl<H> ScanningSessionSearch<H>
where
    H: Send + From<ScanningSessionSearchHit> + 'static,
{
    /// Search committed entries (upstream the `search` async generator).
    pub fn search<'a>(
        &'a self,
        text: &'a str,
        search_options: SessionSearchOptions,
    ) -> BoxStream<'a, Result<H, SearchError>> {
        let normalized_text = text.trim().to_lowercase();
        if normalized_text.is_empty()
            || search_options.limit == Some(0)
            || search_options
                .entry_types
                .as_ref()
                .is_some_and(Vec::is_empty)
        {
            return futures::stream::empty().boxed();
        }
        let readables = match &self.source {
            ScanningReadableSource::Readables(list) => Readables::List(list.clone().into_iter()),
            ScanningReadableSource::SourceFn(source) => {
                Readables::Pending(source(&normalized_text, &search_options))
            }
        };
        let project_text = resolve_project_text(self.project_text.clone());
        // Upstream casts the default hit to THit; the port goes through From.
        let create_hit = self.create_hit.clone().unwrap_or_else(|| {
            Arc::new(
                |metadata: &SessionMetadata, candidate: &SessionSearchCandidate| {
                    ScanningSessionSearchHit {
                        session_id: metadata.id.clone(),
                        entry_id: candidate.entry_id.clone(),
                        timestamp: candidate.timestamp,
                        snippet: candidate.text.clone(),
                    }
                    .into()
                },
            )
        });
        ScanningSearchStream {
            normalized_text,
            entry_types: search_options
                .entry_types
                .map(|types| types.into_iter().collect()),
            limit: search_options.limit,
            hits: 0,
            seen: HashSet::new(),
            signal: search_options.signal,
            project_text,
            page_size: self.page_size,
            matcher: self.matcher.clone(),
            create_hit,
            readables,
            current: None,
            finished: false,
        }
        .boxed()
    }
}

impl<H> super::SessionSearch<H> for ScanningSessionSearch<H>
where
    H: Send + From<ScanningSessionSearchHit> + 'static,
{
    fn search<'a>(
        &'a self,
        text: &'a str,
        options: SessionSearchOptions,
    ) -> BoxStream<'a, Result<H, SearchError>> {
        ScanningSessionSearch::search(self, text, options)
    }
}

enum Readables {
    List(std::vec::IntoIter<Arc<dyn ScanningReadable>>),
    /// Source future not yet resolved (upstream generator body runs lazily
    /// on the first pull).
    Pending(ScanningReadableFuture),
    Dynamic(BoxStream<'static, Result<Arc<dyn ScanningReadable>, SearchError>>),
}

enum NextReadable {
    Item(Result<Arc<dyn ScanningReadable>, SearchError>),
    Exhausted,
}

struct CurrentReadable {
    metadata: SessionMetadata,
    scan: ScanReadableEntries,
}

struct ScanningSearchStream<H> {
    normalized_text: String,
    entry_types: Option<HashSet<String>>,
    limit: Option<usize>,
    hits: usize,
    seen: HashSet<String>,
    signal: Option<AbortSignal>,
    project_text: ProjectTextFn,
    page_size: Option<usize>,
    matcher: Option<MatchFn>,
    create_hit: CreateHitFn<H>,
    readables: Readables,
    current: Option<CurrentReadable>,
    finished: bool,
}

impl<H> ScanningSearchStream<H> {
    fn start_readable(
        &mut self,
        readable: Arc<dyn ScanningReadable>,
    ) -> Result<CurrentReadable, SearchError> {
        let metadata = readable
            .get_metadata()
            .map_err(|error| SearchError::Storage(error.to_string()))?;
        if !self.seen.insert(metadata.id.clone()) {
            return Err(SearchError::DuplicateSessionId(metadata.id.clone()));
        }
        let scan = ScanReadableEntries::new(
            readable,
            metadata.clone(),
            self.project_text.clone(),
            self.page_size,
            self.entry_types
                .clone()
                .map(|types| types.into_iter().collect()),
        );
        Ok(CurrentReadable { metadata, scan })
    }
}

impl<H: Send> Stream for ScanningSearchStream<H> {
    type Item = Result<H, SearchError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if this.finished {
                return Poll::Ready(None);
            }
            // Current readable: pull the next candidate. Abort checks run
            // after an item arrives, matching the upstream for-await bodies.
            if let Some(current) = &mut this.current {
                match current.scan.next() {
                    Some(Ok(candidate)) => {
                        if this.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
                            this.finished = true;
                            return Poll::Ready(Some(Err(SearchError::Aborted)));
                        }
                        if let Some(types) = &this.entry_types {
                            if !types.contains(candidate.entry_type.as_str()) {
                                continue;
                            }
                        }
                        let matched = match &this.matcher {
                            Some(matcher) => {
                                matcher(&this.normalized_text, &candidate, &current.metadata)
                            }
                            None => candidate
                                .text
                                .to_lowercase()
                                .contains(&this.normalized_text),
                        };
                        if !matched {
                            continue;
                        }
                        let hit = (this.create_hit)(&current.metadata, &candidate);
                        this.hits += 1;
                        if this.limit.is_some_and(|limit| this.hits >= limit) {
                            // Upstream returns after yielding the hit.
                            this.finished = true;
                        }
                        return Poll::Ready(Some(Ok(hit)));
                    }
                    Some(Err(error)) => {
                        this.finished = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                    None => this.current = None,
                }
            }
            let next = match &mut this.readables {
                Readables::List(list) => match list.next() {
                    Some(readable) => NextReadable::Item(Ok(readable)),
                    None => NextReadable::Exhausted,
                },
                Readables::Pending(future) => match future.poll_unpin(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => NextReadable::Item(Err(error)),
                    Poll::Ready(Ok(stream)) => {
                        this.readables = Readables::Dynamic(stream);
                        continue;
                    }
                },
                Readables::Dynamic(stream) => match stream.poll_next_unpin(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => NextReadable::Exhausted,
                    Poll::Ready(Some(item)) => NextReadable::Item(item),
                },
            };
            match next {
                NextReadable::Exhausted => {
                    this.finished = true;
                    return Poll::Ready(None);
                }
                NextReadable::Item(Err(error)) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(error)));
                }
                NextReadable::Item(Ok(readable)) => {
                    // Upstream `throwIfAborted` after each readable arrives.
                    if this.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
                        this.finished = true;
                        return Poll::Ready(Some(Err(SearchError::Aborted)));
                    }
                    match this.start_readable(readable) {
                        Ok(current) => this.current = Some(current),
                        Err(error) => {
                            this.finished = true;
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                }
            }
        }
    }
}
