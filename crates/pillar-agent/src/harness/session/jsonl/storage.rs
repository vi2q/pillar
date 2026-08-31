//! Port of packages/agent/src/harness/session/jsonl/storage.ts (pi v0.84.3)
//! — file-backed `SessionStorage` over a single JSONL file per session.

use std::sync::{Arc, Mutex};

use super::super::memory::SessionStorage;
use super::super::memory::{ProvisionedEntry, ProvisionedRecord};
use super::super::state::{SessionMutation, SessionState};
use super::super::types::{
    BranchBounds, Entry, EntryQuery, LanePointer, LaneRecord, LogItem, LogOptions, RecordQuery,
    SessionError, SessionErrorCode, SessionMetadata, SessionStats,
};
use super::codec::{encode_header, encode_mutation, invalid_file, parse_header, parse_mutation};
use super::types::{JsonlSessionMetadata, JsonlV4Header};
use crate::harness::types::FileSystem;

/// File-backed session storage (upstream `JsonlSessionStorage`).
/// Generic over the FS backend because `FileSystem` uses RPITIT methods and
/// is not dyn-compatible (docs/INSTRUCTIONS.md #41).
pub struct JsonlSessionStorage<F: FileSystem + ?Sized> {
    fs: Arc<F>,
    metadata: JsonlSessionMetadata,
    state: Mutex<SessionState>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn storage_err(message: impl Into<String>) -> SessionError {
    SessionError::new(SessionErrorCode::Storage, message)
}

impl<FT: FileSystem + ?Sized> JsonlSessionStorage<FT> {
    /// Create a fresh session file with only the header (upstream
    /// `JsonlSessionStorage.create`).
    pub async fn create(
        fs: Arc<FT>,
        path: &str,
        header: &JsonlV4Header,
    ) -> Result<Self, SessionError> {
        fs.write_file(path, encode_header(header).as_bytes())
            .await
            .map_err(|error| {
                storage_err(format!("Failed to initialize session {path}: {error}"))
            })?;
        let info = fs.file_info(path).await.map_err(|error| {
            storage_err(format!("Failed to read session metadata {path}: {error}"))
        })?;
        Ok(Self::new(
            fs,
            metadata_from_header(header, path, info.mtime_ms),
        ))
    }

    /// Load a session file, replaying every mutation line (upstream
    /// `JsonlSessionStorage.load`). A torn final line (truncated append) is
    /// repaired by atomically republishing the valid prefix.
    pub async fn load(fs: Arc<FT>, path: &str) -> Result<Self, SessionError> {
        let content = fs
            .read_text_file(path)
            .await
            .map_err(|error| storage_err(format!("Failed to read session {path}: {error}")))?;

        let mut physical_lines: Vec<&str> = content.split('\n').collect();
        if physical_lines.last() == Some(&"") {
            physical_lines.pop();
        }
        if physical_lines.is_empty() || physical_lines[0].is_empty() {
            return Err(invalid_file(
                path,
                1,
                &super::codec::JsonlDecodeError::schema("is missing a header"),
            ));
        }

        let header =
            parse_header(physical_lines[0]).map_err(|error| invalid_file(path, 1, &error))?;
        let info = fs.file_info(path).await.map_err(|error| {
            storage_err(format!("Failed to read session metadata {path}: {error}"))
        })?;
        let storage = Self::new(
            fs.clone(),
            metadata_from_header(&header, path, info.mtime_ms),
        );

        let last_index = physical_lines.len() - 1;
        for (index, line) in physical_lines.iter().enumerate().skip(1) {
            match parse_mutation(line) {
                Ok(mutation) => {
                    if let Err(error) = storage.lock_state().apply_mutation(mutation) {
                        if error.code == SessionErrorCode::InvalidEntry {
                            return Err(invalid_file(
                                path,
                                index + 1,
                                &super::codec::JsonlDecodeError::schema(&error.message),
                            ));
                        }
                        return Err(error);
                    }
                }
                Err(error) => {
                    let is_torn_tail =
                        index == last_index && error.kind == super::codec::JsonlDecodeKind::Syntax;
                    if is_torn_tail {
                        // Drop the unacknowledged partial append by atomically
                        // publishing the valid prefix.
                        let valid_prefix = format!("{}\n", physical_lines[..index].join("\n"));
                        publish_file_atomically(&*fs, path, &valid_prefix).await?;
                        return Ok(storage);
                    }
                    return Err(invalid_file(path, index + 1, &error));
                }
            }
        }

        if !content.ends_with('\n') {
            fs.append_file(path, b"\n").await.map_err(|error| {
                storage_err(format!(
                    "Failed to repair unterminated session tail {path}: {error}"
                ))
            })?;
        }
        Ok(storage)
    }

    fn new(fs: Arc<FT>, metadata: JsonlSessionMetadata) -> Self {
        Self {
            fs,
            metadata,
            state: Mutex::new(SessionState::new()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().expect("session state lock")
    }

    /// Write a fork file: copy the fork mutations from this session's state
    /// into a fresh file under the new header (upstream `fork`).
    pub async fn fork(
        &self,
        path: &str,
        header: &JsonlV4Header,
        options: &super::super::types::ForkOptions,
    ) -> Result<Self, SessionError> {
        let mutations = {
            let state = self.lock_state();
            state.create_fork_mutations(options)?
        };
        let temp_path = format!("{path}.tmp");
        let staged = staged_create(self.fs.clone(), &temp_path, header).await?;
        for mutation in &mutations {
            self.fs
                .append_file(&temp_path, encode_mutation(mutation).as_bytes())
                .await
                .map_err(|error| {
                    storage_err(format!("Failed to append session {temp_path}: {error}"))
                })?;
            staged.lock_state().apply_mutation(mutation.clone())?;
        }
        self.fs
            .rename_file(&temp_path, path)
            .await
            .map_err(|error| {
                storage_err(format!("Failed to publish staged file {path}: {error}"))
            })?;
        Self::load(self.fs.clone(), path).await
    }

    /// Wait for all queued writes to complete (upstream `drain`).
    pub async fn drain(&self) {
        // Writes are awaited inline by the caller through `enqueue`; nothing
        // is buffered beyond the in-flight call, so draining is a no-op.
    }

    pub async fn get_metadata(&self) -> JsonlSessionMetadata {
        self.metadata.clone()
    }
}

/// Sized-generic wrapper so `?Sized` storage can stage a fork file.
fn staged_create<FT: FileSystem + ?Sized>(
    fs: Arc<FT>,
    path: &str,
    header: &JsonlV4Header,
) -> impl std::future::Future<Output = Result<JsonlSessionStorage<FT>, SessionError>> {
    JsonlSessionStorage::<FT>::create(fs, path, header)
}

/// Build a complete sibling temporary file, then atomically rename it over
/// the destination (upstream `publishFileAtomically`).
async fn publish_file_atomically<F2: FileSystem + ?Sized>(
    fs: &F2,
    destination_path: &str,
    content: &str,
) -> Result<(), SessionError> {
    let temp_path = format!("{destination_path}.tmp");
    let result = async {
        fs.write_file(&temp_path, content.as_bytes())
            .await
            .map_err(|error| {
                storage_err(format!(
                    "Failed to stage torn-tail repair {destination_path}: {error}"
                ))
            })?;
        fs.rename_file(&temp_path, destination_path)
            .await
            .map_err(|error| {
                storage_err(format!(
                    "Failed to publish staged file {destination_path}: {error}"
                ))
            })?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs.remove(&temp_path, false, true).await;
    }
    result
}

/// Build session metadata from a header (upstream `metadataFromHeader`).
pub fn metadata_from_header(
    header: &JsonlV4Header,
    path: &str,
    modified_at: u64,
) -> JsonlSessionMetadata {
    JsonlSessionMetadata {
        id: header.id.clone(),
        created_at: header.created_at,
        parent_session_id: header.parent_session_id.clone(),
        cwd: header.cwd.clone(),
        path: path.to_owned(),
        modified_at,
        source_format: 4,
        legacy_parent_session_path: header.legacy_parent_session_path.clone(),
        metadata: header.metadata.clone(),
    }
}

impl<F: FileSystem + 'static> SessionStorage for JsonlSessionStorage<F> {
    fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        Ok(SessionMetadata {
            id: self.metadata.id.clone(),
            created_at: self.metadata.created_at,
            parent_session_id: self.metadata.parent_session_id.clone(),
        })
    }

    fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        Ok(self.lock_state().get_lanes())
    }

    fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.lock_state();
        state.validate_new_lane(lane)?;
        state.validate_target(at)?;
        let mutation = SessionMutation::Lane {
            seq: state.next_sequence(),
            lane: lane.to_owned(),
            leaf_id: at.map(str::to_owned),
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)
    }

    fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.lock_state();
        state.require_lane(lane)?;
        state.validate_target(to)?;
        let mutation = SessionMutation::Lane {
            seq: state.next_sequence(),
            lane: lane.to_owned(),
            leaf_id: to.map(str::to_owned),
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)
    }

    fn append_entry(&self, new_entry: ProvisionedEntry, lane: &str) -> Result<Entry, SessionError> {
        let mut state = self.lock_state();
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&new_entry.id)?;
        let entry = Entry {
            id: new_entry.id,
            seq: state.next_sequence(),
            parent_id,
            timestamp: now_millis(),
            payload: new_entry.payload,
        };
        let mutation = SessionMutation::Entry {
            lane: Some(lane.to_owned()),
            entry: entry.clone(),
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)?;
        Ok(entry)
    }

    fn append_record(&self, new_record: ProvisionedRecord) -> Result<LaneRecord, SessionError> {
        let mut state = self.lock_state();
        state.require_lane(&new_record.lane)?;
        state.validate_unused_id(&new_record.id)?;
        let current_open_operation_id = state
            .find_open_operations(&new_record.lane, Some(1))?
            .into_iter()
            .next()
            .map(|record| record.id);
        if matches!(
            new_record.payload,
            super::super::types::RecordPayload::OperationStarted { .. }
        ) && current_open_operation_id.is_some()
        {
            return Err(storage_err(format!(
                "Lane {} already has an open operation {}",
                new_record.lane,
                current_open_operation_id.unwrap_or_default()
            )));
        }
        let record = LaneRecord {
            id: new_record.id,
            seq: state.next_sequence(),
            lane: new_record.lane,
            timestamp: now_millis(),
            payload: new_record.payload,
        };
        let mutation = SessionMutation::Record {
            record: record.clone(),
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)?;
        Ok(record)
    }

    fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        Ok(self.lock_state().get_entry(id).cloned())
    }

    fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.lock_state().find_entries(query)
    }

    fn find_entries_on_branch(
        &self,
        start: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        self.lock_state()
            .find_entries_on_branch(start, query, bounds)
    }

    fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        self.lock_state().find_records(query)
    }

    fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        self.lock_state().find_open_operations(lane, limit)
    }

    fn get_log(&self, options: &LogOptions) -> Result<Vec<LogItem>, SessionError> {
        self.lock_state().get_log(options.after_seq, options.limit)
    }

    fn get_name(&self) -> Result<Option<String>, SessionError> {
        Ok(self.lock_state().get_name().map(str::to_owned))
    }

    fn set_name(&self, name: Option<String>) -> Result<(), SessionError> {
        let mut state = self.lock_state();
        let mutation = SessionMutation::Name {
            seq: state.next_sequence(),
            name,
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)
    }

    fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        Ok(self.lock_state().get_label(id).map(str::to_owned))
    }

    fn set_label(&self, id: &str, label: Option<String>) -> Result<(), SessionError> {
        let mut state = self.lock_state();
        state.validate_target(Some(id))?;
        let mutation = SessionMutation::Label {
            seq: state.next_sequence(),
            target_id: id.to_owned(),
            label,
        };
        self.append_mutation_blocking(&mutation)?;
        state.apply_mutation(mutation)
    }

    fn get_stats(&self) -> Result<SessionStats, SessionError> {
        Ok(*self.lock_state().get_stats())
    }
}

impl<FT: FileSystem + 'static> JsonlSessionStorage<FT> {
    /// Synchronous append used by the `SessionStorage` trait methods. The
    /// upstream storage chains every write through a promise queue; the
    /// port serializes on the state mutex (callers hold `lock_state()`)
    /// and runs the async FS call on the blocking pool via
    /// `Handle::spawn_blocking` driven by an inline current-thread runtime,
    /// which works from both multi-thread and current-thread runtimes
    /// (including `#[tokio::test]`). A std fallback covers callers outside
    /// any runtime. Requires `FT: 'static`-compatible bounds (i.e. a sized
    /// owned FS), hence the dedicated impl block.
    fn append_mutation_blocking(&self, mutation: &SessionMutation) -> Result<(), SessionError> {
        let encoded = encode_mutation(mutation);
        let path = self.metadata.path.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let fs = self.fs.clone();
                let task_path = path.clone();
                let task_encoded = encoded.clone();
                futures::executor::block_on(handle.spawn_blocking(move || {
                    let append =
                        async move { fs.append_file(&task_path, task_encoded.as_bytes()).await };
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map(|runtime| runtime.block_on(append))
                        .unwrap_or_else(|error| {
                            Err(crate::harness::types::FileError::new(
                                crate::harness::types::FileErrorCode::Unknown,
                                format!("Failed to build append runtime: {error}"),
                                None,
                            ))
                        })
                }))
                .unwrap_or_else(|error| {
                    Err(crate::harness::types::FileError::new(
                        crate::harness::types::FileErrorCode::Unknown,
                        format!("Failed to join append task: {error}"),
                        None,
                    ))
                })
            }
            Err(_) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, encoded.as_bytes()))
                .map_err(|error| {
                    crate::harness::types::FileError::new(
                        crate::harness::types::FileErrorCode::Unknown,
                        error.to_string(),
                        None,
                    )
                }),
        };
        result.map_err(|error| storage_err(format!("Failed to append session {path}: {error}")))
    }
}
