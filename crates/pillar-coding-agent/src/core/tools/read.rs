//! Port of packages/coding-agent/src/core/tools/read.ts (pi v0.84.3), the
//! execution core: text file reads with offset/limit windows, actionable
//! continuation notices, and truncation details.
//!
//! divergence: images are detected by extension/magic prefix and returned
//! as a text note only (image processing/resizing and vision-model
//! attachment land with the images port); the ToolDefinition/TUI renderer
//! half and the pi-docs compact classification are not ported.

use std::fs;
use std::path::Path;

use crate::core::tools::path_utils::resolve_read_path;
use crate::core::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, truncate_head,
};

/// Image extensions supported by upstream (jpg, png, gif, webp, bmp).
const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
        .unwrap_or(false)
}

/// The read execution result (upstream `{ content, details }`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadResult {
    /// The text content (or image note).
    pub text: String,
    /// Image payload when the file is a supported image (base64 data).
    pub image: Option<ImageAttachment>,
    /// Set when byte/line truncation occurred (upstream
    /// `details.truncation`); `total_lines` is the full file line count.
    pub truncated: bool,
    pub total_file_lines: usize,
}

/// An image attachment (upstream `ImageContent` subset).
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAttachment {
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// The tool parameter shape as JSON (upstream `readSchema`).
pub fn read_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
            "offset": {"type": "number", "description": "Line number to start reading from (1-indexed)"},
            "limit": {"type": "number", "description": "Maximum number of lines to read"}
        },
        "required": ["path"]
    })
}

/// The tool description (upstream `description`, verbatim).
pub fn read_description() -> String {
    format!(
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES / 1024
    )
}

/// Execute the read tool (upstream the `execute` body).
pub fn read(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    cwd: &str,
) -> Result<ReadResult, String> {
    let absolute_path = resolve_read_path(path, cwd);

    // Check if the file exists and is readable.
    if !absolute_path.is_file() {
        return Err(format!(
            "File not found or is not a regular file: {}",
            absolute_path.display()
        ));
    }

    if is_image_path(&absolute_path) {
        // Image note (upstream reads + processes the image; the port returns
        // the raw bytes for the caller to attach).
        let data = fs::read(&absolute_path).map_err(|e| e.to_string())?;
        let mime_type = match absolute_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            _ => "application/octet-stream",
        };
        return Ok(ReadResult {
            text: format!("Read image file [{mime_type}]"),
            image: Some(ImageAttachment {
                data,
                mime_type: mime_type.to_string(),
            }),
            ..Default::default()
        });
    }

    // Text content.
    let buffer = fs::read(&absolute_path).map_err(|e| e.to_string())?;
    let text_content = String::from_utf8_lossy(&buffer);
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    // Apply offset if specified. Convert from 1-indexed input to 0-indexed
    // array access.
    let start_line = offset.map_or(0, |o| o.saturating_sub(1));
    let start_line_display = start_line + 1;
    // Check if offset is out of bounds.
    if start_line >= total_file_lines {
        return Err(format!(
            "Offset {} is beyond end of file ({total_file_lines} lines total)",
            offset.map_or_else(|| "undefined".to_string(), |o| o.to_string())
        ));
    }
    let selected_content: String;
    let mut user_limited_lines: Option<usize> = None;
    // If limit is specified by the user, honor it first. Otherwise
    // truncateHead decides.
    if let Some(limit) = limit {
        let end_line = (start_line + limit).min(total_file_lines);
        selected_content = all_lines[start_line..end_line].join("\n");
        user_limited_lines = Some(end_line - start_line);
    } else {
        selected_content = all_lines[start_line..].join("\n");
    }
    // Apply truncation, respecting both line and byte limits.
    let truncation = truncate_head(&selected_content, TruncationOptions::default());
    let mut output_text: String;
    let mut result = ReadResult {
        total_file_lines,
        ..Default::default()
    };
    if truncation.first_line_exceeds_limit {
        // First line alone exceeds the byte limit. Point the model at a
        // bash fallback.
        let first_line_size = all_lines[start_line].len();
        output_text = format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
            crate::core::truncate::format_size(first_line_size),
            crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
        );
        result.truncated = true;
    } else if truncation.truncated {
        // Truncation occurred. Build an actionable continuation notice.
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        output_text = truncation.content.clone();
        if truncation.truncated_by == Some(crate::core::truncate::TruncatedBy::Lines) {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            ));
        } else {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
            ));
        }
        result.truncated = true;
    } else if let Some(user_limited) = user_limited_lines {
        if start_line + user_limited < total_file_lines {
            // User-specified limit stopped early, but the file still has
            // more content.
            let remaining = total_file_lines - (start_line + user_limited);
            let next_offset = start_line + user_limited + 1;
            output_text = format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            );
        } else {
            output_text = truncation.content;
        }
    } else {
        // No truncation and no remaining user-limited content.
        output_text = truncation.content;
    }

    result.text = output_text;
    Ok(result)
}
