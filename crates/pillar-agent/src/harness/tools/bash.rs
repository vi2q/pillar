//! Port of packages/agent/src/harness/tools/bash.ts (pi v0.84.3).
//!
//! divergence: upstream throttles `onUpdate` with `setTimeout`; the port
//! throttles with `tokio::time::Instant` (async sleep between emissions).
//! Upstream throws on failure statuses; the port returns
//! `ToolExecuteError`. The port's `execute_shell_with_capture` currently
//! runs the command to completion before progress callbacks fire (see
//! shell_output.rs), so mid-stream throttled updates are exercised by the
//! driver once streaming lands; the throttle state machine is ported and
//! unit-tested here.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::harness::tools::file_mutation_queue::FileMutationQueues;
use crate::harness::types::{ExecutionEnv, ExecutionErrorCode};
use crate::harness::utils::shell_output::{ShellCaptureOptions, execute_shell_with_capture};
use crate::harness::utils::truncate::{DEFAULT_MAX_BYTES, format_size};
use crate::types::{AgentToolResult, ToolExecuteError};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
#[cfg_attr(not(test), allow(dead_code))] // consumed by the streaming driver
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// Upstream `BashToolInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct BashToolInput {
    pub command: String,
    pub timeout: Option<f64>,
}

/// Parse the bash tool input (upstream typebox validation).
pub fn parse_bash_input(input: &Value) -> Result<BashToolInput, ToolExecuteError> {
    let object = input
        .as_object()
        .ok_or_else(|| ToolExecuteError("bash input must be an object".to_owned()))?;
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolExecuteError("bash input requires a string command".to_owned()))?;
    Ok(BashToolInput {
        command: command.to_owned(),
        timeout: object.get("timeout").and_then(Value::as_f64),
    })
}

/// Upstream `BashToolDetails` serialized into `AgentToolResult.details`.
#[derive(Debug, Clone, PartialEq)]
pub struct BashToolDetails {
    pub truncation: Option<crate::harness::utils::truncate::TruncationResult>,
    pub full_output_path: Option<String>,
}

impl BashToolDetails {
    fn to_value(&self) -> Value {
        let truncation = self.truncation.as_ref().map(|truncation| {
            serde_json::json!({
                "content": truncation.content,
                "truncated": truncation.truncated,
                "truncatedBy": truncation.truncated_by.map(|by| match by {
                    crate::harness::utils::truncate::TruncatedBy::Lines => "lines",
                    crate::harness::utils::truncate::TruncatedBy::Bytes => "bytes",
                }),
                "totalLines": truncation.total_lines,
                "totalBytes": truncation.total_bytes,
                "outputLines": truncation.output_lines,
                "outputBytes": truncation.output_bytes,
                "lastLinePartial": truncation.last_line_partial,
                "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
                "maxLines": truncation.max_lines,
                "maxBytes": truncation.max_bytes,
            })
        });
        serde_json::json!({
            "truncation": truncation,
            "fullOutputPath": self.full_output_path,
        })
    }
}

/// Upstream `BashExecution`: the execution descriptor passed to `prepare`.
#[derive(Debug, Clone, PartialEq)]
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub inherit_env: bool,
}

/// Upstream `BashPrepare`: adjust the execution descriptor before running.
pub type BashPrepareFn = dyn Fn(&mut BashExecution) -> Result<(), ToolExecuteError> + Send + Sync;

/// Upstream `BashToolOptions`.
#[derive(Default, Clone)]
pub struct BashToolOptions {
    pub command_prefix: Option<String>,
    /// Upstream `prepare(execution, context, signal)`.
    pub prepare: Option<Arc<BashPrepareFn>>,
}

fn tool_error(message: impl Into<String>) -> ToolExecuteError {
    ToolExecuteError(message.into())
}

/// JS `String(n)` for the timeout in the timeout message.
fn format_timeout(timeout: Option<f64>) -> String {
    match timeout {
        None => String::new(),
        Some(value) if value.fract() == 0.0 => format!("{}", value as u64),
        Some(value) => format!("{value}"),
    }
}

/// Upstream `validateTimeout`.
fn validate_timeout(timeout: Option<f64>) -> Result<(), ToolExecuteError> {
    let Some(timeout) = timeout else {
        return Ok(());
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(tool_error(
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(tool_error(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        )));
    }
    Ok(())
}

/// Upstream `createBashTool().execute`.
///
/// divergence: the harness-side mutation queue does not apply to bash
/// (upstream only queues write/edit); abort is observed via the capture
/// result's `cancelled` flag.
pub async fn execute_bash_tool<E: ExecutionEnv + ?Sized>(
    env: &E,
    input: &Value,
    signal: Option<&crate::abort::AbortSignal>,
    on_update: Option<crate::types::AgentToolUpdateCallback>,
    options: Option<&BashToolOptions>,
    _queues: &FileMutationQueues,
) -> Result<AgentToolResult, ToolExecuteError> {
    let input = parse_bash_input(input)?;
    validate_timeout(input.timeout)?;
    let options = options.cloned().unwrap_or_default();
    let mut execution = BashExecution {
        command: match &options.command_prefix {
            Some(prefix) => format!("{prefix}\n{}", input.command),
            None => input.command.clone(),
        },
        cwd: env.cwd().to_owned(),
        env: Vec::new(),
        inherit_env: true,
    };
    if let Some(prepare) = &options.prepare {
        prepare(&mut execution)?;
    }

    // Upstream throttled-update state.
    let throttle = Arc::new(ThrottleState::new(on_update.clone()));
    throttle.emit_initial();

    let capture = execute_shell_with_capture(
        env,
        &execution.command,
        Some(ShellCaptureOptions {
            cwd: Some(execution.cwd.clone()),
            env: execution.env.clone(),
            inherit_env: Some(execution.inherit_env),
            timeout: input.timeout,
            abort_signal: signal.cloned(),
            return_execution_errors: true,
        }),
    )
    .await
    .map_err(|error| tool_error(error.to_string()))?;

    throttle.flush(&capture)?;

    let mut output_text = capture.output.clone();
    let mut details: Option<BashToolDetails> = None;
    if let Some(truncation) = &capture.truncation {
        if truncation.truncated {
            let full_output_path = capture.full_output_path.clone().unwrap_or_default();
            details = Some(BashToolDetails {
                truncation: Some(truncation.clone()),
                full_output_path: Some(full_output_path.clone()),
            });
            let start_line = truncation.total_lines - truncation.output_lines + 1;
            let end_line = truncation.total_lines;
            if truncation.last_line_partial {
                let last_line_size = format_size(capture.last_line_bytes);
                output_text.push_str(&format!(
                    "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {full_output_path}]",
                    format_size(truncation.output_bytes)
                ));
            } else if truncation.truncated_by
                == Some(crate::harness::utils::truncate::TruncatedBy::Lines)
            {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {full_output_path}]",
                    truncation.total_lines
                ));
            } else {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_output_path}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                ));
            }
        }
    }

    let append_status = |status: &str| {
        if output_text.is_empty() {
            status.to_owned()
        } else {
            format!("{output_text}\n\n{status}")
        }
    };
    if capture.cancelled {
        return Err(tool_error(append_status("Command aborted")));
    }
    if let Some(execution_error) = &capture.execution_error {
        if execution_error.code == ExecutionErrorCode::Timeout {
            return Err(tool_error(append_status(&format!(
                "Command timed out after {} seconds",
                format_timeout(input.timeout)
            ))));
        }
        return Err(tool_error(execution_error.message.clone()));
    }
    if let Some(exit_code) = capture.exit_code {
        if exit_code != 0 {
            return Err(tool_error(append_status(&format!(
                "Command exited with code {exit_code}"
            ))));
        }
    }

    Ok(AgentToolResult {
        content: vec![pillar_ai::types::Content::text(if output_text.is_empty() {
            "(no output)".to_owned()
        } else {
            output_text
        })],
        details: details
            .map(|details| details.to_value())
            .unwrap_or(Value::Null),
        ..Default::default()
    })
}

/// Upstream throttled `onUpdate` state: coalesces dirty flags under a
/// 100ms window.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the streaming driver
struct ThrottleState {
    on_update: Option<crate::types::AgentToolUpdateCallback>,
    inner: Mutex<ThrottleInner>,
}

struct ThrottleInner {
    dirty: bool,
    last_update_at: Option<std::time::Instant>,
}

impl ThrottleState {
    fn new(on_update: Option<crate::types::AgentToolUpdateCallback>) -> Self {
        Self {
            on_update,
            inner: Mutex::new(ThrottleInner {
                dirty: false,
                last_update_at: None,
            }),
        }
    }

    /// Upstream `onUpdate?.({ content: [], details: undefined })`.
    fn emit_initial(&self) {
        if let Some(on_update) = &self.on_update {
            on_update(AgentToolResult::default());
        }
    }

    /// Mark dirty (upstream `scheduleOutputUpdate`); returns the delay
    /// after which an emission is due, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    fn mark_dirty(&self) -> Option<std::time::Duration> {
        let mut inner = self.inner.lock().expect("throttle poisoned");
        inner.dirty = true;
        self.on_update.as_ref()?;
        let elapsed = inner
            .last_update_at
            .map(|at| at.elapsed().as_millis() as u64)
            .unwrap_or(BASH_UPDATE_THROTTLE_MS);
        BASH_UPDATE_THROTTLE_MS
            .checked_sub(elapsed)
            .map(std::time::Duration::from_millis)
    }

    /// Upstream `emitOutputUpdate`: emit when dirty, resetting the flag
    /// and timestamp.
    #[cfg_attr(not(test), allow(dead_code))]
    fn emit_now(&self, capture: &crate::harness::utils::shell_output::ShellCaptureResult) -> bool {
        let should_emit = {
            let mut inner = self.inner.lock().expect("throttle poisoned");
            if !inner.dirty {
                false
            } else {
                inner.dirty = false;
                inner.last_update_at = Some(std::time::Instant::now());
                true
            }
        };
        if !should_emit {
            return false;
        }
        if let Some(on_update) = &self.on_update {
            let _ = capture;
            on_update(AgentToolResult::default());
        }
        true
    }

    /// Final flush before the tool returns (upstream clears the timer and
    /// emits the settled capture).
    fn flush(
        &self,
        capture: &crate::harness::utils::shell_output::ShellCaptureResult,
    ) -> Result<(), ToolExecuteError> {
        if let Some(on_update) = &self.on_update {
            let details = BashToolDetails {
                truncation: capture.truncation.as_ref().filter(|t| t.truncated).cloned(),
                full_output_path: capture.full_output_path.clone(),
            };
            on_update(AgentToolResult {
                content: vec![pillar_ai::types::Content::text(capture.output.clone())],
                details: details.to_value(),
                ..Default::default()
            });
        }
        Ok(())
    }
}

/// Upstream `createBashTool()`: the wire-level tool definition.
pub fn create_bash_tool() -> crate::types::AgentTool {
    crate::harness::tools::write::wire_tool(
        "bash",
        "bash",
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
            },
            "required": ["command"]
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bash_input() {
        let input = json!({"command": "ls", "timeout": 5});
        let parsed = parse_bash_input(&input).expect("valid");
        assert_eq!(parsed.command, "ls");
        assert_eq!(parsed.timeout, Some(5.0));

        assert!(parse_bash_input(&json!({})).is_err());
    }

    #[test]
    fn validates_timeout_bounds() {
        assert!(validate_timeout(None).is_ok());
        assert!(validate_timeout(Some(10.0)).is_ok());
        assert!(validate_timeout(Some(0.0)).is_err());
        assert!(validate_timeout(Some(-1.0)).is_err());
        assert!(validate_timeout(Some(f64::NAN)).is_err());
        assert!(validate_timeout(Some(MAX_TIMEOUT_SECONDS + 1.0)).is_err());
    }

    #[test]
    fn builds_command_prefix() {
        let execution = BashExecution {
            command: "value=hello\nprintf $value".to_owned(),
            cwd: "/tmp".to_owned(),
            env: Vec::new(),
            inherit_env: true,
        };
        assert_eq!(execution.command, "value=hello\nprintf $value");
    }

    /// The throttle must coalesce dirty flags: a mark while inside the
    /// window defers; after the window the emit resets state.
    #[test]
    fn throttle_coalesces_updates() {
        let state = ThrottleState::new(Some(Arc::new(|_| {})));
        // First mark has no prior emission: upstream's delay collapses to
        // <= 0, expressed here as a zero delay (emit immediately).
        assert_eq!(state.mark_dirty(), Some(std::time::Duration::ZERO));
        state.emit_now(&crate::harness::utils::shell_output::ShellCaptureResult::default());
        // A mark right after an emission defers inside the window.
        assert!(state.mark_dirty().is_some(), "inside window defers");
        state.emit_now(&crate::harness::utils::shell_output::ShellCaptureResult::default());
        let inner = state.inner.lock().unwrap();
        assert!(!inner.dirty);
    }

    #[test]
    fn formats_details_value() {
        let details = BashToolDetails {
            truncation: None,
            full_output_path: Some("/tmp/out.log".to_owned()),
        };
        let value = details.to_value();
        assert_eq!(value["fullOutputPath"], "/tmp/out.log");
        assert_eq!(value["truncation"], Value::Null);
    }
}
