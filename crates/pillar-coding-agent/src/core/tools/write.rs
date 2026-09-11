//! Port of packages/coding-agent/src/core/tools/write.ts (pi v0.84.3), the
//! execution core: create-or-overwrite writes under the file mutation
//! queue, with parent directory creation and abort-safe queue semantics.
//!
//! divergence: the syntax-highlight render cache and ToolDefinition/TUI
//! renderer half are not ported; abort checks are inline (upstream checks
//! `signal.aborted` after each await).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use pillar_agent::abort::AbortSignal;
use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;

use crate::core::tools::file_mutation_queue::{FileMutationQueue, global_file_mutation_queue};
use crate::core::tools::path_utils::resolve_to_cwd;

/// The write execution result (upstream `{ content, details }`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteResult {
    /// Upstream: "Successfully wrote N bytes to PATH".
    pub text: String,
}

/// The tool parameter shape as JSON (upstream `writeSchema`).
pub fn write_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
            "content": {"type": "string", "description": "Content to write to the file"}
        },
        "required": ["path", "content"]
    })
}

/// The tool description (upstream `description`, verbatim).
pub fn write_description() -> String {
    "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string()
}

/// Execute the write tool (upstream the `execute` body): parent directories
/// are created recursively and the write is serialized against other
/// mutations of the same file via the file mutation queue.
///
/// divergence: abort checks run inline before each filesystem step
/// (upstream checks after each await); the mutation queue is shared with
/// other tool executions through a process-global instance.
pub fn write(
    path: &str,
    content: &str,
    cwd: &str,
    signal: Option<&AbortSignal>,
    queue: &FileMutationQueue,
) -> Result<WriteResult, String> {
    let absolute_path = resolve_to_cwd(path, cwd);
    let dir = absolute_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    queue.with_file_mutation_queue(&absolute_path, || {
        let throw_if_aborted = || -> Result<(), String> {
            if signal.is_some_and(|s| s.is_aborted()) {
                return Err("Operation aborted".to_string());
            }
            Ok(())
        };

        throw_if_aborted()?;
        // Create parent directories if needed.
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory: {e}"))?;
        throw_if_aborted()?;

        // Write the file contents.
        fs::write(&absolute_path, content).map_err(|e| format!("Failed to write file: {e}"))?;
        throw_if_aborted()?;

        Ok(WriteResult {
            text: format!("Successfully wrote {} bytes to {path}", content.len()),
        })
    })?
}

/// Build the write tool as an `AgentTool` (upstream `createWriteTool`).
pub fn write_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "write".to_string(),
            description: write_description(),
            parameters: write_parameters_json(),
            constrained_sampling: None,
        },
        label: "write".to_string(),
        prepare_arguments: None,
        execute: Arc::new(move |_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: path".to_string())
                    })?;
                let content = args
                    .get("content")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: content".to_string())
                    })?;
                let result = write(
                    path,
                    content,
                    &cwd,
                    signal.as_ref(),
                    global_file_mutation_queue(),
                )
                .map_err(ToolExecuteError)?;
                Ok(AgentToolResult {
                    content: vec![Content::text(result.text)],
                    details: serde_json::Value::Null,
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}
