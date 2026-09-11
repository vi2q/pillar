//! Parity tests for bash-executor.ts + exec.ts (pi v0.84.3): sanitized
//! streaming, rolling-buffer + temp-file spill, tail truncation, and
//! cancellation semantics, plus the exec_command utility.

use std::sync::{Arc, Mutex};

use pillar_agent::abort::AbortSignal;
use pillar_coding_agent::core::bash_executor::{
    BashResult, CallbackSink, ExecOptions, exec_command, execute_bash_local,
    execute_bash_with_operations,
};
use pillar_coding_agent::core::truncate::DEFAULT_MAX_BYTES;

fn collect_sink(buffer: Arc<Mutex<Vec<String>>>) -> impl FnMut(&str) {
    move |chunk: &str| {
        buffer.lock().unwrap().push(chunk.to_string());
    }
}

#[allow(clippy::type_complexity)]
fn exec_ok(
    output: Vec<String>,
    exit_code: i32,
) -> impl FnMut(&mut dyn FnMut(&[u8])) -> Result<Option<i32>, String> {
    move |on_data: &mut dyn FnMut(&[u8])| {
        for chunk in &output {
            on_data(chunk.as_bytes());
        }
        Ok(Some(exit_code))
    }
}

#[test]
fn constants_and_helpers_exist() {
    assert_eq!(DEFAULT_MAX_BYTES, 50 * 1024);
}

// --- execCommand -----------------------------------------------------------------

#[test]
fn exec_command_captures_stdout_stderr_and_code() {
    let result = exec_command(
        "sh",
        &["-c".to_string(), "echo out; echo err 1>&2".to_string()],
        ".",
        ExecOptions::default(),
    );
    assert_eq!(result.code, 0);
    assert!(!result.killed);
    assert!(result.stdout.contains("out"), "{}", result.stdout);
    assert!(result.stderr.contains("err"), "{}", result.stderr);
}

#[test]
fn exec_command_missing_binary_reports_error() {
    let result = exec_command(
        "definitely-not-a-binary-xyz",
        &[],
        ".",
        ExecOptions::default(),
    );
    assert_eq!(result.code, 1);
}

#[test]
fn exec_command_timeout_kills_the_process() {
    let result = exec_command(
        "sh",
        &["-c".to_string(), "sleep 5".to_string()],
        ".",
        ExecOptions {
            timeout: Some(100),
            signal: None,
        },
    );
    assert!(result.killed, "{result:?}");
}

// --- bash executor -------------------------------------------------------------------

#[test]
fn bash_executor_streams_sanitized_chunks() {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink_buffer = buffer.clone();
    let mut sink = CallbackSink {
        callback: collect_sink(sink_buffer),
    };
    let result = execute_bash_with_operations(
        "cmd",
        "/tmp",
        &mut exec_ok(vec!["\x1b[31mred\x1b[0m plain\r\nline2".to_string()], 0),
        &mut sink,
        None,
    )
    .unwrap();
    // ANSI stripped and \r normalized.
    assert_eq!(result.output, "red plain\nline2");
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.cancelled);
    assert!(!result.truncated);
    assert!(result.full_output_path.is_none());
    let chunks = buffer.lock().unwrap();
    assert_eq!(chunks.len(), 1, "one chunk fed");
    assert_eq!(chunks[0], "red plain\nline2");
}

#[test]
fn bash_executor_truncates_long_output_and_spills_to_temp_file() {
    // A single long line exceeding the truncation threshold.
    let long: String = "x".repeat(DEFAULT_MAX_BYTES + 100);
    let mut sink = CallbackSink {
        callback: |_chunk: &str| {},
    };
    let result = execute_bash_with_operations(
        "cmd",
        "/tmp",
        &mut exec_ok(vec![long.clone()], 0),
        &mut sink,
        None,
    )
    .unwrap();
    assert!(result.truncated);
    assert!(result.full_output_path.is_some(), "temp file spilled");
    let path = result.full_output_path.unwrap();
    let full = std::fs::read_to_string(&path).unwrap();
    assert!(full.contains(&long), "full output persisted");
    std::fs::remove_file(&path).ok();
    // Truncated content keeps the tail.
    assert!(result.output.len() <= DEFAULT_MAX_BYTES);
    assert!(result.output.ends_with(&long[long.len() - 10..]));
}

#[test]
fn bash_executor_reports_cancellation_with_no_exit_code() {
    let signal = AbortSignal::new();
    signal.abort();
    let mut sink = CallbackSink {
        callback: |_chunk: &str| {},
    };
    let result = execute_bash_with_operations(
        "cmd",
        "/tmp",
        &mut exec_ok(vec!["partial output".to_string()], 0),
        &mut sink,
        Some(&signal),
    )
    .unwrap();
    assert!(result.cancelled);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.output, "partial output", "streamed output kept");
}

#[test]
fn bash_executor_propagates_operation_errors_unless_aborted() {
    let mut sink = CallbackSink {
        callback: |_chunk: &str| {},
    };
    let error = execute_bash_with_operations(
        "cmd",
        "/tmp",
        &mut |_on_data| Err("boom".to_string()),
        &mut sink,
        None,
    )
    .unwrap_err();
    assert_eq!(error, "boom");

    // Aborted signal converts the error into a cancelled result.
    let signal = AbortSignal::new();
    signal.abort();
    let aborted = execute_bash_with_operations(
        "cmd",
        "/tmp",
        &mut |_on_data| Err("boom".to_string()),
        &mut sink,
        Some(&signal),
    )
    .unwrap();
    assert!(aborted.cancelled);
}

#[test]
fn bash_executor_local_exec_runs_commands() {
    let dir = std::env::temp_dir().join(format!("pillar-bash-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut sink = CallbackSink {
        callback: |_chunk: &str| {},
    };
    let result = execute_bash_local("echo hello", &dir.to_string_lossy(), &mut sink, None).unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.output.contains("hello"), "{}", result.output);

    // Missing cwd errors like upstream.
    let error = execute_bash_local(
        "echo hi",
        "/definitely/not/a/real/dir",
        &mut CallbackSink {
            callback: |_: &str| {},
        },
        None,
    )
    .unwrap_err();
    assert!(
        error.contains("Working directory does not exist"),
        "{error}"
    );
}

#[test]
fn bash_result_defaults_match_upstream_shape() {
    let result = BashResult::default();
    assert_eq!(result.exit_code, None);
    assert!(!result.cancelled);
    assert!(!result.truncated);
    assert_eq!(result.output, "");
    assert!(result.full_output_path.is_none());
}
