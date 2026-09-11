//! Port of packages/coding-agent/src/core/tools/edit.ts (pi v0.84.3), the
//! execution core: exact-text replacement with fuzzy fallback under the
//! file mutation queue, BOM stripping, line-ending preservation, and the
//! diff/patch details.
//!
//! divergence: the TUI preview/render half and the argument-repair
//! (`prepareEditArguments`) JSON-string coercion are not ported; abort
//! checks run inline before each step.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use pillar_agent::abort::AbortSignal;
use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;

use crate::core::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, generate_diff_string, generate_unified_patch,
    normalize_to_lf, restore_line_endings,
};
use crate::core::tools::file_mutation_queue::{FileMutationQueue, global_file_mutation_queue};
use crate::core::tools::path_utils::resolve_to_cwd;

/// The edit execution result (upstream `{ content, details }`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditResult {
    /// Upstream: "Successfully replaced N block(s) in PATH."
    pub text: String,
    /// Display-oriented diff of the changes made.
    pub diff: String,
    /// Standard unified patch of the changes made.
    pub patch: String,
    /// Line number of the first change in the new file.
    pub first_changed_line: Option<usize>,
}

/// Validate the edit input (upstream `validateEditInput`).
fn validate_edit_input(edits: &[Edit]) -> Result<(), String> {
    if edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        );
    }
    Ok(())
}

/// The tool parameter shape as JSON (upstream `editSchema`).
pub fn edit_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                        "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                    }
                },
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
            }
        },
        "required": ["path", "edits"]
    })
}

/// The tool description (upstream `description`, verbatim).
pub fn edit_description() -> String {
    "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_string()
}

/// Execute the edit tool (upstream the `execute` body).
pub fn edit(
    path: &str,
    edits: &[Edit],
    cwd: &str,
    signal: Option<&AbortSignal>,
    queue: &FileMutationQueue,
) -> Result<EditResult, String> {
    validate_edit_input(edits)?;
    let absolute_path = resolve_to_cwd(path, cwd);

    queue.with_file_mutation_queue(&absolute_path, || {
        let throw_if_aborted = || -> Result<(), String> {
            if signal.is_some_and(|s| s.is_aborted()) {
                return Err("Operation aborted".to_string());
            }
            Ok(())
        };

        throw_if_aborted()?;

        // Check if the file exists and is readable.
        if !absolute_path.is_file() {
            throw_if_aborted()?;
            return Err(format!("Could not edit file: {path}. Error code: ENOENT."));
        }
        throw_if_aborted()?;

        // Read the file.
        let buffer = fs::read(&absolute_path).map_err(|e| e.to_string())?;
        let raw_content = String::from_utf8_lossy(&buffer).to_string();
        throw_if_aborted()?;

        // Strip BOM before matching. The model will not include an invisible
        // BOM in oldText.
        let bom = if raw_content.starts_with('\u{feff}') {
            "\u{feff}"
        } else {
            ""
        };
        let content = raw_content.strip_prefix('\u{feff}').unwrap_or(&raw_content);
        let original_ending = crate::core::tools::edit_diff::detect_line_ending(content);
        let normalized_content = normalize_to_lf(content);
        let applied = apply_edits_to_normalized_content(&normalized_content, edits, path)?;
        throw_if_aborted()?;

        let final_content = format!(
            "{bom}{}",
            restore_line_endings(&applied.new_content, original_ending)
        );
        fs::write(&absolute_path, final_content)
            .map_err(|e| format!("Failed to write file: {e}"))?;
        throw_if_aborted()?;

        let (diff, first_changed_line) =
            generate_diff_string(&applied.base_content, &applied.new_content, 4);
        let patch = generate_unified_patch(path, &applied.base_content, &applied.new_content);
        Ok(EditResult {
            text: format!("Successfully replaced {} block(s) in {path}.", edits.len()),
            diff,
            patch,
            first_changed_line,
        })
    })?
}

/// Check whether a file exists and is writable (upstream `access` with
/// R_OK | W_OK) — exposed for callers validating before edit.
pub fn check_editable(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Error code: ENOENT".to_string());
    }
    // The port treats a readable regular file as writable-checkable; actual
    // permission errors surface at write time.
    if path.is_dir() {
        return Err("Error code: EISDIR".to_string());
    }
    Ok(())
}

/// Parse the `edits` argument (upstream `editSchema` items).
fn parse_edits(args: &serde_json::Value) -> Result<Vec<Edit>, ToolExecuteError> {
    let edits = args
        .get("edits")
        .and_then(|value| value.as_array())
        .ok_or_else(|| ToolExecuteError("Missing required parameter: edits".to_string()))?;
    let mut parsed = Vec::with_capacity(edits.len());
    for edit in edits {
        let old_text = edit
            .get("oldText")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ToolExecuteError("edits[].oldText must be a string".to_string()))?;
        let new_text = edit
            .get("newText")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ToolExecuteError("edits[].newText must be a string".to_string()))?;
        parsed.push(Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }
    Ok(parsed)
}

/// Build the edit tool as an `AgentTool` (upstream `createEditTool`).
pub fn edit_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "edit".to_string(),
            description: edit_description(),
            parameters: edit_parameters_json(),
            constrained_sampling: None,
        },
        label: "edit".to_string(),
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
                let edits = parse_edits(&args)?;
                let result = edit(
                    path,
                    &edits,
                    &cwd,
                    signal.as_ref(),
                    global_file_mutation_queue(),
                )
                .map_err(ToolExecuteError)?;
                Ok(AgentToolResult {
                    content: vec![Content::text(result.text)],
                    details: serde_json::json!({
                        "diff": result.diff,
                        "patch": result.patch,
                        "firstChangedLine": result.first_changed_line,
                    }),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}
