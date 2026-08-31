//! Port of packages/agent/src/harness/session/jsonl/repo.ts (pi v0.84.3) —
//! create/open/list/delete/fork over cwd-encoded JSONL session files.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pillar_ai::uuid::uuidv7;

use super::super::memory::{Session, SessionCreateOptions};
use super::super::types::{ForkOptions, SessionError, SessionErrorCode};
use super::codec::parse_header;
use super::storage::{JsonlSessionStorage, metadata_from_header};
use super::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata,
    JsonlSessionRepoOptions, JsonlV4Header,
};
use crate::harness::types::FileSystem;

/// Wall-clock milliseconds provider (upstream tests fake `Date`; the port
/// injects the clock instead).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone)]
struct CreateDestination {
    id: String,
    cwd: String,
}

const SESSION_ID_PATTERN_STRICT: bool = true;

fn validate_session_id(id: &str) -> Result<(), SessionError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if !valid {
        return Err(SessionError::new(
            SessionErrorCode::InvalidPayload,
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    Ok(())
}

fn session_directory_name(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let encoded: String = trimmed
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{encoded}--")
}

fn session_file_name(created_at: u64, id: &str) -> String {
    // ISO timestamp with ':' and '.' replaced by '-' (upstream
    // `sessionFileName`).
    let millis = created_at % 1000;
    let secs = created_at / 1000;
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{millis:03}_{id}.jsonl"
    )
}

/// Days-since-epoch to (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
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

/// Held create/fork reservation for one logical destination (upstream
/// `claimCreateDestination` try/finally). The key is removed on drop so
/// failed creations release the reservation (upstream `finally`).
struct DestinationReservation<'a> {
    reservations: &'a Mutex<HashSet<String>>,
    key: String,
}

impl<'a> DestinationReservation<'a> {
    fn acquire(
        reservations: &'a Mutex<HashSet<String>>,
        destination: &CreateDestination,
    ) -> Result<Self, SessionError> {
        let key = format!("{}\0{}", destination.cwd, destination.id);
        let mut guard = reservations.lock().expect("create destinations lock");
        if !guard.insert(key.clone()) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {}", destination.id),
            ));
        }
        Ok(Self { reservations, key })
    }
}

impl Drop for DestinationReservation<'_> {
    fn drop(&mut self) {
        self.reservations
            .lock()
            .expect("create destinations lock")
            .remove(&self.key);
    }
}

/// JSONL session repository (upstream `JsonlSessionRepo`).
pub struct JsonlSessionRepo<F: FileSystem + ?Sized> {
    fs: Arc<F>,
    sessions_root_input: String,
    /// Same-process create/fork reservations per logical destination
    /// (upstream `activeCreateDestinations`). The durable filename includes
    /// a timestamp, so the async existence check alone can let two
    /// concurrent calls both decide the same `{cwd, id}` is free.
    active_create_destinations: Mutex<HashSet<String>>,
    clock: Clock,
}

impl<F: FileSystem + 'static> JsonlSessionRepo<F> {
    pub fn new(options: JsonlSessionRepoOptions<F>) -> Self
    where
        F: Sized,
    {
        Self {
            fs: options.fs,
            sessions_root_input: options.sessions_root,
            active_create_destinations: Mutex::new(HashSet::new()),
            clock: Arc::new(now_millis),
        }
    }

    /// Override the wall clock (upstream tests fake `Date.now`).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn sessions_root(&self) -> &str {
        &self.sessions_root_input
    }

    async fn absolute(&self, path: &str) -> Result<String, SessionError> {
        self.fs
            .absolute_path(path)
            .await
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
    }

    async fn join(&self, parts: &[&str]) -> Result<String, SessionError> {
        self.fs
            .join_path(parts)
            .await
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))
    }

    fn session_directory_sync(&self, root: &str, cwd: &str) -> String {
        let name = session_directory_name(cwd);
        if root.ends_with('/') {
            format!("{root}{name}")
        } else {
            format!("{root}/{name}")
        }
    }

    /// List session metadata without opening sessions (upstream
    /// `listJsonlSessionMetadata`).
    pub async fn list_metadata(
        &self,
        query: &JsonlSessionListOptions,
    ) -> Result<Vec<JsonlSessionMetadata>, SessionError> {
        let root = self.absolute(self.sessions_root()).await?;

        let directories: Vec<String> = match &query.cwd {
            Some(cwd) => {
                let resolved = self.absolute(cwd).await?;
                let directory = self.session_directory_sync(&root, &resolved);
                if self.fs.exists(&directory).await.unwrap_or(false) {
                    vec![directory]
                } else {
                    Vec::new()
                }
            }
            None => {
                if !self.fs.exists(&root).await.unwrap_or(false) {
                    Vec::new()
                } else {
                    self.fs
                        .list_dir(&root)
                        .await
                        .map_err(|error| {
                            SessionError::new(SessionErrorCode::Storage, error.to_string())
                        })?
                        .into_iter()
                        .filter(|entry| {
                            matches!(
                                entry.kind,
                                crate::harness::types::FileKind::Directory
                                    | crate::harness::types::FileKind::Symlink
                            )
                        })
                        .map(|entry| entry.path)
                        .collect()
                }
            }
        };

        let mut metadata: Vec<JsonlSessionMetadata> = Vec::new();
        for directory in directories {
            let files =
                self.fs.list_dir(&directory).await.map_err(|error| {
                    SessionError::new(SessionErrorCode::Storage, error.to_string())
                })?;
            for file in files {
                if matches!(file.kind, crate::harness::types::FileKind::Directory)
                    || !file.name.ends_with(".jsonl")
                {
                    continue;
                }
                let lines =
                    self.fs
                        .read_text_lines(&file.path, Some(1))
                        .await
                        .map_err(|error| {
                            SessionError::new(
                                SessionErrorCode::Storage,
                                format!("Failed to read session header {}: {error}", file.path),
                            )
                        })?;
                let Some(first_line) = lines.into_iter().next() else {
                    continue;
                };
                if let Ok(header) = parse_header(&first_line) {
                    metadata.push(metadata_from_header(&header, &file.path, file.mtime_ms));
                }
            }
        }
        metadata.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
        Ok(metadata)
    }

    /// Open a session for reading and writing (upstream
    /// `loadJsonlSessionStorage` + `open`).
    pub async fn open(&self, metadata: &JsonlSessionMetadata) -> Result<Session, SessionError> {
        let storage = self.load_storage(metadata).await?;
        Ok(Session::new(Box::new(storage)))
    }

    async fn load_storage(
        &self,
        metadata: &JsonlSessionMetadata,
    ) -> Result<JsonlSessionStorage<F>, SessionError> {
        if !self
            .fs
            .exists(&metadata.path)
            .await
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?
        {
            return Err(SessionError::new(
                SessionErrorCode::NotFound,
                format!("Session not found: {}", metadata.id),
            ));
        }
        let storage = JsonlSessionStorage::<F>::load(self.fs.clone(), &metadata.path).await?;
        let loaded = storage.get_metadata().await;
        if loaded.id != metadata.id {
            return Err(SessionError::new(
                SessionErrorCode::InvalidEntry,
                format!("Session id does not match header: {}", metadata.id),
            ));
        }
        Ok(storage)
    }

    /// Create a new session (upstream `create`).
    pub async fn create(
        &self,
        options: &JsonlSessionCreateOptions,
    ) -> Result<Session, SessionError> {
        let destination = self.resolve_create_destination(options).await?;
        let _reservation = self.claim_destination_guard(&destination)?;
        let (header, path) = self.prepare_create(&destination, options).await?;
        let storage = JsonlSessionStorage::<F>::create(self.fs.clone(), &path, &header).await?;
        Ok(Session::new(Box::new(storage)))
    }

    /// Fork a session into a new one (upstream `fork`).
    pub async fn fork(
        &self,
        source: &JsonlSessionMetadata,
        options: &JsonlSessionCreateOptions,
        fork_options: &ForkOptions,
    ) -> Result<Session, SessionError> {
        let source_storage = self.load_storage(source).await?;
        let mut create_options = options.clone();
        create_options.parent_session_id = Some(
            create_options
                .parent_session_id
                .unwrap_or_else(|| source.id.clone()),
        );
        let destination = self.resolve_create_destination(&create_options).await?;
        let _reservation = self.claim_destination_guard(&destination)?;
        let (header, path) = self.prepare_create(&destination, &create_options).await?;
        let storage = source_storage.fork(&path, &header, fork_options).await?;
        Ok(Session::new(Box::new(storage)))
    }

    /// Prevent same-process create/fork races for one logical destination
    /// (upstream `claimCreateDestination`). The reservation set lives on the
    /// repo; the guard removes the key when dropped.
    fn claim_destination_guard<'a>(
        &'a self,
        destination: &'a CreateDestination,
    ) -> Result<DestinationReservation<'a>, SessionError> {
        DestinationReservation::acquire(&self.active_create_destinations, destination)
    }

    /// Delete a session file (upstream `delete`).
    pub async fn delete(&self, metadata: &JsonlSessionMetadata) -> Result<(), SessionError> {
        self.fs
            .remove(&metadata.path, false, true)
            .await
            .map_err(|error| {
                SessionError::new(
                    SessionErrorCode::Storage,
                    format!("Failed to delete session {}: {error}", metadata.path),
                )
            })
    }

    async fn resolve_create_destination(
        &self,
        options: &JsonlSessionCreateOptions,
    ) -> Result<CreateDestination, SessionError> {
        let id = options.id.clone().unwrap_or_else(uuidv7);
        validate_session_id(&id)?;
        let cwd = self.absolute(&options.cwd).await?;
        Ok(CreateDestination { id, cwd })
    }

    async fn session_id_exists(&self, id: &str, cwd: &str) -> Result<bool, SessionError> {
        let suffix = format!("_{id}.jsonl");
        let directory = self.session_directory_sync(self.sessions_root(), cwd);
        if !self.fs.exists(&directory).await.unwrap_or(false) {
            return Ok(false);
        }
        let files = self
            .fs
            .list_dir(&directory)
            .await
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
        Ok(files.iter().any(|entry| {
            !matches!(entry.kind, crate::harness::types::FileKind::Directory)
                && entry.name.ends_with(&suffix)
        }))
    }

    async fn prepare_create(
        &self,
        destination: &CreateDestination,
        options: &JsonlSessionCreateOptions,
    ) -> Result<(JsonlV4Header, String), SessionError> {
        let CreateDestination { id, cwd } = destination;
        if self.session_id_exists(id, cwd).await? {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }

        let created_at = (self.clock)();
        let directory = self.session_directory_sync(self.sessions_root(), cwd);
        let path = self
            .join(&[&directory, &session_file_name(created_at, id)])
            .await?;

        let header = JsonlV4Header {
            kind: "header".to_owned(),
            version: 4,
            id: id.clone(),
            created_at,
            cwd: cwd.clone(),
            parent_session_id: options.parent_session_id.clone(),
            legacy_parent_session_path: None,
            metadata: options.metadata.clone(),
        };
        self.fs
            .create_dir(&directory, true)
            .await
            .map_err(|error| SessionError::new(SessionErrorCode::Storage, error.to_string()))?;
        Ok((header, path))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Base create options are reused through [`JsonlSessionCreateOptions`];
/// kept for layout parity with upstream imports.
#[allow(dead_code)]
type _SessionCreateOptionsWitness = SessionCreateOptions;

/// The upstream repo validates ids against `/^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$/`;
/// `validate_session_id` implements the same rule.
#[allow(dead_code)]
const _SESSION_ID_PATTERN_DOC: bool = SESSION_ID_PATTERN_STRICT;
