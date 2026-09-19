//! Host ports for the Rust workflow tools (design §2, "host portsの最小責任").
//!
//! The tool handlers are pure logic over these traits: they never spawn Cargo,
//! read a file or open a socket themselves. A CLI or an embedded harness
//! supplies the adapters, and the in-memory implementations here are the
//! deterministic test doubles and the seed of a real host.
//!
//! The contracts follow the design's minimal-responsibility table:
//!
//! - [`WorkspaceCatalogPort`] reads *saved* metadata and resolves approved
//!   configurations. It does not fetch metadata: a refresh is an explicit,
//!   effect-gated host operation (design §9).
//! - [`CargoJobBroker`] starts only authorized argv, and reports progress,
//!   retained output, exit and cancellation. It does not claim a run was
//!   side-effect free (design §9).
//! - [`SourceSnapshotPort`] reads versioned source text for a diagnostic
//!   location; it does not imply the whole workspace is a consistent snapshot
//!   (design §3).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::diagnostic::{BuildStatus, TestStatus};
use super::error::RustToolError;
use super::metadata::{Configuration, WorkspaceCatalog};

/// Who owns a run. Refusals must not reveal another owner's target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerId(String);

impl OwnerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A broker-assigned run identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The caller's id for one start attempt (design §7.2, §10).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The broker's state machine (design §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Planned,
    Authorized,
    Queued,
    Running,
    Cancelling,
    Cancelled,
    Exited,
    /// The process is gone but its result was not collected.
    Lost,
    /// The process may or may not have run; never reported as success.
    OutcomeUnknown,
}

impl RunState {
    /// The wire form (matches the `serde` name).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Authorized => "authorized",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Exited => "exited",
            Self::Lost => "lost",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// Within `Running`, build and test are distinguishable (design §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Build,
    Test,
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitStatus {
    Exited { code: i32 },
    Signaled { signal: i32 },
    Unknown,
}

/// Which output stream a page comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// An authorized start (design §4, §9). The host re-checks at this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    pub owner: OwnerId,
    pub plan_id: String,
    pub step_id: String,
    pub request_id: RequestId,
    pub configuration_id: String,
    /// The metadata digest the plan was made against; the host compares it.
    pub metadata_digest: String,
    /// The configuration fingerprint the plan was made against.
    pub configuration_fingerprint: String,
    pub argv: Vec<String>,
    /// A digest of `argv`, for duplicate-start suppression (design §10).
    pub command_digest: u64,
}

/// A run as the broker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunRecord {
    pub run_id: RunId,
    pub owner: OwnerId,
    pub plan_id: String,
    pub step_id: String,
    pub request_id: RequestId,
    pub configuration_id: String,
    pub metadata_digest: String,
    pub argv: Vec<String>,
    pub state: RunState,
    pub phase: RunPhase,
    pub exit_status: Option<ExitStatus>,
    pub build_status: BuildStatus,
    pub test_status: TestStatus,
    /// Bytes of raw output the broker still retains.
    pub retained_bytes: usize,
}

/// One page of retained output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutputPage {
    pub stream: OutputStream,
    pub offset: usize,
    pub bytes: Vec<u8>,
    pub total_bytes: usize,
    /// More bytes exist past this page.
    pub truncated: bool,
    /// The artifact was evicted or never retained (design §6.2).
    pub expired: bool,
}

/// The complete retained output of a run, for normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawRunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Retention hit a cap; the output is not the full run (design §6.2).
    pub truncated: bool,
}

/// A read source range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceSlice {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: u64,
    pub text: String,
}

/// Saved metadata and approved configurations (design §2).
pub trait WorkspaceCatalogPort: Send + Sync {
    fn catalog(&self) -> Result<Arc<WorkspaceCatalog>, RustToolError>;
    fn configurations(&self) -> Result<Vec<Configuration>, RustToolError>;
}

/// Authorized Cargo execution and output retention (design §2, §9).
#[async_trait]
pub trait CargoJobBroker: Send + Sync {
    /// Start an authorized argv. Same owner + request id + command digest joins
    /// or returns the existing run; the same id with a different command is
    /// refused (design §10).
    async fn start(&self, request: StartRequest) -> Result<RunRecord, RustToolError>;

    /// Current state. The owner is checked; another owner is `permission_denied`.
    async fn status(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError>;

    /// A page of retained output.
    async fn output(
        &self,
        owner: &OwnerId,
        run_id: &RunId,
        stream: OutputStream,
        offset: usize,
        limit_bytes: usize,
    ) -> Result<OutputPage, RustToolError>;

    /// Cancel propagation. Acceptance is not proof the process stopped; the
    /// returned state says what the broker knows (design §9).
    async fn cancel(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError>;

    /// The retained output for normalization. `rs_diagnostics` never starts a
    /// run; it reads what this returns.
    async fn raw(&self, owner: &OwnerId, run_id: &RunId) -> Result<RawRunOutput, RustToolError>;
}

/// Versioned source text for a diagnostic location (design §2).
pub trait SourceSnapshotPort: Send + Sync {
    fn read_range(
        &self,
        path: &str,
        start_line: u64,
        line_count: u64,
    ) -> Result<SourceSlice, RustToolError>;
}

// --- in-memory fakes -------------------------------------------------------

/// An in-memory saved-metadata host.
#[derive(Debug, Default)]
pub struct MemoryWorkspace {
    catalog_json: Mutex<String>,
    configurations: Mutex<Vec<Configuration>>,
}

impl MemoryWorkspace {
    pub fn new(catalog_json: impl Into<String>) -> Self {
        Self {
            catalog_json: Mutex::new(catalog_json.into()),
            configurations: Mutex::new(Vec::new()),
        }
    }

    pub fn with_configurations(self, configurations: Vec<Configuration>) -> Self {
        *self.configurations.lock().expect("configurations lock") = configurations;
        self
    }

    /// Replace the saved metadata, as a refresh would (design §4: a plan made
    /// against the old digest must be refused).
    pub fn set_catalog_json(&self, catalog_json: impl Into<String>) {
        *self.catalog_json.lock().expect("catalog lock") = catalog_json.into();
    }

    pub fn set_configurations(&self, configurations: Vec<Configuration>) {
        *self.configurations.lock().expect("configurations lock") = configurations;
    }
}

impl WorkspaceCatalogPort for MemoryWorkspace {
    fn catalog(&self) -> Result<Arc<WorkspaceCatalog>, RustToolError> {
        let json = self.catalog_json.lock().expect("catalog lock").clone();
        Ok(Arc::new(WorkspaceCatalog::from_json(&json)?))
    }

    fn configurations(&self) -> Result<Vec<Configuration>, RustToolError> {
        Ok(self
            .configurations
            .lock()
            .expect("configurations lock")
            .clone())
    }
}

/// What a scripted run emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedRun {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: i32,
    pub build_status: BuildStatus,
    pub test_status: TestStatus,
}

impl ScriptedRun {
    pub fn finished(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
        exit_status: i32,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_status,
            build_status: BuildStatus::Succeeded,
            test_status: TestStatus::Unknown,
        }
    }

    pub fn with_statuses(mut self, build: BuildStatus, test: TestStatus) -> Self {
        self.build_status = build;
        self.test_status = test;
        self
    }
}

#[derive(Default)]
struct BrokerState {
    runs: HashMap<RunId, RunRecord>,
    outputs: HashMap<RunId, RawRunOutput>,
    order: VecDeque<RunId>,
    requests: HashMap<(String, String), (u64, RunId)>,
    counter: u64,
}

/// An in-memory broker for tests and hosts without a process boundary.
///
/// The authorizer is explicit: a broker that has not been told to authorize
/// refuses every start, mirroring "the host owns the trust decision" rather
/// than defaulting to permission.
pub struct MemoryBroker {
    inner: Mutex<BrokerState>,
    authorized: AtomicBool,
    script: Mutex<ScriptedRun>,
    capacity: usize,
}

impl MemoryBroker {
    /// An authorizing broker whose runs finish immediately with `script`.
    pub fn new(script: ScriptedRun) -> Self {
        Self {
            inner: Mutex::new(BrokerState::default()),
            authorized: AtomicBool::new(true),
            script: Mutex::new(script),
            capacity: 16,
        }
    }

    /// A broker with nothing authorized yet.
    pub fn unauthorized(script: ScriptedRun) -> Self {
        let broker = Self::new(script);
        broker.authorized.store(false, Ordering::SeqCst);
        broker
    }

    pub fn set_authorized(&self, authorized: bool) {
        self.authorized.store(authorized, Ordering::SeqCst);
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    fn find<'a>(
        state: &'a BrokerState,
        owner: &OwnerId,
        run_id: &RunId,
    ) -> Result<&'a RunRecord, RustToolError> {
        match state.runs.get(run_id) {
            Some(record) if &record.owner == owner => Ok(record),
            // Do not distinguish "missing" from "another owner": a refusal
            // must not describe the target (design §3).
            _ => Err(RustToolError::permission_denied(
                "no such run for this session",
            )),
        }
    }
}

#[async_trait]
impl CargoJobBroker for MemoryBroker {
    async fn start(&self, request: StartRequest) -> Result<RunRecord, RustToolError> {
        if !self.authorized.load(Ordering::SeqCst) {
            return Err(RustToolError::permission_denied(
                "Cargo execution is not authorized for this workspace",
            ));
        }
        let mut state = self.inner.lock().expect("broker lock");
        let key = (
            request.owner.as_str().to_string(),
            request.request_id.as_str().to_string(),
        );
        if let Some((digest, run_id)) = state.requests.get(&key) {
            if *digest != request.command_digest {
                return Err(RustToolError::operation_id_mismatch(
                    "this request id was used before with a different command",
                ));
            }
            return Ok(state.runs.get(run_id).expect("recorded run").clone());
        }

        let script = self.script.lock().expect("script lock").clone();
        state.counter += 1;
        let run_id = RunId::new(format!("run{}", state.counter));
        let output = RawRunOutput {
            stdout: script.stdout,
            stderr: script.stderr,
            truncated: false,
        };
        let record = RunRecord {
            run_id: run_id.clone(),
            owner: request.owner.clone(),
            plan_id: request.plan_id,
            step_id: request.step_id,
            request_id: request.request_id,
            configuration_id: request.configuration_id,
            metadata_digest: request.metadata_digest,
            argv: request.argv,
            state: RunState::Exited,
            phase: RunPhase::Build,
            exit_status: Some(ExitStatus::Exited {
                code: script.exit_status,
            }),
            build_status: script.build_status,
            test_status: script.test_status,
            retained_bytes: output.stdout.len() + output.stderr.len(),
        };
        state
            .requests
            .insert(key, (request.command_digest, run_id.clone()));
        state.outputs.insert(run_id.clone(), output);
        state.runs.insert(run_id.clone(), record.clone());
        state.order.push_back(run_id);
        while state.order.len() > self.capacity {
            if let Some(evicted) = state.order.pop_front() {
                state.runs.remove(&evicted);
                state.outputs.remove(&evicted);
            }
        }
        Ok(record)
    }

    async fn status(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError> {
        let state = self.inner.lock().expect("broker lock");
        Ok(Self::find(&state, owner, run_id)?.clone())
    }

    async fn output(
        &self,
        owner: &OwnerId,
        run_id: &RunId,
        stream: OutputStream,
        offset: usize,
        limit_bytes: usize,
    ) -> Result<OutputPage, RustToolError> {
        let state = self.inner.lock().expect("broker lock");
        Self::find(&state, owner, run_id)?;
        let Some(output) = state.outputs.get(run_id) else {
            return Ok(OutputPage {
                stream,
                offset,
                bytes: Vec::new(),
                total_bytes: 0,
                truncated: false,
                expired: true,
            });
        };
        let source = match stream {
            OutputStream::Stdout => &output.stdout,
            OutputStream::Stderr => &output.stderr,
        };
        let total_bytes = source.len();
        let start = offset.min(total_bytes);
        let end = start.saturating_add(limit_bytes).min(total_bytes);
        Ok(OutputPage {
            stream,
            offset: start,
            bytes: source[start..end].to_vec(),
            total_bytes,
            truncated: end < total_bytes,
            expired: false,
        })
    }

    async fn cancel(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError> {
        let mut state = self.inner.lock().expect("broker lock");
        let record = Self::find(&state, owner, run_id)?.clone();
        let cancelled = if matches!(record.state, RunState::Exited | RunState::Cancelled) {
            record
        } else {
            let mut cancelled = record;
            cancelled.state = RunState::Cancelled;
            cancelled.phase = RunPhase::Build;
            state.runs.insert(run_id.clone(), cancelled.clone());
            cancelled
        };
        Ok(cancelled)
    }

    async fn raw(&self, owner: &OwnerId, run_id: &RunId) -> Result<RawRunOutput, RustToolError> {
        let state = self.inner.lock().expect("broker lock");
        Self::find(&state, owner, run_id)?;
        Ok(state.outputs.get(run_id).cloned().unwrap_or(RawRunOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: true,
        }))
    }
}

/// An in-memory source host for tests.
#[derive(Debug, Default)]
pub struct MemorySources {
    files: Mutex<HashMap<String, String>>,
}

impl MemorySources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_file(&self, path: impl Into<String>, text: impl Into<String>) {
        self.files
            .lock()
            .expect("files lock")
            .insert(path.into(), text.into());
    }
}

impl SourceSnapshotPort for MemorySources {
    fn read_range(
        &self,
        path: &str,
        start_line: u64,
        line_count: u64,
    ) -> Result<SourceSlice, RustToolError> {
        let files = self.files.lock().expect("files lock");
        let text = files.get(path).ok_or_else(|| {
            RustToolError::source_unbound("the location is not an authorized workspace file")
        })?;
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let total_lines = lines.len() as u64;
        if start_line == 0 || start_line > total_lines.max(1) {
            return Err(RustToolError::invalid_request(
                "source range starts past the end of the file",
            ));
        }
        let start = (start_line - 1) as usize;
        let end = (start + line_count.max(1) as usize).min(lines.len());
        Ok(SourceSlice {
            path: path.to_string(),
            start_line,
            end_line: end as u64,
            total_lines,
            text: lines[start..end].concat(),
        })
    }
}
