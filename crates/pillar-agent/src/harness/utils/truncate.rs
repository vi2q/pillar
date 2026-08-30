//! Port of packages/agent/src/harness/utils/truncate.ts (pi v0.84.3).
//!
//! Shared truncation utilities for tool outputs. Truncation is based on
//! two independent limits — whichever is hit first wins:
//! - Line limit (default: 2000 lines)
//! - Byte limit (default: 50KB)
//!
//! Never returns partial lines (except bash tail truncation edge case).
//!
//! divergence: JS strings are UTF-16; Rust strings are UTF-8. The upstream
//! code tracks UTF-8 byte lengths via Buffer and repairs unpaired
//! surrogates; Rust `&str` is always valid UTF-8, so surrogate-repair
//! branches collapse. Upstream's `truncateStringToBytesFromEnd` operates on
//! UTF-16 code units and converts back; the port walks UTF-8 bytes
//! directly, which yields the same visible output for valid input.

/// Maximum characters per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Default line limit.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default byte limit (50KB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Which limit caused truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// Result of a truncation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Which limit was hit, or `None` when not truncated.
    pub truncated_by: Option<TruncatedBy>,
    /// Total number of lines in the original content.
    pub total_lines: usize,
    /// Total number of bytes in the original content.
    pub total_bytes: usize,
    /// Number of complete lines in the truncated output.
    pub output_lines: usize,
    /// Number of bytes in the truncated output.
    pub output_bytes: usize,
    /// Whether the last line was partially truncated (tail edge case only).
    pub last_line_partial: bool,
    /// Whether the first line exceeded the byte limit (head truncation).
    pub first_line_exceeds_limit: bool,
    /// The max lines limit that was applied.
    pub max_lines: usize,
    /// The max bytes limit that was applied.
    pub max_bytes: usize,
}

/// Truncation options; `None` fields fall back to the defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    /// Maximum number of lines (default: 2000).
    pub max_lines: Option<usize>,
    /// Maximum number of bytes (default: 50KB).
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    fn resolve(&self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// UTF-8 byte length of a string (upstream `utf8ByteLength`).
pub fn utf8_byte_length(content: &str) -> usize {
    content.len()
}

/// Same as `splitLinesForCounting` upstream: split on `\n`, dropping a
/// single trailing empty element when the content ends with `\n`.
pub fn lines_for_count(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as human-readable size.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn no_truncation(
    content: &str,
    total_lines: usize,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    TruncationResult {
        content: content.to_owned(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate content from the head (keep first N lines/bytes).
/// Suitable for file reads where you want to see the beginning.
///
/// Never returns partial lines. If the first line exceeds the byte limit,
/// returns empty content with `first_line_exceeds_limit = true`.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();

    let total_bytes = utf8_byte_length(content);
    let lines = lines_for_count(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return no_truncation(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    // Check if the first line alone exceeds the byte limit.
    let first_line_bytes = utf8_byte_length(lines[0]);
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    // Collect complete lines that fit.
    let mut output_lines: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, &line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = utf8_byte_length(line) + if i > 0 { 1 } else { 0 }; // +1 for newline
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines.push(line);
        output_bytes_count += line_bytes;
    }

    // If we exited due to the line limit.
    if output_lines.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines.join("\n");
    let final_output_bytes = utf8_byte_length(&output_content);

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit (from the end), aligning to
/// UTF-8 character boundaries. Upstream slices UTF-16 code units and decodes
/// from the byte tail; the port slices the UTF-8 bytes and widens to a
/// char boundary, producing identical visible output.
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let bytes = s.as_bytes();
    if bytes.len() <= max_bytes {
        return s.to_owned();
    }
    let mut start = bytes.len() - max_bytes;
    // Skip continuation bytes so we start on a character boundary (upstream
    // skips UTF-8 continuation bytes the same way after decoding).
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Truncate content from the tail (keep last N lines/bytes).
/// Suitable for bash output where you want to see the end (errors, final
/// results).
///
/// May return a partial first line if the last line of the original content
/// exceeds the byte limit.
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolve();

    let total_bytes = utf8_byte_length(content);
    let lines = lines_for_count(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return no_truncation(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    // Work backwards from the end.
    let mut output_lines: Vec<String> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for i in (0..lines.len()).rev() {
        if output_lines.len() >= max_lines {
            break;
        }
        let line = lines[i];
        let line_bytes = utf8_byte_length(line) + if !output_lines.is_empty() { 1 } else { 0 };

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: if we haven't added ANY lines yet and this line
            // exceeds max_bytes, take the end of the line (partial).
            if output_lines.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_bytes_count = utf8_byte_length(&truncated_line);
                output_lines.insert(0, truncated_line);
                last_line_partial = true;
            }
            break;
        }

        output_lines.insert(0, line.to_owned());
        output_bytes_count += line_bytes;
    }

    // If we exited due to the line limit.
    if output_lines.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines.join("\n");
    let final_output_bytes = utf8_byte_length(&output_content);

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
        output_bytes: final_output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a single line to max characters, adding `[truncated]` suffix.
/// Used for grep match lines.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_owned(), false);
    }
    let truncated: String = line.chars().take(max_chars).collect();
    (format!("{truncated}... [truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_utf8_bytes() {
        let content = "aé🙂\nb";
        let result = truncate_head(
            content,
            TruncationOptions {
                max_bytes: Some(100),
                max_lines: Some(10),
            },
        );
        assert!(!result.truncated);
        assert_eq!(result.total_bytes, 9);
        assert_eq!(result.output_bytes, 9);
    }

    #[test]
    fn trailing_newline_is_not_an_extra_line() {
        let content = "line\nline\nline\n";
        let head = truncate_head(content, TruncationOptions::default());
        let tail = truncate_tail(content, TruncationOptions::default());
        assert!(!head.truncated);
        assert_eq!(head.total_lines, 3);
        assert_eq!(head.output_lines, 3);
        assert!(!tail.truncated);
        assert_eq!(tail.total_lines, 3);
    }
}
