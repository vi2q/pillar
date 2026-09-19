//! Native host adapters for the Rust workflow tools (design §2 host ports,
//! §9 authorization and isolation).
//!
//! The pure core ([`pillar_agent::rust_tools`]) never spawns a process or
//! reads a file. This module is the development host that does:
//!
//! - [`NativeMetadataHost`] reads *saved* `cargo metadata` and refreshes it
//!   only through an explicit, effect-gated [`NativeMetadataHost::refresh`].
//!   Planning itself never launches Cargo (design §4).
//! - [`NativeCargoBroker`] runs an authorized `cargo test` argv through the
//!   shared [`exec_command`], so output is bounded and the abort signal
//!   cancels it (design §9). It refuses a program or subcommand it does not
//!   recognize, and it refuses everything until the effect authorizer allows
//!   the `Exec` intent.
//! - [`NativeSources`] reads a workspace file behind the `FsRead` intent and
//!   refuses a path outside the workspace root.
//!
//! Native-only: the embedding profiles have no process or filesystem
//! capability (docs/DEVELOPMENT-STRATEGY.md §4).
//!
//! Known R1 limits, recorded so they are not mistaken for finished behavior:
//! cancellation aborts the cargo process but not its process group (the
//! `sb265` grandchild fix), and the retained output becomes pageable when the
//! process exits, not incrementally while it runs. Moving to a streamed,
//! process-group execution is R4.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use pillar_agent::abort::AbortSignal;
use pillar_agent::rust_tools::host::{
    CargoJobBroker, ExitStatus, OutputPage, OutputStream, OwnerId, RawRunOutput, RunId, RunPhase,
    RunRecord, RunState, SourceSlice, SourceSnapshotPort, StartRequest, WorkspaceCatalogPort,
};
use pillar_agent::rust_tools::{
    Configuration, RustToolError, RustToolLimits, RustToolkit, WorkspaceCatalog,
};

use crate::core::effects::{EffectAuthorizer, EffectDecision, EffectIntent};
use crate::core::exec::{
    DRAIN_GRACE, ExecOptions, POLL_INTERVAL, TERM_GRACE, exec_command, hard_kill, terminate,
};
use crate::core::rust_analyzer::DocumentReader;

/// The default `cargo metadata` invocation (`--format-version 1` is stable).
pub const METADATA_COMMAND: [&str; 4] = ["cargo", "metadata", "--format-version", "1"];
/// The subcommands the broker will run. `rs_verify_plan` only produces `test`;
/// `check` is allowed for a caller that plans a compile-only step.
pub const BROKER_SUBCOMMANDS: [&str; 2] = ["test", "check"];

fn authorize(
    authorizer: Option<&EffectAuthorizer>,
    intent: EffectIntent,
) -> Result<(), RustToolError> {
    let Some(authorizer) = authorizer else {
        // A host that owns the trust decision may omit the authorizer, but
        // then it must have decided the workspace is trusted before building
        // this adapter.
        return Ok(());
    };
    match authorizer(&intent) {
        EffectDecision::Allow => Ok(()),
        EffectDecision::Deny { reason } => Err(RustToolError::permission_denied(format!(
            "execution refused: {reason}"
        ))),
    }
}

// --- metadata --------------------------------------------------------------

/// Reads saved `cargo metadata`, with an explicit effect-gated refresh.
pub struct NativeMetadataHost {
    cwd: String,
    authorizer: Option<EffectAuthorizer>,
    command: Vec<String>,
    timeout_ms: Option<u64>,
    saved: Mutex<Option<String>>,
    configurations: Mutex<Vec<Configuration>>,
}

impl NativeMetadataHost {
    pub fn new(cwd: impl Into<String>, authorizer: Option<EffectAuthorizer>) -> Self {
        Self {
            cwd: cwd.into(),
            authorizer,
            command: METADATA_COMMAND
                .iter()
                .map(|part| part.to_string())
                .collect(),
            timeout_ms: Some(5 * 60 * 1000),
            saved: Mutex::new(None),
            configurations: Mutex::new(Vec::new()),
        }
    }

    /// Replace the command (tests use a scripted producer).
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    pub fn set_saved_metadata(&self, json: impl Into<String>) {
        *self.saved.lock().expect("metadata lock") = Some(json.into());
    }

    pub fn set_configurations(&self, configurations: Vec<Configuration>) {
        *self.configurations.lock().expect("configurations lock") = configurations;
    }

    /// Run the metadata command once and save its stdout.
    ///
    /// This is the explicit refresh entry (design §9: metadata refresh goes
    /// through the effect gate). It is never called from planning.
    pub fn refresh(&self) -> Result<(), RustToolError> {
        let program =
            self.command.first().cloned().ok_or_else(|| {
                RustToolError::metadata_unavailable("no metadata command configured")
            })?;
        let args: Vec<String> = self.command.iter().skip(1).cloned().collect();
        authorize(
            self.authorizer.as_ref(),
            EffectIntent::Exec {
                command: program.clone(),
                args: args.clone(),
            },
        )?;
        let result = exec_command(
            &program,
            &args,
            &self.cwd,
            &ExecOptions {
                signal: None,
                timeout_ms: self.timeout_ms,
                cwd: None,
            },
        );
        if result.code != 0 {
            return Err(RustToolError::metadata_unavailable(format!(
                "cargo metadata failed with code {}: {}",
                result.code,
                result.stderr.trim()
            )));
        }
        *self.saved.lock().expect("metadata lock") = Some(result.stdout);
        Ok(())
    }
}

impl WorkspaceCatalogPort for NativeMetadataHost {
    fn catalog(&self) -> Result<Arc<WorkspaceCatalog>, RustToolError> {
        let saved = self.saved.lock().expect("metadata lock").clone();
        let json = saved.ok_or_else(|| {
            RustToolError::metadata_unavailable(
                "no saved workspace metadata; refresh it explicitly first",
            )
        })?;
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

// --- broker ----------------------------------------------------------------

/// Bounded, streamed output of one run. The reader threads append while the
/// process runs, so `rs_job output` sees progress before exit.
#[derive(Debug, Default)]
struct StreamBuffers {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl StreamBuffers {
    fn total(&self) -> usize {
        self.stdout.len() + self.stderr.len()
    }
}

#[derive(Default)]
struct BrokerState {
    runs: HashMap<RunId, RunRecord>,
    outputs: HashMap<RunId, Arc<Mutex<StreamBuffers>>>,
    signals: HashMap<RunId, AbortSignal>,
    requests: HashMap<(String, String), (u64, RunId)>,
    order: VecDeque<RunId>,
    counter: u64,
}

/// A native `CargoJobBroker` that streams the child's output as it arrives.
pub struct NativeCargoBroker {
    cwd: String,
    authorizer: Option<EffectAuthorizer>,
    program: String,
    allowed_subcommands: Vec<String>,
    timeout_ms: Option<u64>,
    max_output_bytes: usize,
    capacity: usize,
    state: Arc<Mutex<BrokerState>>,
}

impl NativeCargoBroker {
    pub fn new(cwd: impl Into<String>, authorizer: Option<EffectAuthorizer>) -> Self {
        Self {
            cwd: cwd.into(),
            authorizer,
            program: "cargo".to_string(),
            allowed_subcommands: BROKER_SUBCOMMANDS
                .iter()
                .map(|part| part.to_string())
                .collect(),
            // A Cargo build can be long; the host still owns an upper bound.
            timeout_ms: Some(60 * 60 * 1000),
            max_output_bytes: 4 * 1024 * 1024,
            capacity: 16,
            state: Arc::new(Mutex::new(BrokerState::default())),
        }
    }

    /// Replace the program / subcommands (tests use a scripted producer).
    pub fn with_program(
        mut self,
        program: impl Into<String>,
        allowed_subcommands: Vec<String>,
    ) -> Self {
        self.program = program.into();
        self.allowed_subcommands = allowed_subcommands;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes.max(1);
        self
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    fn run<'a>(
        state: &'a BrokerState,
        owner: &OwnerId,
        run_id: &RunId,
    ) -> Result<&'a RunRecord, RustToolError> {
        match state.runs.get(run_id) {
            Some(record) if &record.owner == owner => Ok(record),
            // Missing and foreign look the same on purpose: a refusal must not
            // describe a target the caller does not own (design §3).
            _ => Err(RustToolError::permission_denied(
                "no such run for this session",
            )),
        }
    }
}

#[async_trait]
impl CargoJobBroker for NativeCargoBroker {
    async fn start(&self, request: StartRequest) -> Result<RunRecord, RustToolError> {
        let program = request
            .argv
            .first()
            .cloned()
            .ok_or_else(|| RustToolError::invalid_request("empty argv"))?;
        if program != self.program {
            return Err(RustToolError::permission_denied(format!(
                "the broker only runs `{}`",
                self.program
            )));
        }
        let subcommand = request
            .argv
            .get(1)
            .cloned()
            .ok_or_else(|| RustToolError::invalid_request("the command has no subcommand"))?;
        if !self
            .allowed_subcommands
            .iter()
            .any(|allowed| allowed == &subcommand)
        {
            return Err(RustToolError::permission_denied(format!(
                "subcommand `{subcommand}` is not approved"
            )));
        }
        let args: Vec<String> = request.argv.iter().skip(1).cloned().collect();
        authorize(
            self.authorizer.as_ref(),
            EffectIntent::Exec {
                command: program.clone(),
                args: args.clone(),
            },
        )?;

        let mut state = self.state.lock().expect("broker lock");
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

        state.counter += 1;
        let run_id = RunId::new(format!("run{}", state.counter));
        let signal = AbortSignal::new();
        let buffers = Arc::new(Mutex::new(StreamBuffers::default()));
        let record = RunRecord {
            run_id: run_id.clone(),
            owner: request.owner.clone(),
            plan_id: request.plan_id,
            step_id: request.step_id,
            request_id: request.request_id,
            configuration_id: request.configuration_id,
            metadata_digest: request.metadata_digest,
            argv: request.argv,
            state: RunState::Running,
            phase: RunPhase::Build,
            exit_status: None,
            build_status: pillar_agent::rust_tools::BuildStatus::Unknown,
            test_status: pillar_agent::rust_tools::TestStatus::Unknown,
            retained_bytes: 0,
        };
        state.signals.insert(run_id.clone(), signal.clone());
        state
            .requests
            .insert(key, (request.command_digest, run_id.clone()));
        state.runs.insert(run_id.clone(), record.clone());
        state.outputs.insert(run_id.clone(), Arc::clone(&buffers));
        state.order.push_back(run_id.clone());
        while state.order.len() > self.capacity {
            if let Some(evicted) = state.order.pop_front() {
                state.runs.remove(&evicted);
                state.outputs.remove(&evicted);
                state.signals.remove(&evicted);
            }
        }
        drop(state);

        let shared = Arc::clone(&self.state);
        let cwd = self.cwd.clone();
        let timeout_ms = self.timeout_ms;
        let max_output_bytes = self.max_output_bytes;
        std::thread::spawn(move || {
            let (code, killed) = run_streaming(
                &program,
                &args,
                &cwd,
                signal,
                timeout_ms,
                max_output_bytes,
                Arc::clone(&buffers),
            );
            let mut state = shared.lock().expect("broker lock");
            if let Some(record) = state.runs.get_mut(&run_id) {
                record.state = if killed {
                    RunState::Cancelled
                } else {
                    RunState::Exited
                };
                record.exit_status = Some(ExitStatus::Exited { code });
                record.retained_bytes = buffers.lock().expect("buffers lock").total();
            }
            state.signals.remove(&run_id);
        });

        Ok(record)
    }

    async fn status(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError> {
        let (mut record, buffers) = {
            let state = self.state.lock().expect("broker lock");
            let record = Self::run(&state, owner, run_id)?.clone();
            (record, state.outputs.get(run_id).cloned())
        };
        if let Some(buffers) = buffers {
            record.retained_bytes = buffers.lock().expect("buffers lock").total();
        }
        Ok(record)
    }

    async fn output(
        &self,
        owner: &OwnerId,
        run_id: &RunId,
        stream: OutputStream,
        offset: usize,
        limit_bytes: usize,
    ) -> Result<OutputPage, RustToolError> {
        let buffers = {
            let state = self.state.lock().expect("broker lock");
            Self::run(&state, owner, run_id)?;
            state.outputs.get(run_id).cloned()
        };
        let Some(buffers) = buffers else {
            return Ok(OutputPage {
                stream,
                offset,
                bytes: Vec::new(),
                total_bytes: 0,
                truncated: false,
                expired: true,
            });
        };
        // The readers append while the process runs, so a page is available
        // before exit (design §9: progress and output).
        let buffers = buffers.lock().expect("buffers lock");
        let source = match stream {
            OutputStream::Stdout => &buffers.stdout,
            OutputStream::Stderr => &buffers.stderr,
        };
        let total_bytes = source.len();
        let start = offset.min(total_bytes);
        let end = start.saturating_add(limit_bytes).min(total_bytes);
        Ok(OutputPage {
            stream,
            offset: start,
            bytes: source[start..end].to_vec(),
            total_bytes,
            truncated: end < total_bytes || buffers.truncated,
            expired: false,
        })
    }

    async fn cancel(&self, owner: &OwnerId, run_id: &RunId) -> Result<RunRecord, RustToolError> {
        let mut state = self.state.lock().expect("broker lock");
        let record = Self::run(&state, owner, run_id)?.clone();
        if let Some(signal) = state.signals.get(run_id) {
            signal.abort();
            if let Some(record) = state.runs.get_mut(run_id) {
                // Acceptance is not proof the process stopped (design §9).
                record.state = RunState::Cancelling;
            }
        }
        Ok(state.runs.get(run_id).cloned().unwrap_or(record))
    }

    async fn raw(&self, owner: &OwnerId, run_id: &RunId) -> Result<RawRunOutput, RustToolError> {
        let buffers = {
            let state = self.state.lock().expect("broker lock");
            Self::run(&state, owner, run_id)?;
            state.outputs.get(run_id).cloned()
        };
        match buffers {
            Some(buffers) => {
                let buffers = buffers.lock().expect("buffers lock");
                Ok(RawRunOutput {
                    stdout: buffers.stdout.clone(),
                    stderr: buffers.stderr.clone(),
                    truncated: buffers.truncated,
                })
            }
            None => Ok(RawRunOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: true,
            }),
        }
    }
}

/// Spawn the child as its own process-group leader, stream both pipes into
/// `buffers` as data arrives, and wait for it.
///
/// Cancellation and timeout signal the process group through the shared
/// `core::exec` helpers, so a grandchild stops too (docs/TASKS.md sb265). The
/// reader threads are not joined: a descendant holding a pipe open must not
/// hang the broker, which is why the wait uses a drain grace like
/// `exec_command`.
fn run_streaming(
    program: &str,
    args: &[String],
    cwd: &str,
    signal: AbortSignal,
    timeout_ms: Option<u64>,
    max_output_bytes: usize,
    buffers: Arc<Mutex<StreamBuffers>>,
) -> (i32, bool) {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return (-1, false),
    };
    if let Some(pipe) = child.stdout.take() {
        spawn_stream_reader(
            pipe,
            Arc::clone(&buffers),
            OutputStream::Stdout,
            max_output_bytes,
        );
    }
    if let Some(pipe) = child.stderr.take() {
        spawn_stream_reader(
            pipe,
            Arc::clone(&buffers),
            OutputStream::Stderr,
            max_output_bytes,
        );
    }

    let deadline = timeout_ms
        .filter(|timeout| *timeout > 0)
        .map(|timeout| Instant::now() + Duration::from_millis(timeout));
    let mut killed = false;
    let mut term_sent: Option<Instant> = None;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(0),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                break -1;
            }
        }
        let aborted = signal.is_aborted();
        let timed_out = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if (aborted || timed_out) && term_sent.is_none() {
            killed = true;
            terminate(&mut child);
            term_sent = Some(Instant::now());
        }
        if term_sent.is_some_and(|sent| sent.elapsed() >= TERM_GRACE) {
            hard_kill(&mut child);
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    std::thread::sleep(DRAIN_GRACE);
    (code, killed)
}

/// Read one pipe into the shared buffers as data arrives. Past the cap the data
/// is dropped but the pipe is still drained, so the writer never blocks.
fn spawn_stream_reader(
    mut pipe: impl Read + Send + 'static,
    buffers: Arc<Mutex<StreamBuffers>>,
    stream: OutputStream,
    cap: usize,
) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let mut buffers = buffers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let target = match stream {
                        OutputStream::Stdout => &mut buffers.stdout,
                        OutputStream::Stderr => &mut buffers.stderr,
                    };
                    let room = cap.saturating_sub(target.len());
                    if room == 0 {
                        buffers.truncated = true;
                        continue;
                    }
                    let keep = room.min(count);
                    target.extend_from_slice(&chunk[..keep]);
                    if keep < count {
                        buffers.truncated = true;
                    }
                }
            }
        }
    });
}

// --- sources ---------------------------------------------------------------

/// Reads workspace source ranges behind the `FsRead` intent.
pub struct NativeSources {
    cwd: String,
    authorizer: Option<EffectAuthorizer>,
    max_bytes: usize,
}

impl NativeSources {
    pub fn new(cwd: impl Into<String>, authorizer: Option<EffectAuthorizer>) -> Self {
        Self {
            cwd: cwd.into(),
            authorizer,
            max_bytes: 1024 * 1024,
        }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, RustToolError> {
        let candidate = Path::new(path);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            Path::new(&self.cwd).join(candidate)
        };
        let root = Path::new(&self.cwd);
        if !resolved.starts_with(root) {
            return Err(RustToolError::permission_denied(
                "the location is outside the workspace",
            ));
        }
        Ok(resolved)
    }
}

impl SourceSnapshotPort for NativeSources {
    fn read_range(
        &self,
        path: &str,
        start_line: u64,
        line_count: u64,
    ) -> Result<SourceSlice, RustToolError> {
        let text = self.read(path)?;
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

/// The analyzer needs the whole document for `didOpen`.
impl DocumentReader for NativeSources {
    fn read(&self, path: &str) -> Result<String, RustToolError> {
        let resolved = self.resolve(path)?;
        let resolved_str = resolved.to_string_lossy().to_string();
        authorize(
            self.authorizer.as_ref(),
            EffectIntent::FsRead { path: resolved_str },
        )?;
        let text = std::fs::read_to_string(&resolved)
            .map_err(|_| RustToolError::source_unbound("the file could not be read"))?;
        if text.len() > self.max_bytes {
            return Err(RustToolError::budget_exceeded(
                "the file exceeds the source read cap",
                "open the document in the editor instead",
            ));
        }
        Ok(text)
    }
}

// --- assembly --------------------------------------------------------------

/// The three native adapters as one host, so the CLI composes a toolkit in one
/// place.
pub struct NativeRustHost {
    metadata: Arc<NativeMetadataHost>,
    broker: Arc<NativeCargoBroker>,
    sources: Arc<NativeSources>,
}

impl NativeRustHost {
    pub fn new(cwd: impl Into<String>, authorizer: Option<EffectAuthorizer>) -> Self {
        let cwd = cwd.into();
        Self {
            metadata: Arc::new(NativeMetadataHost::new(cwd.clone(), authorizer.clone())),
            broker: Arc::new(NativeCargoBroker::new(cwd.clone(), authorizer.clone())),
            sources: Arc::new(NativeSources::new(cwd, authorizer)),
        }
    }

    pub fn metadata(&self) -> &Arc<NativeMetadataHost> {
        &self.metadata
    }

    pub fn broker(&self) -> &Arc<NativeCargoBroker> {
        &self.broker
    }

    /// The source host, for a `ContractService` fallback and for the analyzer's
    /// document reader.
    pub fn sources(&self) -> Arc<dyn SourceSnapshotPort> {
        Arc::clone(&self.sources) as Arc<dyn SourceSnapshotPort>
    }

    /// The analyzer's document reader (the same file host).
    pub fn documents(&self) -> Arc<dyn DocumentReader> {
        Arc::clone(&self.sources) as Arc<dyn DocumentReader>
    }

    /// Explicit effect-gated metadata refresh (design §9).
    pub fn refresh_metadata(&self) -> Result<(), RustToolError> {
        self.metadata.refresh()
    }

    /// Build the tool set for one session owner.
    pub fn toolkit(&self, owner: OwnerId, limits: RustToolLimits) -> RustToolkit {
        RustToolkit::new(
            Arc::clone(&self.metadata) as Arc<dyn WorkspaceCatalogPort>,
            Arc::clone(&self.broker) as Arc<dyn CargoJobBroker>,
            Arc::clone(&self.sources) as Arc<dyn SourceSnapshotPort>,
            owner,
            limits,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effects::{EffectDecision, EffectIntent};

    fn allow_all() -> EffectAuthorizer {
        Arc::new(|_: &EffectIntent| EffectDecision::Allow)
    }

    #[test]
    fn the_metadata_host_reads_saved_json_and_refreshes_explicitly() {
        let host = NativeMetadataHost::new("/ws", Some(allow_all()));
        assert_eq!(
            host.catalog().expect_err("no metadata").code,
            pillar_agent::rust_tools::RustToolErrorCode::MetadataUnavailable
        );

        host.set_saved_metadata(r#"{"packages":[],"workspace_members":[],"workspace_root":"/ws"}"#);
        let catalog = host.catalog().expect("saved metadata");
        assert_eq!(catalog.workspace_root(), Some("/ws"));
    }

    #[test]
    fn the_metadata_refresh_goes_through_the_authorizer() {
        let deny: EffectAuthorizer = Arc::new(|_: &EffectIntent| EffectDecision::Deny {
            reason: "no cargo here".to_string(),
        });
        let host = NativeMetadataHost::new("/ws", Some(deny));
        let error = host.refresh().expect_err("denied");
        assert_eq!(
            error.code,
            pillar_agent::rust_tools::RustToolErrorCode::PermissionDenied
        );
    }

    #[test]
    fn the_sources_refuse_a_path_outside_the_workspace() {
        let sources = NativeSources::new("/ws", Some(allow_all()));
        let error = sources
            .read_range("/etc/hostname", 1, 1)
            .expect_err("outside");
        assert_eq!(
            error.code,
            pillar_agent::rust_tools::RustToolErrorCode::PermissionDenied
        );
    }
}
