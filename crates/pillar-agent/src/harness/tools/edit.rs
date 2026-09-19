//! Port of packages/agent/src/harness/tools/edit.ts (pi v0.84.3).

use serde_json::Value;

use crate::harness::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, normalize_to_lf,
    render_edit_diffs, restore_line_endings, strip_bom,
};
use crate::harness::tools::file_mutation_queue::FileMutationQueues;
use crate::harness::tools::path_utils::resolve_tool_path;
use crate::harness::types::{ExecutionEnv, FileKind};
use crate::types::{AgentToolResult, ToolExecuteError};

/// Upstream `EditToolInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct EditToolInput {
    pub path: String,
    pub edits: Vec<Edit>,
}

/// Upstream `EditToolDetails` serialized into `AgentToolResult.details`.
#[derive(Debug, Clone, PartialEq)]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

impl EditToolDetails {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "diff": self.diff,
            "patch": self.patch,
            "firstChangedLine": self.first_changed_line,
        })
    }
}

fn tool_error(message: impl Into<String>) -> ToolExecuteError {
    ToolExecuteError(message.into())
}

/// Upstream `prepareEditArguments`: coerce stringified/single-edit legacy
/// shapes into the canonical `{ path, edits: [...] }` form.
pub fn prepare_edit_arguments(input: &Value) -> Value {
    let Value::Object(args) = input else {
        return input.clone();
    };
    let mut args = args.clone();

    match args.get("edits") {
        Some(Value::String(text)) => {
            let parsed: Result<Value, _> = serde_json::from_str(text);
            match parsed {
                Ok(Value::Array(items)) => {
                    args.insert("edits".to_owned(), Value::Array(items));
                }
                Ok(parsed) if is_single_edit_input(&parsed) => {
                    args.insert("edits".to_owned(), Value::Array(vec![parsed]));
                }
                _ => {}
            }
        }
        Some(single) if is_single_edit_input(single) => {
            args.insert("edits".to_owned(), Value::Array(vec![single.clone()]));
        }
        _ => {}
    }

    let legacy_old = args.get("oldText").and_then(Value::as_str);
    let legacy_new = args.get("newText").and_then(Value::as_str);
    let (Some(old_text), Some(new_text)) = (legacy_old, legacy_new) else {
        return Value::Object(args);
    };
    let mut edits = match args.get("edits") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    edits.push(serde_json::json!({"oldText": old_text, "newText": new_text}));
    args.remove("oldText");
    args.remove("newText");
    args.insert("edits".to_owned(), Value::Array(edits));
    Value::Object(args)
}

fn is_single_edit_input(value: &Value) -> bool {
    let Value::Object(object) = value else {
        return false;
    };
    object.get("oldText").is_some_and(Value::is_string)
        && object.get("newText").is_some_and(Value::is_string)
}

/// Upstream `validateEditInput`.
fn validate_edit_input(input: &Value) -> Result<EditToolInput, ToolExecuteError> {
    let object = input.as_object().ok_or_else(|| {
        tool_error("Edit tool input is invalid. edits must contain at least one replacement.")
    })?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| tool_error("Edit tool input requires a string path"))?
        .to_owned();
    let edits_value = object.get("edits").and_then(Value::as_array);
    let Some(items) = edits_value else {
        return Err(tool_error(
            "Edit tool input is invalid. edits must contain at least one replacement.",
        ));
    };
    if items.is_empty() {
        return Err(tool_error(
            "Edit tool input is invalid. edits must contain at least one replacement.",
        ));
    }
    let edits = items
        .iter()
        .map(|item| {
            Ok(Edit {
                old_text: item
                    .get("oldText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| tool_error("each edit requires string oldText"))?
                    .to_owned(),
                new_text: item
                    .get("newText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| tool_error("each edit requires string newText"))?
                    .to_owned(),
            })
        })
        .collect::<Result<Vec<Edit>, ToolExecuteError>>()?;
    Ok(EditToolInput { path, edits })
}

fn edit_access_error(path: &str, error: &crate::harness::types::FileError) -> ToolExecuteError {
    tool_error(format!(
        "Could not edit file: {path}. Error code: {}.",
        error.code.as_str()
    ))
}

/// Upstream `createEditTool().execute`.
pub async fn execute_edit_tool<E: ExecutionEnv + ?Sized>(
    env: &E,
    input: &Value,
    signal: Option<&crate::abort::AbortSignal>,
    queues: &FileMutationQueues,
) -> Result<AgentToolResult, ToolExecuteError> {
    let input = validate_edit_input(input)?;
    let absolute_path = resolve_tool_path(env, &input.path)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    queues
        .with_mutation_queue(env, &absolute_path, || async {
            if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                return Err(tool_error("Operation aborted"));
            }
            let info = env.file_info(&absolute_path).await;
            let info = match info {
                Ok(info) => info,
                Err(error) => return Err(edit_access_error(&input.path, &error)),
            };
            if info.kind != FileKind::File && info.kind != FileKind::Symlink {
                return Err(tool_error(format!(
                    "Could not edit file: {}. Path is not a file.",
                    input.path
                )));
            }

            let content = match env.read_text_file(&absolute_path).await {
                Ok(content) => content,
                Err(error) => return Err(edit_access_error(&input.path, &error)),
            };
            if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                return Err(tool_error("Operation aborted"));
            }

            let (bom, text) = strip_bom(&content);
            let original_ending = detect_line_ending(text);
            let normalized_content = normalize_to_lf(text);
            let applied = match apply_edits_to_normalized_content(
                &normalized_content,
                &input.edits,
                &input.path,
            ) {
                Ok(applied) => applied,
                Err(message) => return Err(tool_error(message)),
            };
            if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                return Err(tool_error("Operation aborted"));
            }

            let final_content = format!(
                "{bom}{}",
                restore_line_endings(&applied.new_content, original_ending)
            );
            if let Err(error) = env
                .write_file(&absolute_path, final_content.as_bytes())
                .await
            {
                return Err(edit_access_error(&input.path, &error));
            }
            if signal.is_some_and(crate::abort::AbortSignal::is_aborted) {
                return Err(tool_error("Operation aborted"));
            }

            let rendering =
                render_edit_diffs(&input.path, &applied.base_content, &applied.new_content, 4);
            let details = EditToolDetails {
                diff: rendering.diff,
                patch: rendering.patch,
                first_changed_line: rendering.first_changed_line,
            };
            Ok(AgentToolResult {
                content: vec![pillar_ai::types::Content::text(format!(
                    "Successfully replaced {} block(s) in {}.",
                    input.edits.len(),
                    input.path
                ))],
                details: details.to_value(),
                ..Default::default()
            })
        })
        .await
}

/// Upstream `createEditTool()`: the wire-level tool definition.
pub fn create_edit_tool() -> crate::types::AgentTool {
    let mut tool = crate::harness::tools::write::wire_tool(
        "edit",
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                            "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        }),
    );
    let prepare: std::sync::Arc<crate::types::PrepareArgumentsFn> =
        std::sync::Arc::new(|input: &Value| prepare_edit_arguments(input));
    tool.prepare_arguments = Some(prepare);
    tool
}
