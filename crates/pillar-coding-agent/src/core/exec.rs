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
//! - a cancellation on unix signals the child's *process group* (the child is
//!   started as its own group leader), so a grandchild the command spawned does
//!   not survive the abort and keep the pipes open. Upstream kills only the
//!   child (docs/TASKS.md sb265).
//! - captured output is bounded by [`MAX_CAPTURED_BYTES`] per stream: past it
//!   the readers keep draining the pipe (a full pipe would block the child) but
//!   stop storing, and the result says `truncated`. Upstream accumulates
//!   without a bound; the port must not let a command size the host's heap.

use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the wait loop checks the child and the abort / timeout state.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a SIGTERM gets before the child is killed outright.
pub(crate) const TERM_GRACE: Duration = Duration::from_secs(5);
/// How long the pipe readers get after the child exited (upstream keeps
/// whatever arrived by then).
pub(crate) const DRAIN_GRACE: Duration = Duration::from_millis(100);
/// How much of each stream a run keeps. Beyond it the pipe is still drained
/// (the child must not block on a full pipe) but nothing more is stored, so a
/// command that prints without end costs the host a bounded amount of memory
/// and reports `truncated` (upstream accumulates without a bound).
pub const MAX_CAPTURED_BYTES: usize = 4 * 1024 * 1024;

// The `ExecOptions` / `ExecResult` shapes live in the contract crate (the VM's
// exec host callback names them); the spawn implementation stays here.
pub use pillar_extensions_contract::{ExecOptions, ExecResult};

/// Execute a command and return its output, honouring `signal` / `timeout`
/// (upstream `execCommand`).
pub fn exec_command(
    command: &str,
    args: &[String],
    cwd: &str,
    options: &ExecOptions,
) -> ExecResult {
    let mut process = Command::new(command);
    process
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Make the child its own process-group leader so a cancellation can signal
    // the whole group (the child and any grandchildren it spawned).
    #[cfg(unix)]
    process.process_group(0);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return ExecResult::spawn_failure(error),
    };

    // The readers append as data arrives (upstream's `data` handlers): the
    // wait ends on the child's exit, so buffering until EOF would lose
    // everything an orphaned descendant keeps the pipe for.
    let stdout = Arc::new(Mutex::new(Captured::default()));
    let stderr = Arc::new(Mutex::new(Captured::default()));
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
            hard_kill(&mut child);
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let mut result = collect_output(&stdout, &stderr, code);
    result.killed = killed;
    result
}

/// The output that arrived by the time the child exited (upstream keeps the
/// accumulated buffer; [`DRAIN_GRACE`] lets the last in-flight read land).
fn collect_output(
    stdout: &Arc<Mutex<Captured>>,
    stderr: &Arc<Mutex<Captured>>,
    code: i32,
) -> ExecResult {
    std::thread::sleep(DRAIN_GRACE);
    let read = |buffer: &Arc<Mutex<Captured>>| {
        let captured = buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            String::from_utf8_lossy(&captured.bytes).into_owned(),
            captured.truncated,
        )
    };
    let (stdout_text, stdout_truncated) = read(stdout);
    let (stderr_text, stderr_truncated) = read(stderr);
    ExecResult {
        stdout: stdout_text,
        stderr: stderr_text,
        code,
        killed: false,
        truncated: stdout_truncated || stderr_truncated,
    }
}

/// What one pipe reader keeps: the bytes so far and whether it had to drop any.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Read one pipe into the shared buffer as data arrives. The thread ends at
/// EOF; a descendant holding the pipe open only delays that (the result does
/// not wait for it). Past [`MAX_CAPTURED_BYTES`] the data is *dropped* rather
/// than stored — the read continues, so the writer never blocks on a full pipe.
fn spawn_reader(mut pipe: impl Read + Send + 'static, buffer: Arc<Mutex<Captured>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let mut captured = buffer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let room = MAX_CAPTURED_BYTES.saturating_sub(captured.bytes.len());
                    if room == 0 {
                        captured.truncated = true;
                        continue;
                    }
                    let keep = room.min(count);
                    captured.bytes.extend_from_slice(&chunk[..keep]);
                    if keep < count {
                        captured.truncated = true;
                    }
                }
            }
        }
    });
}

/// Ask the child's process group to stop: SIGTERM on unix (upstream asks the
/// child; the port asks its group so descendants stop too), `Child::kill`
/// elsewhere.
pub(crate) fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: `id()` is a live child pid, and the child is its own process
        // group leader (`process_group(0)`), so its pid is the group id.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// The escalation after [`TERM_GRACE`]: SIGKILL the group on unix, `kill`
/// elsewhere.
pub(crate) fn hard_kill(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: as in `terminate`.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
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
    use pillar_agent::abort::AbortSignal;

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
    fn an_endless_command_costs_a_bounded_amount_of_memory() {
        // ~12 MB of output against a 4 MB budget: the extra is drained (the
        // writer must not block on a full pipe) but not stored, and the result
        // says so. Before the budget the host accumulated every byte.
        let result = exec_command(
            "sh",
            &args(&[
                "-c",
                "i=0; while [ $i -lt 3000 ]; do printf '%5000d' 0; i=$((i+1)); done; printf done",
            ]),
            "/tmp",
            &ExecOptions::default(),
        );
        assert_eq!(
            result.code, 0,
            "the command ran to the end: {}",
            result.stderr
        );
        assert!(result.truncated, "the budget was reported");
        assert!(
            result.stdout.len() <= MAX_CAPTURED_BYTES,
            "the captured output stays within the budget: {} bytes",
            result.stdout.len()
        );
        assert!(
            result.stdout.len() > MAX_CAPTURED_BYTES / 2,
            "the budget is actually used: {} bytes",
            result.stdout.len()
        );
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

    #[cfg(unix)]
    #[test]
    fn an_abort_kills_the_child_process_group() {
        let signal = AbortSignal::new();
        let killer = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            killer.abort();
        });
        // The shell prints the pid of the grandchild it spawned, then waits;
        // the abort must take the grandchild with it, not leave it running.
        let result = exec_command(
            "sh",
            &args(&["-c", "sleep 30 & echo $!; wait"]),
            "/tmp",
            &ExecOptions {
                signal: Some(signal),
                ..Default::default()
            },
        );
        assert!(result.killed);
        let pid: i32 = result.stdout.trim().parse().expect("the grandchild pid");
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: `kill(pid, 0)` only probes for existence.
            if unsafe { libc::kill(pid, 0) } == -1 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone, "the grandchild {pid} must not survive the abort");
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
