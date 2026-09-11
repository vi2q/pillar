//! Port of packages/coding-agent/src/core/bash-executor.ts and
//! core/exec.ts (pi v0.84.3): unified bash execution with streaming,
//! sanitization, tail truncation, and temp-file spill, plus the shared
//! command execution utility for extensions and custom tools.
//!
//! divergence: upstream's `BashOperations` is an async callback interface
//! over spawned processes; the port takes a synchronous `ExecFn` that feeds
//! output chunks (the local-spawn implementation lands with the tools
//! port). `exec_command` runs a process to completion with timeout/abort.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pillar_agent::abort::AbortSignal;

use crate::core::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, TruncationResult, sanitize_binary_output, strip_ansi,
    truncate_tail,
};

// ============================================================================
// exec.ts — execCommand
// ============================================================================

/// Result of executing a command to completion (upstream `ExecResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
}

/// Options for `exec_command` (upstream `ExecOptions`).
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Timeout in milliseconds.
    pub timeout: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// Execute a command (no shell) and collect stdout/stderr/code with
/// optional timeout and abort support (upstream `execCommand`).
///
/// divergence: upstream kills with SIGTERM then SIGKILL after 5s; the port
/// kills the child directly when the timeout or abort fires.
pub fn exec_command(command: &str, args: &[String], cwd: &str, options: ExecOptions) -> ExecResult {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let Ok(mut child) = cmd.spawn() else {
        return ExecResult {
            code: 1,
            ..Default::default()
        };
    };

    let deadline = options
        .timeout
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut killed = false;
    let mut aborted = false;

    loop {
        if let Some(signal) = &options.signal {
            if signal.is_aborted() && !killed {
                let _ = child.kill();
                killed = true;
                aborted = true;
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline && !killed {
                let _ = child.kill();
                killed = true;
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout_bytes = Vec::new();
                let mut stderr_bytes = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_end(&mut stdout_bytes);
                }
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_end(&mut stderr_bytes);
                }
                return ExecResult {
                    stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
                    stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
                    code: status.code().unwrap_or(if aborted { 1 } else { 0 }),
                    killed,
                };
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                return ExecResult {
                    code: 1,
                    killed,
                    ..Default::default()
                };
            }
        }
    }
}

// ============================================================================
// bash-executor.ts — executeBashWithOperations
// ============================================================================

/// The result of a bash execution (upstream `BashResult`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BashResult {
    /// Combined stdout + stderr output (sanitized, possibly truncated).
    pub output: String,
    /// Process exit code (None when killed/cancelled).
    pub exit_code: Option<i32>,
    /// Whether the command was cancelled via signal.
    pub cancelled: bool,
    /// Whether the output was truncated.
    pub truncated: bool,
    /// Temp file containing the full output when truncation hit.
    pub full_output_path: Option<PathBuf>,
    /// Tail-truncation details, present when output was truncated
    /// (upstream `details.truncation`).
    pub truncation: Option<TruncationResult>,
}

/// Output sink for a bash execution (upstream `BashOperations.exec` options
/// subset): feed sanitized chunks here.
pub trait BashOutputSink {
    fn on_chunk(&mut self, chunk: &str);
}

/// No-op sink.
#[derive(Default)]
pub struct NullSink;
impl BashOutputSink for NullSink {
    fn on_chunk(&mut self, _chunk: &str) {}
}

/// Closure sink (upstream `onChunk` callback).
pub struct CallbackSink<F: FnMut(&str)> {
    pub callback: F,
}

impl<F: FnMut(&str)> BashOutputSink for CallbackSink<F> {
    fn on_chunk(&mut self, chunk: &str) {
        (self.callback)(chunk);
    }
}

/// The execution operation fed by the executor (upstream `BashOperations.exec`):
/// receives raw byte chunks; returns the exit code.
pub type BashExecFn<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&[u8])) -> Result<Option<i32>, String>;

/// Accumulator implementing the upstream rolling-buffer + temp-file spill +
/// tail-truncation contract.
struct OutputAccumulator<'a> {
    chunks: Vec<String>,
    output_bytes: usize,
    max_output_bytes: usize,
    total_bytes: usize,
    temp_file_path: Option<PathBuf>,
    temp_file: Option<fs::File>,
    sink: &'a mut dyn BashOutputSink,
}

impl<'a> OutputAccumulator<'a> {
    fn new(sink: &'a mut dyn BashOutputSink) -> Self {
        Self {
            chunks: Vec::new(),
            output_bytes: 0,
            max_output_bytes: DEFAULT_MAX_BYTES * 2,
            total_bytes: 0,
            temp_file_path: None,
            temp_file: None,
            sink,
        }
    }

    fn ensure_temp_file(&mut self) {
        if self.temp_file_path.is_some() {
            return;
        }
        let id = pillar_ai::uuid::uuidv7().replace('-', "");
        let path = std::env::temp_dir().join(format!("pi-bash-{id}.log"));
        if let Ok(file) = fs::File::create(&path) {
            self.temp_file_path = Some(path);
            self.temp_file = Some(file);
            // Write everything buffered so far.
            if let Some(file) = self.temp_file.as_mut() {
                for chunk in &self.chunks {
                    let _ = file.write_all(chunk.as_bytes());
                }
            }
        }
    }

    /// Sanitize a raw byte chunk: strip ANSI, remove binary garbage,
    /// normalize newlines (upstream `onData`).
    fn on_data(&mut self, data: &[u8]) {
        self.total_bytes += data.len();
        let text =
            sanitize_binary_output(&strip_ansi(&String::from_utf8_lossy(data))).replace('\r', "");

        if self.total_bytes > DEFAULT_MAX_BYTES {
            self.ensure_temp_file();
        }
        if let Some(file) = self.temp_file.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }

        self.chunks.push(text.clone());
        self.output_bytes += text.len();
        while self.output_bytes > self.max_output_bytes && self.chunks.len() > 1 {
            let removed = self.chunks.remove(0);
            self.output_bytes -= removed.len();
        }

        self.sink.on_chunk(&text);
    }

    fn full_output(&self) -> String {
        self.chunks.join("")
    }
}

/// Execute a bash command through the caller-supplied operation (upstream
/// `executeBashWithOperations`): output is streamed sanitized chunks,
/// spilled to a temp file past the truncation threshold, and tail-truncated
/// at the end. Cancellation via the abort signal yields `cancelled: true`
/// with no exit code.
pub fn execute_bash_with_operations(
    command: &str,
    cwd: &str,
    exec_fn: BashExecFn<'_>,
    sink: &mut dyn BashOutputSink,
    signal: Option<&AbortSignal>,
) -> Result<BashResult, String> {
    let mut accumulator = OutputAccumulator::new(sink);

    let _ = (command, cwd);
    let result = exec_fn(&mut |data: &[u8]| accumulator.on_data(data));

    let finish = |accumulator: &mut OutputAccumulator<'_>, exit_code: Option<i32>| {
        let full_output = accumulator.full_output();
        let truncation = truncate_tail(&full_output, TruncationOptions::default());
        let truncation_details = truncation.truncated.then(|| truncation.clone());
        if truncation.truncated {
            accumulator.ensure_temp_file();
        }
        if let Some(file) = accumulator.temp_file.as_mut() {
            let _ = file.flush();
        }
        BashResult {
            output: if truncation.truncated {
                truncation.content
            } else {
                full_output
            },
            exit_code,
            cancelled: false,
            truncated: truncation.truncated,
            full_output_path: accumulator.temp_file_path.clone(),
            truncation: truncation_details,
        }
    };

    match result {
        Ok(exit_code) => {
            let cancelled = signal.is_some_and(|s| s.is_aborted());
            if cancelled {
                let mut result = finish(&mut accumulator, None);
                result.cancelled = true;
                return Ok(result);
            }
            Ok(finish(&mut accumulator, exit_code))
        }
        Err(error) => {
            if signal.is_some_and(|s| s.is_aborted()) {
                let mut result = finish(&mut accumulator, None);
                result.cancelled = true;
                return Ok(result);
            }
            if let Some(file) = accumulator.temp_file.as_mut() {
                let _ = file.flush();
            }
            let _ = cwd;
            Err(error)
        }
    }
}

/// Convenience: execute a local command through `std::process` with the
/// executor's sanitization/truncation contract (the local-spawn
/// `BashOperations` equivalent).
pub fn execute_bash_local(
    command: &str,
    cwd: &str,
    sink: &mut dyn BashOutputSink,
    signal: Option<&AbortSignal>,
) -> Result<BashResult, String> {
    if !Path::new(cwd).exists() {
        return Err(format!(
            "Working directory does not exist: {cwd}\nCannot execute bash commands."
        ));
    }
    let result = execute_bash_with_operations(
        command,
        cwd,
        &mut |on_data: &mut dyn FnMut(&[u8])| {
            let output = Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .output();
            match output {
                Ok(output) => {
                    // Stream stdout then stderr as chunks (upstream pipes
                    // interleaved; the port has the full buffers).
                    on_data(&output.stdout);
                    on_data(&output.stderr);
                    Ok(output.status.code())
                }
                Err(e) => Err(e.to_string()),
            }
        },
        sink,
        signal,
    )?;
    Ok(result)
}
