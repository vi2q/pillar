//! Port of packages/coding-agent/src/core/exec.ts (pi v0.84.3): the shared
//! command execution used by extensions and custom tools, with upstream's
//! abort / timeout semantics.
//!
//! divergences:
//! - Node's event loop becomes a blocking spawn with dedicated pipe readers
//!   and a poll loop; the observable result (`stdout` / `stderr` / `code` /
//!   `killed`) matches, and like upstream the wait ends when the *child* exits,
//!   so a detached descendant holding the pipes open cannot hang the call.
//! - the SIGTERM → SIGKILL escalation matches upstream on unix; elsewhere the
//!   child is killed outright (no SIGTERM).

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_agent::abort::AbortSignal;

/// How often the wait loop checks the child and the abort / timeout state.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a SIGTERM gets before the child is killed outright.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// How long the pipe readers get after the child exited (upstream keeps
/// whatever arrived by then).
const DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Options for one command execution (upstream `ExecOptions`).
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Cancels the command; an aborted signal kills it before it starts.
    pub signal: Option<AbortSignal>,
    /// Timeout in milliseconds; `None` or `0` means no timeout.
    pub timeout_ms: Option<u64>,
    /// Working directory override (upstream `options.cwd`, default: the
    /// caller's cwd).
    pub cwd: Option<String>,
}

/// Result of one command execution (upstream `ExecResult`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
}

impl ExecResult {
    /// The failure shape upstream produces when the command cannot be spawned.
    pub fn spawn_failure(error: impl std::fmt::Display) -> Self {
        Self {
            stdout: String::new(),
            stderr: error.to_string(),
            code: -1,
            killed: false,
        }
    }
}

/// Execute a command and return its output, honouring `signal` / `timeout`
/// (upstream `execCommand`).
pub fn exec_command(command: &str, args: &[String], cwd: &str, options: &ExecOptions) -> ExecResult {
    let mut process = Command::new(command);
    process
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return ExecResult::spawn_failure(error),
    };

    // The readers append as data arrives (upstream's `data` handlers): the
    // wait ends on the child's exit, so buffering until EOF would lose
    // everything an orphaned descendant keeps the pipe for.
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(pipe) = child.stdout.take() {
        spawn_reader(pipe, Arc::clone(&stdout));
    }
    if let Some(pipe) = child.stderr.take() {
        spawn_reader(pipe, Arc::clone(&stderr));
    }

    let deadline = options
        .timeout_ms
        .filter(|timeout| *timeout > 0)
        .map(|timeout| Instant::now() + Duration::from_millis(timeout));
    let mut killed = false;
    let mut term_sent: Option<Instant> = None;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Upstream `code ?? 0`: a signal-killed child has no exit code.
                break status.code().unwrap_or(0);
            }
            Ok(None) => {}
            Err(error) => {
                let mut result = collect_output(&stdout, &stderr, 0);
                result.stderr = error.to_string();
                result.code = -1;
                return result;
            }
        }
        let aborted = options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        let timed_out = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if (aborted || timed_out) && term_sent.is_none() {
            killed = true;
            terminate(&mut child);
            term_sent = Some(Instant::now());
        }
        if term_sent.is_some_and(|sent| sent.elapsed() >= TERM_GRACE) {
            let _ = child.kill();
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let mut result = collect_output(&stdout, &stderr, code);
    result.killed = killed;
    result
}

/// The output that arrived by the time the child exited (upstream keeps the
/// accumulated buffer; [`DRAIN_GRACE`] lets the last in-flight read land).
fn collect_output(stdout: &Arc<Mutex<Vec<u8>>>, stderr: &Arc<Mutex<Vec<u8>>>, code: i32) -> ExecResult {
    std::thread::sleep(DRAIN_GRACE);
    let text = |buffer: &Arc<Mutex<Vec<u8>>>| {
        let bytes = buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    };
    ExecResult {
        stdout: text(stdout),
        stderr: text(stderr),
        code,
        killed: false,
    }
}

/// Read one pipe into the shared buffer as data arrives. The thread ends at
/// EOF; a descendant holding the pipe open only delays that (the result does
/// not wait for it).
fn spawn_reader(mut pipe: impl Read + Send + 'static, buffer: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => buffer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend_from_slice(&chunk[..count]),
            }
        }
    });
}

/// Ask the child to stop: SIGTERM on unix (upstream), `Child::kill`
/// elsewhere.
fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: `id()` is a live child pid owned by this process.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn runs_a_command_and_captures_both_streams() {
        let result = exec_command(
            "sh",
            &args(&["-c", "printf out; printf err 1>&2; exit 3"]),
            "/tmp",
            &ExecOptions::default(),
        );
        assert_eq!(result.stdout, "out");
        assert_eq!(result.stderr, "err");
        assert_eq!(result.code, 3);
        assert!(!result.killed);
    }

    #[test]
    fn a_missing_command_reports_the_spawn_failure() {
        let result = exec_command(
            "definitely-not-a-command",
            &[],
            "/tmp",
            &ExecOptions::default(),
        );
        assert_eq!(result.code, -1);
        assert!(!result.stderr.is_empty());
    }

    #[test]
    fn an_aborted_signal_kills_a_slow_command() {
        let signal = AbortSignal::new();
        let killer = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            killer.abort();
        });
        let started = Instant::now();
        let result = exec_command(
            "sh",
            &args(&["-c", "sleep 30"]),
            "/tmp",
            &ExecOptions {
                signal: Some(signal),
                ..Default::default()
            },
        );
        assert!(result.killed);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the abort returned promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_timeout_kills_the_command() {
        let started = Instant::now();
        let result = exec_command(
            "sh",
            &args(&["-c", "sleep 30"]),
            "/tmp",
            &ExecOptions {
                timeout_ms: Some(150),
                ..Default::default()
            },
        );
        assert!(result.killed);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_is_captured_when_a_descendant_keeps_the_pipe_open() {
        // The child exits but leaves a grandchild holding stdout: upstream
        // resolves on the child's exit, so the call must not hang.
        let started = Instant::now();
        let result = exec_command(
            "sh",
            &args(&["-c", "sleep 30 & printf early"]),
            "/tmp",
            &ExecOptions::default(),
        );
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "early");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no hang on inherited pipes: {:?}",
            started.elapsed()
        );
    }
}
