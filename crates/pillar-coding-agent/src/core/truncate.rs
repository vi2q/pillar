//! Port of packages/coding-agent/src/utils/ansi.ts, utils/shell.ts
//! (sanitize half), and core/tools/truncate.ts (pi v0.84.3): ANSI stripping,
//! binary-output sanitization, and the shared head/tail truncation
//! utilities used by tool outputs.

// ============================================================================
// ansi.ts — stripAnsi
// ============================================================================

/// Strip ANSI escape sequences (upstream `stripAnsi`): OSC sequences
/// (`ESC ] ... ST`) and CSI/related sequences (`ESC`/C1 introducer,
/// optional intermediates and params, final byte).
///
/// divergence: the port uses a hand-written scanner equivalent to the
/// upstream `ansiRegex` pattern (OSC non-greedy to BEL/ESC\\/0x9C; CSI with
/// `\d{1,4}(;\d{0,4})*` params and a final byte from the upstream class).
pub fn strip_ansi(value: &str) -> String {
    // Fast path: ANSI codes require ESC (0x1B) or CSI (0x9B) introducer.
    if !value.contains('\u{1b}') && !value.contains('\u{9b}') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b || b == 0x9b {
            if let Some(end) = find_sequence_end(bytes, i) {
                i = end;
                continue;
            }
        }
        // Copy the full UTF-8 character starting at i.
        let ch_len = utf8_len(b);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&value[i..end]);
        i = end;
    }
    out
}

/// Find the index just past the escape sequence starting at `start`, or None
/// when unterminated.
fn find_sequence_end(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes[start] {
        // OSC: ESC ] ... (BEL | ESC \ | 0x9C)
        0x1b if bytes.get(start + 1) == Some(&b']') => {
            let mut j = start + 2;
            while j < bytes.len() {
                match bytes[j] {
                    0x07 => return Some(j + 1), // BEL
                    0x1b if bytes.get(j + 1) == Some(&b'\\') => return Some(j + 2),
                    0x9c => return Some(j + 1),
                    _ => j += 1,
                }
            }
            None
        }
        // CSI and related: introducer, optional [[\]()#;?]* intermediates,
        // optional numeric params separated by ; or :, final byte.
        0x1b | 0x9b => {
            let mut j = start + 1;
            while j < bytes.len()
                && matches!(bytes[j], b'[' | b']' | b'(' | b')' | b'#' | b';' | b'?')
            {
                j += 1;
            }
            // Params: groups of up to 4 digits separated by ; or :
            loop {
                let digits = bytes[j..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count()
                    .min(4);
                j += digits;
                if j < bytes.len() && (bytes[j] == b';' || bytes[j] == b':') {
                    // Optional up-to-4-digit sub-parameter after the separator.
                    j += 1;
                    let sub = bytes[j..]
                        .iter()
                        .take_while(|b| b.is_ascii_digit())
                        .count()
                        .min(4);
                    j += sub;
                    if j < bytes.len() && (bytes[j] == b';' || bytes[j] == b':') {
                        continue;
                    }
                }
                break;
            }
            // Final byte from [\dA-PR-TZcf-nq-uy=><~]
            let final_byte = bytes.get(j).copied()?;
            let is_final = final_byte.is_ascii_digit()
                || matches!(final_byte,
                    b'A'..=b'P' | b'R'..=b'T' | b'Z' | b'a'..=b'c' | b'f'..=b'n' | b'q' | b'u' | b'y'
                    | b'=' | b'>' | b'<' | b'~')
                || final_byte == b'`';
            if is_final { Some(j + 1) } else { None }
        }
        _ => None,
    }
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

// ============================================================================
// shell.ts — sanitizeBinaryOutput
// ============================================================================

/// Remove characters that break terminal width calculations (upstream
/// `sanitizeBinaryOutput`): control chars except \t \n \r, and Unicode
/// format characters 0xFFF9-0xFFFB. Iterates by code points.
pub fn sanitize_binary_output(input: &str) -> String {
    input
        .chars()
        .filter(|&c| {
            let code = c as u32;
            // Allow tab, newline, carriage return
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            // Filter out control characters
            if code <= 0x1f {
                return false;
            }
            // Filter out Unicode format characters
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            // 0x7F (DEL) is a control character too; upstream keeps it, so
            // the port keeps it as well.
            true
        })
        .collect()
}

// ============================================================================
// truncate.ts
// ============================================================================

/// Default max lines for truncation (upstream `DEFAULT_MAX_LINES`).
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default max bytes for truncation, 50KB (upstream `DEFAULT_MAX_BYTES`).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max chars per grep match line (upstream `GREP_MAX_LINE_LENGTH`).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit was hit (upstream `truncatedBy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// Truncation result (upstream `TruncationResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Only for the tail truncation edge case.
    pub last_line_partial: bool,
    /// For head truncation when the first line exceeds the byte limit.
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// Truncation options (upstream `TruncationOptions`).
#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as human-readable size (upstream `formatSize`).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Truncate content from the head (keep first N lines/bytes). Never returns
/// partial lines (upstream `truncateHead`).
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    truncate_head_with_totals(
        content,
        split_lines_for_counting(content).len(),
        content.len(),
        options,
    )
}

/// [`truncate_head`] with the totals supplied by the caller.
///
/// `content` only has to be the head of the text being truncated — long enough
/// that the cut lands inside it (`max_lines` lines and `max_bytes` bytes, or
/// the end of the text) — while `total_lines`/`total_bytes` describe the whole
/// text. The streaming read uses this to keep its buffer bounded without
/// changing any reported total.
pub fn truncate_head_with_totals(
    content: &str,
    total_lines: usize,
    total_bytes: usize,
    options: TruncationOptions,
) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let lines = split_lines_for_counting(content);

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
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
        };
    }

    // First line alone exceeding the byte limit -> empty content.
    let first_line_bytes = lines.first().map_or(0, |line| line.len());
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

    let mut output_lines: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines.push(line);
        output_bytes_count += line_bytes;
    }

    if output_lines.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines.join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        output_bytes: final_output_bytes,
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit counted from the end,
/// respecting UTF-8 boundaries (upstream `truncateStringToBytesFromEnd`).
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    let buf = s.as_bytes();
    if buf.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = buf.len() - max_bytes;
    // Skip continuation bytes to a valid UTF-8 boundary.
    while start < buf.len() && (buf[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&buf[start..]).to_string()
}

/// Truncate content from the tail (keep last N lines/bytes). May return a
/// partial first line when the last line exceeds the byte limit (upstream
/// `truncateTail`).
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
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
        };
    }

    let mut collected: Vec<String> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for line in lines.iter().rev().take(max_lines) {
        let line_bytes = line.len() + if !collected.is_empty() { 1 } else { 0 };
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if collected.is_empty() {
                // Edge case: take the end of the oversized line (partial).
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_bytes_count = truncated_line.len();
                collected.insert(0, truncated_line);
                last_line_partial = true;
            }
            break;
        }
        collected.insert(0, (*line).to_string());
        output_bytes_count += line_bytes;
    }

    if collected.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = collected.join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        output_bytes: final_output_bytes,
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: collected.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a single line to max chars with a `[truncated]` suffix (upstream
/// `truncateLine`, used for grep match lines).
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let kept: String = line.chars().take(max_chars).collect();
    (format!("{kept}... [truncated]"), true)
}
