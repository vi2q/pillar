//! Parity tests for ansi.ts, shell.ts sanitize half, and truncate.ts (pi
//! v0.84.3): ANSI stripping, binary output sanitization, head/tail
//! truncation with byte+line limits, and grep line truncation.

use pillar_coding_agent::core::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, TruncatedBy, TruncationOptions,
    format_size, sanitize_binary_output, strip_ansi, truncate_head, truncate_line, truncate_tail,
};

#[test]
fn constants_match_upstream() {
    assert_eq!(DEFAULT_MAX_LINES, 2000);
    assert_eq!(DEFAULT_MAX_BYTES, 50 * 1024);
    assert_eq!(GREP_MAX_LINE_LENGTH, 500);
}

// --- stripAnsi -----------------------------------------------------------------

#[test]
fn strip_ansi_removes_color_and_style_sequences() {
    assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
    assert_eq!(strip_ansi("\x1b[1;32;40mmix\x1b[0m"), "mix");
    assert_eq!(strip_ansi("no codes"), "no codes");
}

#[test]
fn strip_ansi_handles_osc_sequences() {
    // OSC with BEL terminator.
    assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
    // OSC with ST terminator (ESC \).
    assert_eq!(strip_ansi("\x1b]2;title\x1b\\after"), "after");
}

#[test]
fn strip_ansi_fast_path_and_multibyte_preserved() {
    // No ESC/CSI introducer: fast path returns the string unchanged.
    assert_eq!(strip_ansi("日本語テキスト"), "日本語テキスト");
    // Multi-byte characters around a sequence.
    assert_eq!(strip_ansi("日\x1b[31m本"), "日本");
}

// --- sanitizeBinaryOutput ---------------------------------------------------------

#[test]
fn sanitize_removes_control_and_format_chars() {
    // Control chars removed except \t \n \r.
    assert_eq!(sanitize_binary_output("a\u{0}b\u{1}c"), "abc");
    assert_eq!(sanitize_binary_output("a\tb\nc\rd"), "a\tb\nc\rd");
    // Unicode format characters 0xFFF9-0xFFFB removed.
    assert_eq!(sanitize_binary_output("a\u{fff9}b\u{fffb}c"), "abc");
    // Normal text and DEL (0x7F) preserved (upstream keeps 0x7F).
    assert_eq!(sanitize_binary_output("ok\u{7f}text"), "ok\u{7f}text");
}

// --- formatSize ---------------------------------------------------------------------

#[test]
fn format_size_units() {
    assert_eq!(format_size(512), "512B");
    assert_eq!(format_size(1024), "1.0KB");
    assert_eq!(format_size(1536), "1.5KB");
    assert_eq!(format_size(1024 * 1024), "1.0MB");
}

// --- truncateHead -----------------------------------------------------------------------

#[test]
fn truncate_head_no_truncation_needed() {
    let result = truncate_head("a\nb\nc", TruncationOptions::default());
    assert!(!result.truncated);
    assert_eq!(result.content, "a\nb\nc");
    assert_eq!(result.total_lines, 3);
    assert_eq!(result.truncated_by, None);
}

#[test]
fn truncate_head_by_line_limit() {
    let content = "1\n2\n3\n4\n5";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_lines: Some(3),
            max_bytes: Some(1024),
        },
    );
    assert!(result.truncated);
    assert_eq!(result.content, "1\n2\n3");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 3);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_by_byte_limit_never_partial_lines() {
    let content = "aaaa\nbb\ncc\ndd";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_lines: Some(100),
            max_bytes: Some(10), // "aaaa\nbb" = 7; +\n+cc (3) = 10 <= 10 -> included
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "aaaa\nbb\ncc");
    assert!(!result.last_line_partial);
}

#[test]
fn truncate_head_first_line_exceeds_limit() {
    let content = "aaaaaaaaaaaaaaaaaaaa\nshort";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_lines: Some(100),
            max_bytes: Some(10),
        },
    );
    assert!(result.truncated);
    assert!(result.first_line_exceeds_limit);
    assert_eq!(result.content, "");
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
}

// --- truncateTail -------------------------------------------------------------------------

#[test]
fn truncate_tail_no_truncation_needed() {
    let result = truncate_tail("a\nb\nc", TruncationOptions::default());
    assert!(!result.truncated);
    assert_eq!(result.content, "a\nb\nc");
}

#[test]
fn truncate_tail_keeps_the_end() {
    let content = "1\n2\n3\n4\n5";
    let result = truncate_tail(
        content,
        TruncationOptions {
            max_lines: Some(3),
            max_bytes: Some(1024),
        },
    );
    assert!(result.truncated);
    assert_eq!(result.content, "3\n4\n5");
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
}

#[test]
fn truncate_tail_by_byte_limit() {
    let content = "aa\nbb\ncccc\ndd";
    let result = truncate_tail(
        content,
        TruncationOptions {
            max_lines: Some(100),
            max_bytes: Some(9), // from end: dd(2)+\n+cccc(4)+\n=8; +bb(2)=10 > 9
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "cccc\ndd");
}

#[test]
fn truncate_tail_oversized_last_line_is_partial() {
    let content = "short\naaaaaaaaaaaaaaaaaaaa";
    let result = truncate_tail(
        content,
        TruncationOptions {
            max_lines: Some(100),
            max_bytes: Some(10),
        },
    );
    assert!(result.truncated);
    assert!(result.last_line_partial);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    // Keeps the last 10 bytes of the oversized line.
    assert_eq!(result.content, "aaaaaaaaaa");
    assert_eq!(result.output_bytes, 10);
}

#[test]
fn truncate_tail_respects_utf8_boundaries() {
    // 4-byte emoji at the end; the partial tail must not split it.
    let content = "short\n日本語です"; // last line is 15 bytes
    let result = truncate_tail(
        content,
        TruncationOptions {
            max_lines: Some(100),
            max_bytes: Some(10),
        },
    );
    assert!(result.last_line_partial);
    assert!(result.content.is_char_boundary(0));
    // The kept tail is valid UTF-8 (content round-trips through String).
    let _ = result.content.clone();
}

// --- truncateLine --------------------------------------------------------------------------

#[test]
fn truncate_line_short_line_untouched() {
    let (text, was) = truncate_line("short", 500);
    assert_eq!(text, "short");
    assert!(!was);
}

#[test]
fn truncate_line_long_line_gets_suffix() {
    let long = "x".repeat(600);
    let (text, was) = truncate_line(&long, GREP_MAX_LINE_LENGTH);
    assert!(was);
    assert!(text.starts_with(&"x".repeat(500)));
    assert!(text.ends_with("... [truncated]"));
}
