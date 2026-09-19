//! Port of packages/coding-agent/src/core/tools/read.ts (pi v0.84.3), the
//! execution core: text file reads with offset/limit windows, actionable
//! continuation notices, and truncation details.
//!
//! divergence: images are detected by extension/magic prefix and returned
//! as a text note only (image processing/resizing and vision-model
//! attachment land with the images port); the ToolDefinition/TUI renderer
//! half and the pi-docs compact classification are not ported.

use std::fs;
use std::io::BufRead as _;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;

use crate::core::tools::path_utils::resolve_read_path;
use crate::core::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    truncate_head_with_totals,
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
    /// Truncation details, present when head truncation occurred (upstream
    /// `details.truncation`).
    pub truncation: Option<TruncationResult>,
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
    // Text content. The window is read in one streaming pass: the head kept in
    // memory is bounded by the truncation limits, so a windowed read of a huge
    // file no longer materializes the whole file (nor a per-line slice
    // vector). See [`read_text_window`].
    let window = read_text_window(&absolute_path, offset, limit)?;
    let total_file_lines = window.file_lines;
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
    // If limit is specified by the user, honor it first. Otherwise
    // truncateHead decides.
    let user_limited_lines = limit.map(|_| window.selection_lines);
    // Apply truncation, respecting both line and byte limits. The totals come
    // from the streaming pass over the whole selection.
    let truncation = truncate_head_with_totals(
        &window.head,
        window.truncation_lines(),
        window.selection_bytes,
        TruncationOptions::default(),
    );
    let mut output_text: String;
    let mut result = ReadResult {
        total_file_lines,
        ..Default::default()
    };
    if truncation.first_line_exceeds_limit {
        // First line alone exceeds the byte limit. Point the model at a
        // bash fallback.
        let first_line_size = window.head.split('\n').next().map_or(0, str::len);
        output_text = format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
            crate::core::truncate::format_size(first_line_size),
            crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
        );
        result.truncated = true;
        result.truncation = Some(truncation.clone());
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
        result.truncation = Some(truncation.clone());
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

/// One streaming pass over a text file: the selected line window, kept only as
/// far as truncation needs it, plus the counts the notices report.
struct TextWindow {
    /// Head of the selected window (`join("\n")` of its lines).
    head: String,
    /// Lines in the whole file (`split('\n')` length).
    file_lines: usize,
    /// Lines and bytes of the selected window before truncation.
    selection_lines: usize,
    selection_bytes: usize,
    /// `\n` separators inside the selection, and whether it ends with one:
    /// `truncate_head` counts lines without the trailing empty element (and
    /// counts none for empty content), while the notices count
    /// `split('\n')` elements.
    selection_newlines: usize,
    selection_ends_with_newline: bool,
}

impl TextWindow {
    /// The selected window's line count as `truncate_head` reports it.
    fn truncation_lines(&self) -> usize {
        if self.selection_bytes == 0 {
            0
        } else if self.selection_ends_with_newline {
            self.selection_newlines
        } else {
            self.selection_newlines + 1
        }
    }
}

/// Read the `offset`/`limit` window of `path` in a single streaming pass.
///
/// The window is the `join("\n")` of the `split('\n')` elements
/// `[start, min(start + limit, split('\n').len()))` — the string the whole-file
/// read used to build — but only its head is kept. The whole file is scanned
/// because its line count is part of the result, yet a windowed read of a huge
/// file no longer materializes it: once the head holds `DEFAULT_MAX_LINES`
/// lines or more than `DEFAULT_MAX_BYTES` bytes (which puts the truncation cut
/// inside it) the pass keeps counting without storing.
fn read_text_window(
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<TextWindow, String> {
    fn in_window(index: usize, start_line: usize, end_line: Option<usize>) -> bool {
        index >= start_line && end_line.is_none_or(|end_line| index < end_line)
    }

    /// Append one `split('\n')` element to the window, storing it only while
    /// the head can still grow.
    fn push_element(window: &mut TextWindow, head_lines: &mut usize, text: &str) {
        let store = *head_lines == 0
            || (*head_lines < DEFAULT_MAX_LINES && window.head.len() <= DEFAULT_MAX_BYTES);
        if window.selection_lines > 0 {
            window.selection_newlines += 1;
            window.selection_bytes += 1;
            if store {
                window.head.push('\n');
            }
        }
        window.selection_bytes += text.len();
        window.selection_lines += 1;
        window.selection_ends_with_newline = text.is_empty() && window.selection_newlines > 0;
        if store {
            window.head.push_str(text);
            *head_lines += 1;
        }
    }

    let start_line = offset.map_or(0, |o| o.saturating_sub(1));
    let end_line = limit.map(|limit| start_line.saturating_add(limit));

    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(file);
    let mut element: Vec<u8> = Vec::new();
    let mut window = TextWindow {
        head: String::new(),
        file_lines: 0,
        selection_lines: 0,
        selection_bytes: 0,
        selection_newlines: 0,
        selection_ends_with_newline: false,
    };
    let mut head_lines = 0usize;
    let mut index = 0usize;
    let mut newlines = 0usize;

    loop {
        element.clear();
        let read = reader
            .read_until(b'\n', &mut element)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        // Elements break at `\n`, so a multi-byte character never spans two of
        // them: converting each element lossily matches converting the whole
        // file at once.
        let terminated = element.last() == Some(&b'\n');
        if terminated {
            newlines += 1;
        }
        let content = if terminated {
            &element[..element.len() - 1]
        } else {
            &element[..]
        };
        if in_window(index, start_line, end_line) {
            push_element(
                &mut window,
                &mut head_lines,
                &String::from_utf8_lossy(content),
            );
        }
        index += 1;
    }

    // `split('\n')` has one element after a trailing newline: the empty tail
    // (and the single element of an empty file). It is not read by the loop.
    if index == newlines && in_window(index, start_line, end_line) {
        push_element(&mut window, &mut head_lines, "");
    }

    window.file_lines = newlines + 1;
    Ok(window)
}

/// Serialize a truncation result to the tool `details` shape (upstream
/// `TruncationResult`, camelCase).
fn truncation_to_json(truncation: &TruncationResult) -> serde_json::Value {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": truncation.truncated_by.map(|by| match by {
            TruncatedBy::Lines => "lines",
            TruncatedBy::Bytes => "bytes",
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

/// Build the read tool as an `AgentTool` (upstream `createReadTool`).
pub fn read_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "read".to_string(),
            description: read_description(),
            parameters: read_parameters_json(),
            constrained_sampling: None,
        },
        label: "read".to_string(),
        prepare_arguments: None,
        execute: Arc::new(move |_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return Err(ToolExecuteError("Operation aborted".to_string()));
                }
                let path = args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: path".to_string())
                    })?;
                let offset = args
                    .get("offset")
                    .and_then(|value| value.as_u64())
                    .map(|value| value as usize);
                let limit = args
                    .get("limit")
                    .and_then(|value| value.as_u64())
                    .map(|value| value as usize);
                let result = read(path, offset, limit, &cwd).map_err(ToolExecuteError)?;

                let mut content = Vec::new();
                match &result.image {
                    Some(image) => {
                        content.push(Content::text(result.text.clone()));
                        content.push(Content::Image {
                            data: base64::engine::general_purpose::STANDARD.encode(&image.data),
                            mime_type: image.mime_type.clone(),
                        });
                    }
                    None => content.push(Content::text(result.text)),
                }
                let details = match &result.truncation {
                    Some(truncation) => {
                        serde_json::json!({ "truncation": truncation_to_json(truncation) })
                    }
                    None => serde_json::Value::Null,
                };
                Ok(AgentToolResult {
                    content,
                    details,
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::truncate::truncate_head;

    /// The whole-file selection the streaming window replaced: the reference
    /// this differential test compares against.
    fn reference_window(
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> (usize, usize, usize, String) {
        let text = String::from_utf8_lossy(&fs::read(path).expect("read")).into_owned();
        let all_lines: Vec<&str> = text.split('\n').collect();
        let total = all_lines.len();
        let start = offset.map_or(0, |o| o.saturating_sub(1));
        let end = limit.map_or(total, |limit| start.saturating_add(limit).min(total));
        let (selection, lines) = if start >= total {
            (String::new(), 0)
        } else {
            (all_lines[start..end].join("\n"), end - start)
        };
        (total, lines, selection.len(), selection)
    }

    /// The windowed streaming read must agree with the whole-file read for
    /// every file shape: the counts the notices report, and the truncation
    /// result (whose totals come from the scan, not from the buffer).
    #[test]
    fn streaming_window_matches_the_whole_file_reference() {
        let dir = std::env::temp_dir().join(format!("pillar-read-window-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let long_line = "x".repeat(60 * 1024);
        let many_lines: String = (0..(DEFAULT_MAX_LINES + 400))
            .map(|index| format!("line {index}\n"))
            .collect();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty.txt", Vec::new()),
            ("no-trailing.txt", b"a\nb\nc".to_vec()),
            ("trailing.txt", b"a\nb\nc\n".to_vec()),
            ("blank-lines.txt", b"a\n\n\nb\n".to_vec()),
            ("crlf.txt", b"a\r\nb\r\n".to_vec()),
            ("invalid-utf8.bin", b"a\xffb\nc\xfed\n".to_vec()),
            (
                "multibyte.txt",
                "\u{3b1}\n\u{3b2}\u{3b3}\n".as_bytes().to_vec(),
            ),
            (
                "long-line.txt",
                format!("{long_line}\nshort\n").into_bytes(),
            ),
            ("many-lines.txt", many_lines.into_bytes()),
        ];
        let offsets = [
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(usize::MAX),
            Some(DEFAULT_MAX_LINES + 10),
        ];
        let limits = [None, Some(0), Some(1), Some(2), Some(usize::MAX)];

        for (name, bytes) in &cases {
            let path = dir.join(name);
            fs::write(&path, bytes).expect("write case");
            for offset in offsets {
                for limit in limits {
                    let (total, lines, byte_len, selection) =
                        reference_window(&path, offset, limit);
                    let window = read_text_window(&path, offset, limit).expect("window");

                    let context = (name, offset, limit);
                    assert_eq!(window.file_lines, total, "{context:?}");
                    assert_eq!(window.selection_lines, lines, "{context:?}");
                    assert_eq!(window.selection_bytes, byte_len, "{context:?}");
                    assert!(
                        selection.starts_with(&window.head),
                        "head is not a prefix of the selection: {context:?}"
                    );
                    assert_eq!(
                        truncate_head_with_totals(
                            &window.head,
                            window.truncation_lines(),
                            window.selection_bytes,
                            TruncationOptions::default(),
                        ),
                        truncate_head(&selection, TruncationOptions::default()),
                        "{context:?}"
                    );
                }
            }
        }
    }
}
