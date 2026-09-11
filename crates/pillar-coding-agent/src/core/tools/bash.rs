//! Port of packages/coding-agent/src/core/tools/bash.ts (pi v0.84.3), the
//! tool wrapper: schema, description, and the `AgentTool` over the bash
//! execution core in `crate::core::bash_executor`.
//!
//! divergence: the port executes synchronously and does not stream
//! `onUpdate` progress (upstream streams sanitized chunks while running);
//! the command prefix / custom shell configuration is not ported.

use std::sync::Arc;

use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;

use crate::core::bash_executor::{BashOutputSink, execute_bash_local};
use crate::core::truncate::DEFAULT_MAX_BYTES;
use crate::core::truncate::DEFAULT_MAX_LINES;

/// The tool parameter shape as JSON (upstream `bashSchema`).
pub fn bash_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Shell command to execute"},
            "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
        },
        "required": ["command"]
    })
}

/// The tool description (upstream `description` with the default shell name).
pub fn bash_description() -> String {
    format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    )
}

/// Build the bash tool as an `AgentTool` (upstream `createBashTool`).
pub fn bash_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "bash".to_string(),
            description: bash_description(),
            parameters: bash_parameters_json(),
            constrained_sampling: None,
        },
        label: "bash".to_string(),
        prepare_arguments: None,
        execute: Arc::new(move |_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return Err(ToolExecuteError("Operation aborted".to_string()));
                }
                let command = args
                    .get("command")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: command".to_string())
                    })?;
                let mut sink = NoopSink;
                let result = execute_bash_local(command, &cwd, &mut sink, signal.as_ref())
                    .map_err(ToolExecuteError)?;

                let mut details = serde_json::Map::new();
                if let Some(truncation) = &result.truncation {
                    details.insert("truncation".to_string(), truncation_json(truncation));
                }
                if let Some(path) = &result.full_output_path {
                    details.insert(
                        "fullOutputPath".to_string(),
                        serde_json::json!(path.to_string_lossy()),
                    );
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(result.output)],
                    details: serde_json::Value::Object(details),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}

struct NoopSink;
impl BashOutputSink for NoopSink {
    fn on_chunk(&mut self, _chunk: &str) {}
}

/// Serialize a truncation result to the upstream camelCase shape.
fn truncation_json(truncation: &crate::core::truncate::TruncationResult) -> serde_json::Value {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": truncation.truncated_by.map(|by| match by {
            crate::core::truncate::TruncatedBy::Lines => "lines",
            crate::core::truncate::TruncatedBy::Bytes => "bytes",
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
}
