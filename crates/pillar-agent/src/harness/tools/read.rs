//! Port of packages/agent/src/harness/tools/read.ts (pi v0.84.3).

use serde_json::Value;

use crate::harness::tools::image::detect_supported_image_mime_type;
use crate::harness::tools::path_utils::resolve_read_tool_path;
use crate::harness::types::ExecutionEnv;
use crate::harness::utils::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head,
};
use crate::types::{AgentToolResult, ToolExecuteError};

/// Upstream `ReadToolInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadToolInput {
    pub path: String,
    pub offset: Option<f64>,
    pub limit: Option<f64>,
}

/// Parse the read tool input (upstream typebox validation).
pub fn parse_read_input(input: &Value) -> Result<ReadToolInput, ToolExecuteError> {
    let object = input
        .as_object()
        .ok_or_else(|| ToolExecuteError("read input must be an object".to_owned()))?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolExecuteError("read input requires a string path".to_owned()))?;
    let number = |key: &str| {
        object.get(key).and_then(Value::as_f64).map(|value| {
            if value.is_finite() {
                Ok(value)
            } else {
                Err(ToolExecuteError(format!("read input {key} must be finite")))
            }
        })
    };
    Ok(ReadToolInput {
        path: path.to_owned(),
        offset: number("offset").transpose()?,
        limit: number("limit").transpose()?,
    })
}

/// Upstream `ReadImageProcessorResult`.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadImageProcessorResult {
    Ok {
        data: String,
        mime_type: String,
        hints: Vec<String>,
    },
    Err {
        message: String,
    },
}

/// Upstream `ReadImageProcessor`: convert/resize detected image bytes.
pub type ReadImageProcessor =
    std::sync::Arc<dyn Fn(Vec<u8>, &str, bool) -> ReadImageProcessorFuture + Send + Sync>;

/// Future produced by [`ReadImageProcessor`].
pub type ReadImageProcessorFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ReadImageProcessorResult> + Send>>;

/// Upstream `ReadToolDetails` serialized into `AgentToolResult.details`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadToolDetails {
    pub truncation: Option<crate::harness::utils::truncate::TruncationResult>,
}

impl ReadToolDetails {
    fn to_value(&self) -> Value {
        // Upstream serializes { truncation?: TruncationResult } camelCase.
        match &self.truncation {
            None => Value::Null,
            Some(truncation) => serde_json::json!({
                "truncation": {
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
                }
            }),
        }
    }
}

fn tool_error(message: impl Into<String>) -> ToolExecuteError {
    ToolExecuteError(message.into())
}

/// Upstream `createReadTool().execute`.
pub async fn execute_read_tool<E: ExecutionEnv + ?Sized>(
    env: &E,
    input: &Value,
    options: Option<&ReadToolOptions>,
) -> Result<AgentToolResult, ToolExecuteError> {
    let input = parse_read_input(input)?;
    let absolute_path = resolve_read_tool_path(env, &input.path)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    let bytes = env
        .read_binary_file(&absolute_path)
        .await
        .map_err(|error| tool_error(error.to_string()))?;

    if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
        if let Some(processor) = options.and_then(|options| options.image_processor.as_ref()) {
            let auto_resize =
                options.is_some_and(|options| options.auto_resize_images.unwrap_or(true));
            let processed = processor(bytes, mime_type, auto_resize).await;
            return match processed {
                ReadImageProcessorResult::Err { message } => Ok(AgentToolResult {
                    content: vec![pillar_ai::types::Content::text(format!(
                        "Read image file [{mime_type}]\n{message}"
                    ))],
                    ..Default::default()
                }),
                ReadImageProcessorResult::Ok {
                    data,
                    mime_type: processed_mime,
                    hints,
                } => {
                    let hints = if hints.is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", hints.join("\n"))
                    };
                    Ok(AgentToolResult {
                        content: vec![
                            pillar_ai::types::Content::text(format!(
                                "Read image file [{processed_mime}]{hints}"
                            )),
                            pillar_ai::types::Content::Image {
                                data,
                                mime_type: processed_mime,
                            },
                        ],
                        ..Default::default()
                    })
                }
            };
        }
        if mime_type == "image/bmp" {
            return Ok(AgentToolResult {
                content: vec![pillar_ai::types::Content::text(
                    "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]",
                )],
                ..Default::default()
            });
        }
        return Ok(AgentToolResult {
            content: vec![
                pillar_ai::types::Content::text(format!("Read image file [{mime_type}]")),
                pillar_ai::types::Content::Image {
                    data: crate::harness::tools::image::encode_base64(&bytes),
                    mime_type: mime_type.to_owned(),
                },
            ],
            ..Default::default()
        });
    }

    let text_content = String::from_utf8_lossy(&bytes);
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    // Upstream: `offset ? Math.max(0, offset - 1) : 0` (1-indexed input).
    // An extreme f64 saturates into `usize::MAX`; the bounds check below runs
    // before the 1-indexed display is derived, so a saturated offset reports
    // "beyond end of file" instead of overflowing on `+ 1`.
    let start_line = match input.offset {
        Some(offset) if offset != 0.0 => (offset.max(1.0) - 1.0) as usize,
        _ => 0,
    };
    if start_line >= all_lines.len() {
        return Err(tool_error(format!(
            "Offset {} is beyond end of file ({} lines total)",
            input.offset.map(format_float).unwrap_or_default(),
            all_lines.len()
        )));
    }
    let start_line_display = start_line.saturating_add(1);

    let (selected_content, user_limited_lines) = match input.limit {
        Some(limit) => {
            // `limit.max(0.0) as usize` saturates for huge inputs; upstream's
            // numbers cannot overflow, so the sum must clamp rather than wrap.
            let end_line = start_line
                .saturating_add(limit.max(0.0) as usize)
                .min(all_lines.len());
            (
                all_lines[start_line..end_line].join("\n"),
                Some(end_line - start_line),
            )
        }
        None => (all_lines[start_line..].join("\n"), None),
    };

    let truncation = truncate_head(
        &selected_content,
        TruncationOptions {
            max_lines: None,
            max_bytes: None,
        },
    );
    let mut details = ReadToolDetails { truncation: None };
    let output_text: String;
    if truncation.first_line_exceeds_limit {
        let first_line_size = format_size(all_lines[start_line].len());
        output_text = format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES),
            input.path
        );
        details = ReadToolDetails {
            truncation: Some(truncation),
        };
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        let mut text = truncation.content.clone();
        if truncation.truncated_by == Some(crate::harness::utils::truncate::TruncatedBy::Lines) {
            text.push_str(&format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            ));
        } else {
            text.push_str(&format!(
                "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            ));
        }
        output_text = text;
        details = ReadToolDetails {
            truncation: Some(truncation),
        };
    } else if let Some(user_limited_lines) = user_limited_lines {
        if start_line + user_limited_lines < all_lines.len() {
            let remaining = all_lines.len() - (start_line + user_limited_lines);
            let next_offset = start_line + user_limited_lines + 1;
            output_text = format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            );
        } else {
            output_text = truncation.content;
        }
    } else {
        output_text = truncation.content;
    }

    Ok(AgentToolResult {
        content: vec![pillar_ai::types::Content::text(output_text)],
        details: details.to_value(),
        ..Default::default()
    })
}

/// Upstream prints numbers via JS `String(n)`; keep integral floats integral.
///
/// divergence: a value beyond `u64` cannot ride the integer path (the cast
/// saturates to a different number, so an extreme offset reported a bogus line
/// number), so it prints through `Display` instead. Upstream's `String(n)`
/// switches to exponential notation past 1e21; the port always prints the
/// decimal expansion.
fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() <= u64::MAX as f64 {
        format!("{}", value as u64)
    } else {
        format!("{value}")
    }
}

/// Upstream `ReadToolOptions`.
#[derive(Default, Clone)]
pub struct ReadToolOptions {
    /// Whether an injected image processor should resize images. Default:
    /// true.
    pub auto_resize_images: Option<bool>,
    /// Optional image conversion/resizing implementation.
    pub image_processor: Option<ReadImageProcessor>,
}

/// Upstream `createReadTool()`: the wire-level tool definition.
pub fn create_read_tool() -> crate::types::AgentTool {
    crate::harness::tools::write::wire_tool(
        "read",
        "read",
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "number", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "number", "description": "Maximum number of lines to read"}
            },
            "required": ["path"]
        }),
    )
}
