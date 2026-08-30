//! Port of packages/agent/src/harness/tools/write.ts (pi v0.84.3).
//!
//! divergence: upstream throws on abort; the port returns
//! `ToolExecuteError`. The upstream JSON schema maps to manual input
//! parsing over `serde_json::Value` (the typebox dependency is not ported).

use serde_json::{Value, json};

use pillar_ai::types::Tool;

use crate::harness::tools::file_mutation_queue::FileMutationQueues;
use crate::harness::tools::path_utils::resolve_tool_path;
use crate::harness::types::{ExecutionEnv, FileError};
use crate::types::{AgentToolResult, ToolExecuteError};

/// Upstream `WriteToolInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteToolInput {
    pub path: String,
    pub content: String,
}

/// Parse the write tool input (upstream typebox validation).
pub fn parse_write_input(input: &Value) -> Result<WriteToolInput, ToolExecuteError> {
    let object = input
        .as_object()
        .ok_or_else(|| ToolExecuteError("write input must be an object".to_owned()))?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolExecuteError("write input requires a string path".to_owned()))?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolExecuteError("write input requires a string content".to_owned()))?;
    Ok(WriteToolInput {
        path: path.to_owned(),
        content: content.to_owned(),
    })
}

fn tool_error(message: impl Into<String>) -> ToolExecuteError {
    ToolExecuteError(message.into())
}

/// Upstream `createWriteTool().execute`: write content, creating parent
/// directories. The mutation queue serializes writes to the same
/// canonical path.
pub async fn execute_write_tool<E: ExecutionEnv + ?Sized>(
    env: &E,
    input: &Value,
    signal: Option<&crate::abort::AbortSignal>,
    queues: &FileMutationQueues,
) -> Result<AgentToolResult, ToolExecuteError> {
    let input = parse_write_input(input)?;
    let absolute_path = resolve_tool_path(env, &input.path)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    queues
        .with_mutation_queue(env, &absolute_path, || async {
            let result: Result<AgentToolResult, ToolExecuteError> = async {
                if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                    return Err(tool_error("Operation aborted"));
                }
                env.write_file(&absolute_path, input.content.as_bytes())
                    .await
                    .map_err(|error: FileError| tool_error(error.to_string()))?;
                if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                    return Err(tool_error("Operation aborted"));
                }
                Ok(AgentToolResult {
                    content: vec![pillar_ai::types::Content::text(format!(
                        "Successfully wrote {} bytes to {}",
                        input.content.len(),
                        input.path
                    ))],
                    ..Default::default()
                })
            }
            .await;
            // The mutation queue's error channel is FileError; carry tool
            // failures through it and re-map to ToolExecuteError outside.
            result.map_err(|error| {
                FileError::new(crate::harness::types::FileErrorCode::Unknown, error.0, None)
            })
        })
        .await
        .map_err(|error| tool_error(error.to_string()))
}

/// Upstream `createWriteTool()`: the wire-level tool definition.
pub fn create_write_tool() -> crate::types::AgentTool {
    wire_tool(
        "write",
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        }),
    )
}

/// Build a wire-level `AgentTool` with no executable binding yet (the
/// harness binds `execute` with a resolved context snapshot).
pub(crate) fn wire_tool(
    name: &str,
    label: &str,
    description: &str,
    parameters: Value,
) -> crate::types::AgentTool {
    crate::types::AgentTool {
        tool: Tool {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
            constrained_sampling: None,
        },
        label: label.to_owned(),
        prepare_arguments: None,
        execute: std::sync::Arc::new(
            |_tool_call_id: String,
             _args: serde_json::Value,
             _signal: Option<crate::abort::AbortSignal>,
             _on_update: Option<crate::types::AgentToolUpdateCallback>| {
                Box::pin(async {
                    Err(ToolExecuteError(
                        "tool execute binding requires a harness context".to_owned(),
                    ))
                })
            },
        ),
        execution_mode: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_write_input() {
        let input = json!({"path": "a.txt", "content": "hi"});
        let parsed = parse_write_input(&input).expect("valid");
        assert_eq!(parsed.path, "a.txt");
        assert_eq!(parsed.content, "hi");

        assert!(parse_write_input(&json!({"path": "a.txt"})).is_err());
        assert!(parse_write_input(&json!({"path": 1, "content": "x"})).is_err());
        assert!(parse_write_input(&json!([])).is_err());
    }
}
