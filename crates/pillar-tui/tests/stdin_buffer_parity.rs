//! Parity tests for stdin-buffer (pi v0.84.3 stdin-buffer.ts).

use std::time::{Duration, Instant};

use pillar_tui::stdin_buffer::{
    SequenceStatus, StdinBuffer, extract_complete_sequences, is_complete_csi_sequence,
    is_complete_sequence,
};

fn now() -> Instant {
    Instant::now()
}

// --- isCompleteSequence -------------------------------------------------------------------------

#[test]
fn plain_text_is_not_escape() {
    assert_eq!(is_complete_sequence("a"), SequenceStatus::NotEscape);
    assert_eq!(is_complete_sequence("hello"), SequenceStatus::NotEscape);
}

#[test]
fn lone_esc_is_incomplete() {
    assert_eq!(is_complete_sequence("\u{1b}"), SequenceStatus::Incomplete);
}

#[test]
fn csi_arrow_key_is_complete() {
    assert_eq!(is_complete_sequence("\u{1b}[A"), SequenceStatus::Complete);
    assert_eq!(
        is_complete_sequence("\u{1b}[1;5A"),
        SequenceStatus::Complete
    );
}

#[test]
fn csi_partial_is_incomplete() {
    assert_eq!(is_complete_sequence("\u{1b}["), SequenceStatus::Incomplete);
    assert_eq!(
        is_complete_sequence("\u{1b}[1;"),
        SequenceStatus::Incomplete
    );
}

#[test]
fn sgr_mouse_sequence() {
    assert_eq!(
        is_complete_sequence("\u{1b}[<35;20;5m"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}[<0;10;10M"),
        SequenceStatus::Complete
    );
    // Wrong structure ending with m → incomplete.
    assert_eq!(
        is_complete_sequence("\u{1b}[<35;20m"),
        SequenceStatus::Incomplete
    );
}

#[test]
fn old_style_mouse_needs_six_bytes() {
    assert_eq!(is_complete_sequence("\u{1b}[M"), SequenceStatus::Incomplete);
    assert_eq!(
        is_complete_sequence("\u{1b}[Mabc"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}[Mab"),
        SequenceStatus::Incomplete
    );
    // Two bytes of the three-byte payload is still incomplete.
    assert_eq!(
        is_complete_sequence("\u{1b}[Ma"),
        SequenceStatus::Incomplete
    );
}

#[test]
fn osc_terminators() {
    assert_eq!(
        is_complete_sequence("\u{1b}]8;;https://x\u{1b}\\"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}]0;title\u{7}"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}]8;;url"),
        SequenceStatus::Incomplete
    );
}

#[test]
fn dcs_and_apc_terminate_with_st() {
    assert_eq!(
        is_complete_sequence("\u{1b}P>|xterm\u{1b}\\"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}P>|xterm"),
        SequenceStatus::Incomplete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}_Gf=100;abc\u{1b}\\"),
        SequenceStatus::Complete
    );
    assert_eq!(
        is_complete_sequence("\u{1b}_Gf=100"),
        SequenceStatus::Incomplete
    );
}

#[test]
fn ss3_and_meta() {
    assert_eq!(is_complete_sequence("\u{1b}OA"), SequenceStatus::Complete);
    assert_eq!(is_complete_sequence("\u{1b}O"), SequenceStatus::Incomplete);
    // ESC + single char = meta key.
    assert_eq!(is_complete_sequence("\u{1b}a"), SequenceStatus::Complete);
}

#[test]
fn csi_completeness_direct() {
    assert_eq!(
        is_complete_csi_sequence("\u{1b}[A"),
        SequenceStatus::Complete
    );
    // Final byte out of range.
    assert_eq!(
        is_complete_csi_sequence("\u{1b}[1"),
        SequenceStatus::Incomplete
    );
}

// --- extractCompleteSequences -----------------------------------------------------------------------

#[test]
fn extract_single_sequences() {
    let (seqs, remainder) = extract_complete_sequences("abc");
    assert_eq!(seqs, vec!["a", "b", "c"]);
    assert_eq!(remainder, "");
}

#[test]
fn extract_mixed_text_and_escapes() {
    let (seqs, remainder) = extract_complete_sequences("a\u{1b}[Ab\u{1b}[B");
    assert_eq!(seqs, vec!["a", "\u{1b}[A", "b", "\u{1b}[B"]);
    assert_eq!(remainder, "");
}

#[test]
fn extract_keeps_partial_escape_as_remainder() {
    let (seqs, remainder) = extract_complete_sequences("a\u{1b}[1;5");
    assert_eq!(seqs, vec!["a"]);
    assert_eq!(remainder, "\u{1b}[1;5");
}

#[test]
fn extract_wezterm_double_escape_keypress_then_kitty_release() {
    // '\x1b\x1b[27;...u' → first ESC alone, then the CSI-u sequence.
    let (seqs, remainder) = extract_complete_sequences("\u{1b}\u{1b}[27;1;27u");
    assert_eq!(seqs, vec!["\u{1b}", "\u{1b}[27;1;27u"]);
    assert_eq!(remainder, "");
}

// --- StdinBuffer --------------------------------------------------------------------------------------

#[test]
fn buffer_emits_immediate_sequences() {
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("ab", now());
    assert_eq!(outcome.data, vec!["a", "b"]);
    assert!(outcome.paste.is_none());
    assert!(outcome.flush_deadline.is_none());
}

#[test]
fn buffer_holds_partial_sequence_for_flush() {
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("\u{1b}[1;5", now());
    assert!(outcome.data.is_empty());
    assert!(outcome.flush_deadline.is_some());
    // Flush emits the raw remainder.
    assert_eq!(buf.flush(), vec!["\u{1b}[1;5".to_string()]);
}

#[test]
fn buffer_lone_esc_uses_escape_timeout() {
    let mut buf = StdinBuffer::new();
    let start = now();
    let outcome = buf.process_with_clock("\u{1b}", start);
    assert!(outcome.data.is_empty());
    let deadline = outcome.flush_deadline.unwrap();
    assert_eq!(deadline.duration_since(start), Duration::from_millis(10));
}

#[test]
fn buffer_partial_csi_uses_sequence_timeout() {
    let mut buf = StdinBuffer::new();
    let start = now();
    let outcome = buf.process_with_clock("\u{1b}[1;", start);
    let deadline = outcome.flush_deadline.unwrap();
    assert_eq!(deadline.duration_since(start), Duration::from_millis(50));
}

#[test]
fn buffer_completes_sequence_across_chunks() {
    let mut buf = StdinBuffer::new();
    let first = buf.process_with_clock("\u{1b}[", now());
    assert!(first.data.is_empty());
    let second = buf.process_with_clock("A", now());
    assert_eq!(second.data, vec!["\u{1b}[A".to_string()]);
    assert!(second.flush_deadline.is_none());
}

#[test]
fn buffer_bracketed_paste_emits_paste_event() {
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("\u{1b}[200~pasted text\u{1b}[201~", now());
    assert_eq!(outcome.paste.as_deref(), Some("pasted text"));
    assert!(outcome.data.is_empty());
    // Trailing input after the paste is processed.
    let outcome2 = buf.process_with_clock("x", now());
    assert_eq!(outcome2.data, vec!["x".to_string()]);
}

#[test]
fn buffer_paste_across_chunks() {
    let mut buf = StdinBuffer::new();
    let first = buf.process_with_clock("\u{1b}[200~par", now());
    assert!(first.paste.is_none());
    assert!(first.data.is_empty());
    let second = buf.process_with_clock("tial\u{1b}[201~after", now());
    assert_eq!(second.paste.as_deref(), Some("partial"));
    assert_eq!(second.data, vec!["a", "f", "t", "e", "r"]);
}

#[test]
fn buffer_input_before_paste_marker_is_processed() {
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("ab\u{1b}[200~content\u{1b}[201~", now());
    assert_eq!(outcome.data, vec!["a", "b"]);
    assert_eq!(outcome.paste.as_deref(), Some("content"));
}

#[test]
fn buffer_kitty_printable_dedup() {
    // A Kitty CSI-u printable sequence followed by the raw character:
    // the raw char is swallowed (pending codepoint dedup).
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("\u{1b}[97u", now());
    assert_eq!(outcome.data, vec!["\u{1b}[97u".to_string()]);
    let outcome2 = buf.process_with_clock("a", now());
    // The raw 'a' matches the pending codepoint → suppressed.
    assert!(outcome2.data.is_empty(), "{:?}", outcome2.data);
}

#[test]
fn buffer_kitty_printable_dedup_resets_on_other_input() {
    let mut buf = StdinBuffer::new();
    let _ = buf.process_with_clock("\u{1b}[97u", now());
    // A different char arrives → emitted normally, pending cleared.
    let outcome = buf.process_with_clock("b", now());
    assert_eq!(outcome.data, vec!["b".to_string()]);
    // Now 'a' arrives raw → emitted normally (pending was cleared).
    let outcome2 = buf.process_with_clock("a", now());
    assert_eq!(outcome2.data, vec!["a".to_string()]);
}

#[test]
fn buffer_clear_and_get() {
    let mut buf = StdinBuffer::new();
    let _ = buf.process_with_clock("\u{1b}[1;", now());
    assert_eq!(buf.get_buffer(), "\u{1b}[1;");
    buf.clear();
    assert_eq!(buf.get_buffer(), "");
    assert!(buf.flush().is_empty());
}

#[test]
fn buffer_empty_input_with_empty_buffer_emits_empty_sequence() {
    let mut buf = StdinBuffer::new();
    let outcome = buf.process_with_clock("", now());
    assert_eq!(outcome.data, vec![String::new()]);
}
