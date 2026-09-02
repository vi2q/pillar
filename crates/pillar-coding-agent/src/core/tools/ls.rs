//! Port of packages/coding-agent/src/core/tools/ls.ts (pi v0.84.3), the
//! execution core: alphabetical directory listing with `/` suffixes for
//! directories, entry limits, byte truncation, and actionable notices.
//!
//! divergence: upstream exposes a ToolDefinition with TUI renderers; the
//! port provides the `ls` execution function over pluggable operations
//! (upstream `LsOperations`), returning the content/details shapes. The
//! rendering half and tool wrapping are not ported.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_head};

/// Default entry limit (upstream `DEFAULT_LIMIT`).
pub const DEFAULT_LIMIT: usize = 500;

/// Pluggable directory-listing operations (upstream `LsOperations`).
pub trait LsOperations {
    fn exists(&self, absolute_path: &Path) -> bool;
    /// Returns true when the path is a directory; an error string when the
    /// stat fails.
    fn is_directory(&self, absolute_path: &Path) -> Result<bool, String>;
    fn readdir(&self, absolute_path: &Path) -> Result<Vec<String>, String>;
}

/// Local filesystem operations (upstream `defaultLsOperations`).
pub struct LocalLsOperations;

impl LsOperations for LocalLsOperations {
    fn exists(&self, absolute_path: &Path) -> bool {
        absolute_path.exists()
    }

    fn is_directory(&self, absolute_path: &Path) -> Result<bool, String> {
        fs::metadata(absolute_path)
            .map(|meta| meta.is_dir())
            .map_err(|e| e.to_string())
    }

    fn readdir(&self, absolute_path: &Path) -> Result<Vec<String>, String> {
        let entries =
            fs::read_dir(absolute_path).map_err(|e| format!("Cannot read directory: {e}"))?;
        let mut names = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => names.push(entry.file_name().to_string_lossy().to_string()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(names)
    }
}

/// The ls execution result (upstream `{ content, details }`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LsToolResult {
    pub text: String,
    /// Entry limit hit, with the effective limit (upstream
    /// `details.entryLimitReached`).
    pub entry_limit_reached: Option<usize>,
    /// Set when byte truncation occurred (upstream `details.truncation`).
    pub truncation_max_bytes: Option<usize>,
}

/// Execute the ls tool (upstream the `execute` body).
pub fn ls(
    path: Option<&str>,
    limit: Option<usize>,
    cwd: &str,
    ops: &dyn LsOperations,
) -> Result<LsToolResult, String> {
    let dir_path = resolve_to_cwd(path.unwrap_or("."), cwd);
    let effective_limit = limit.unwrap_or(DEFAULT_LIMIT);

    if !ops.exists(&dir_path) {
        return Err(format!("Path not found: {}", dir_path.display()));
    }
    if !ops.is_directory(&dir_path)? {
        return Err(format!("Not a directory: {}", dir_path.display()));
    }
    let entries = ops.readdir(&dir_path)?;

    // Sort alphabetically, case-insensitive.
    let mut entries = entries;
    entries.sort_by_key(|a| a.to_lowercase());

    // Format entries with directory indicators.
    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in &entries {
        if results.len() >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = dir_path.join(entry);
        let Ok(is_dir) = ops.is_directory(&full_path) else {
            // Skip entries we cannot stat.
            continue;
        };
        results.push(if is_dir {
            format!("{entry}/")
        } else {
            entry.clone()
        });
    }

    if results.is_empty() {
        return Ok(LsToolResult {
            text: "(empty directory)".to_string(),
            ..Default::default()
        });
    }

    let raw_output = results.join("\n");
    // Byte truncation only: entry count is already capped.
    let truncation = truncate_head(
        &raw_output,
        TruncationOptions {
            max_lines: Some(usize::MAX),
            max_bytes: None,
        },
    );
    let mut output = truncation.content.clone();
    let mut details = LsToolResult::default();
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{} entries limit reached. Use limit={} for more",
            effective_limit,
            effective_limit * 2
        ));
        details.entry_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached",
            crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
        ));
        details.truncation_max_bytes = Some(DEFAULT_MAX_BYTES);
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }

    details.text = output;
    Ok(details)
}

/// The tool parameter shape as JSON for schema-aware callers (upstream
/// `lsSchema`).
pub fn ls_parameters_json() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory to list (default: current directory)"},
            "limit": {"type": "number", "description": "Maximum number of entries to return (default: 500)"}
        }
    })
}

/// The tool description (upstream `description`, verbatim).
pub fn ls_description() -> String {
    format!(
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {} entries or {}KB (whichever is hit first).",
        DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    )
}
