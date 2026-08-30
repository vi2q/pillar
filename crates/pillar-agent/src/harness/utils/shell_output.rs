//! Port of packages/agent/src/harness/utils/shell-output.ts (pi v0.84.3).
//!
//! Captures shell output with tail truncation and a full-output spill
//! file.
//!
//! divergence: upstream streams chunks through exec callbacks and spills
//! to the full-output file mid-run once limits are crossed. The port's
//! `Shell::exec` collects complete output first; the capture then computes
//! the tail truncation and writes the spill file. Observable results
//! (tail content, truncation metadata, spill path, cancelled/exit
//! handling) match upstream for complete-output scenarios; only
//! mid-run `on_chunk` progress callbacks differ. Byte accounting uses the
//! same sanitize + count rules.

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    truncate_tail,
};
use crate::harness::types::{
    ExecutionError, ExecutionErrorCode, FileSystem, Shell, ShellExecOptions,
};

/// Progress snapshot (upstream `ShellCaptureProgress`).
#[derive(Debug, Clone, Default)]
pub struct ShellCaptureProgress {
    /// Tail output (possibly truncated content).
    pub output: String,
    /// Truncation details for the tail.
    pub truncation: Option<TruncationResult>,
    /// Path of the full-output spill file, when created.
    pub full_output_path: Option<String>,
    /// Byte length of the currently open last line.
    pub last_line_bytes: usize,
}

/// Result of a captured shell execution (upstream `ShellCaptureResult`).
#[derive(Debug, Default)]
pub struct ShellCaptureResult {
    /// Tail output (possibly truncated content).
    pub output: String,
    /// Truncation details for the tail.
    pub truncation: Option<TruncationResult>,
    /// Path of the full-output spill file, when created.
    pub full_output_path: Option<String>,
    /// Byte length of the currently open last line.
    pub last_line_bytes: usize,
    /// Exit code, or `None` when cancelled.
    pub exit_code: Option<i32>,
    /// Whether the command was aborted.
    pub cancelled: bool,
    /// Whether the tail was truncated.
    pub truncated: bool,
    /// Shell execution failure returned inline (upstream
    /// `returnExecutionErrors`).
    pub execution_error: Option<ExecutionError>,
}

/// Options for captured shell execution (upstream `ShellCaptureOptions`).
#[derive(Default)]
pub struct ShellCaptureOptions {
    /// Working directory override.
    pub cwd: Option<String>,
    /// Environment variable overrides.
    pub env: Vec<(String, String)>,
    /// Whether to inherit the default environment. Default true.
    pub inherit_env: Option<bool>,
    /// Timeout in seconds.
    pub timeout: Option<f64>,
    /// Abort signal.
    pub abort_signal: Option<crate::abort::AbortSignal>,
    /// Return shell execution failures inline instead of as a failed
    /// `Result`.
    pub return_execution_errors: bool,
}

impl std::fmt::Debug for ShellCaptureOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellCaptureOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("abort_signal", &self.abort_signal.is_some())
            .field("return_execution_errors", &self.return_execution_errors)
            .finish()
    }
}

/// Strip control characters from shell output and remove `\r` (upstream
/// `sanitizeBinaryOutput` + `.replace(/\r/g, "")`).
pub fn sanitize_binary_output(input: &str) -> String {
    input
        .chars()
        .filter(|c| {
            let code = *c as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            !(0xfff9..=0xfffb).contains(&code)
        })
        .collect()
}

/// Accounting state over the sanitized combined output (upstream `onChunk`
/// accumulator variables).
#[derive(Debug, Default)]
struct OutputAccounting {
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
}

fn account_chunk(accounting: &mut OutputAccounting, text: &str) {
    let text_bytes = text.len();
    accounting.total_bytes += text_bytes;
    let newline_count = text.matches('\n').count();
    accounting.completed_lines += newline_count;
    if let Some(last_newline) = text.rfind('\n') {
        let trailing = &text[last_newline + 1..];
        accounting.current_line_bytes = trailing.len();
        accounting.has_open_line = !trailing.is_empty();
    } else if !text.is_empty() {
        accounting.current_line_bytes += text_bytes;
        accounting.has_open_line = true;
    }
}

fn build_progress(
    accounting: &OutputAccounting,
    tail_output: &str,
    full_output_path: Option<String>,
) -> ShellCaptureProgress {
    let tail_truncation = truncate_tail(
        tail_output,
        TruncationOptions {
            max_lines: Some(DEFAULT_MAX_LINES),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        },
    );
    let total_lines = accounting.completed_lines + if accounting.has_open_line { 1 } else { 0 };
    let truncated = total_lines > DEFAULT_MAX_LINES || accounting.total_bytes > DEFAULT_MAX_BYTES;
    let mut truncation = tail_truncation;
    truncation.truncated = truncated;
    truncation.truncated_by = if truncated {
        Some(
            truncation
                .truncated_by
                .unwrap_or(if accounting.total_bytes > DEFAULT_MAX_BYTES {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }),
        )
    } else {
        None
    };
    truncation.total_lines = total_lines;
    truncation.total_bytes = accounting.total_bytes;
    ShellCaptureProgress {
        output: if truncated {
            truncation.content.clone()
        } else {
            tail_output.to_owned()
        },
        truncation: Some(truncation),
        full_output_path,
        last_line_bytes: accounting.current_line_bytes,
    }
}

/// Execute a shell command with tail-captured output and a spill file when
/// the output exceeds the truncation limits (upstream
/// `executeShellWithCapture`). Requires a composed
/// [`ExecutionEnv`](crate::harness::types::ExecutionEnv) (FileSystem +
/// Shell) because the spill file is written through the filesystem.
pub async fn execute_shell_with_capture(
    env: &(impl Shell + FileSystem),
    command: &str,
    options: Option<ShellCaptureOptions>,
) -> Result<ShellCaptureResult, ExecutionError> {
    let options = options.unwrap_or_default();

    let result = env
        .exec(
            command,
            Some(ShellExecOptions {
                cwd: options.cwd.clone(),
                env: options.env.clone(),
                inherit_env: options.inherit_env,
                timeout: options.timeout,
                abort_signal: options.abort_signal.clone(),
                ..Default::default()
            }),
        )
        .await;

    // Combine stdout+stderr (upstream feeds both streams through the same
    // onChunk accumulator).
    let (combined, exit_code, exec_error) = match result {
        Ok(output) => (
            format!("{}{}", output.stdout, output.stderr),
            Some(output.exit_code),
            None,
        ),
        Err(error) => (String::new(), None, Some(error)),
    };

    // Sanitize the whole stream and account it as one chunk (the port's
    // exec collects complete output; see module divergence note).
    let text = sanitize_binary_output(&combined).replace('\r', "");
    let mut accounting = OutputAccounting::default();
    account_chunk(&mut accounting, &text);
    let tail_output = text;

    let mut progress = build_progress(&accounting, &tail_output, None);
    let truncated = progress.truncation.as_ref().is_some_and(|t| t.truncated);

    // Spill the full output to a temp file when truncation applies
    // (upstream ensureFullOutputFile).
    let full_output_path = if truncated && !tail_output.is_empty() {
        let temp_file = env
            .create_temp_file("bash-", ".log")
            .await
            .map_err(|error| ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string()))?;
        env.append_file(&temp_file, tail_output.as_bytes())
            .await
            .map_err(|error| ExecutionError::new(ExecutionErrorCode::Unknown, error.to_string()))?;
        Some(temp_file)
    } else {
        None
    };
    progress.full_output_path = full_output_path;

    if let Some(error) = exec_error {
        let aborted = error.code == ExecutionErrorCode::Aborted
            || options
                .abort_signal
                .as_ref()
                .is_some_and(|s| s.is_aborted());
        let truncated_now = progress.truncation.as_ref().is_some_and(|t| t.truncated);
        if aborted {
            return Ok(ShellCaptureResult {
                output: progress.output,
                truncation: progress.truncation,
                full_output_path: progress.full_output_path,
                last_line_bytes: progress.last_line_bytes,
                exit_code: None,
                cancelled: true,
                truncated: truncated_now,
                execution_error: None,
            });
        }
        if options.return_execution_errors {
            return Ok(ShellCaptureResult {
                output: progress.output,
                truncation: progress.truncation,
                full_output_path: progress.full_output_path,
                last_line_bytes: progress.last_line_bytes,
                exit_code: None,
                cancelled: false,
                truncated: truncated_now,
                execution_error: Some(error),
            });
        }
        return Err(error);
    }

    let exit_code = exit_code.unwrap_or(0);
    let truncated_now = progress.truncation.as_ref().is_some_and(|t| t.truncated);
    Ok(ShellCaptureResult {
        output: progress.output,
        truncation: progress.truncation,
        full_output_path: progress.full_output_path,
        last_line_bytes: progress.last_line_bytes,
        exit_code: Some(exit_code),
        cancelled: false,
        truncated: truncated_now,
        execution_error: None,
    })
}
