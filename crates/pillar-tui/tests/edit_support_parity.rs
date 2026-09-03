//! Parity tests for tui undo-stack.ts, kill-ring.ts, and
//! word-navigation.ts (pi v0.84.3).

use pillar_tui::edit_support::{
    KillRing, UndoStack, find_word_backward, find_word_forward, segment_words,
};

// --- undo stack --------------------------------------------------------------------------

#[test]
fn undo_stack_push_pop_clear() {
    let mut stack: UndoStack<String> = UndoStack::new();
    assert!(stack.pop().is_none());
    stack.push(&"one".to_string());
    stack.push(&"two".to_string());
    assert_eq!(stack.len(), 2);
    assert_eq!(stack.pop().as_deref(), Some("two"));
    assert_eq!(stack.pop().as_deref(), Some("one"));
    assert_eq!(stack.pop(), None);
    stack.push(&"three".to_string());
    stack.clear();
    assert_eq!(stack.len(), 0);
    assert!(stack.pop().is_none());
}

#[test]
fn undo_stack_clone_on_push_detaches_snapshots() {
    let mut stack: UndoStack<Vec<i32>> = UndoStack::new();
    let mut state = vec![1, 2, 3];
    stack.push(&state);
    state.push(4);
    state[0] = 100;
    // The snapshot is a detached clone.
    assert_eq!(stack.pop(), Some(vec![1, 2, 3]));
}

// --- kill ring -----------------------------------------------------------------------------

#[test]
fn kill_ring_push_peek_and_rotate() {
    let mut ring = KillRing::new();
    assert_eq!(ring.peek(), None);
    ring.push("first", false, false);
    ring.push("second", false, false);
    assert_eq!(ring.len(), 2);
    assert_eq!(ring.peek(), Some("second"));

    // rotate moves the last entry to the front (yank-pop cycling).
    ring.rotate();
    assert_eq!(ring.peek(), Some("first"));
    ring.rotate();
    assert_eq!(ring.peek(), Some("second"));

    // Empty text is ignored.
    ring.push("", false, false);
    assert_eq!(ring.len(), 2);
}

#[test]
fn kill_ring_accumulation_prepend_vs_append() {
    let mut ring = KillRing::new();
    // Backward deletion (prepend): earlier text ends up after later kills.
    ring.push("world", false, false);
    ring.push("hello ", true, true);
    assert_eq!(ring.peek(), Some("hello world"));
    assert_eq!(ring.len(), 1);

    // Forward deletion (append).
    let mut ring = KillRing::new();
    ring.push("hello ", false, false);
    ring.push("world", false, true);
    assert_eq!(ring.peek(), Some("hello world"));

    // Accumulate with an empty ring just pushes.
    let mut ring = KillRing::new();
    ring.push("only", true, true);
    assert_eq!(ring.peek(), Some("only"));
}

// --- word segmentation -----------------------------------------------------------------------

#[test]
fn segmentation_word_punct_whitespace_runs() {
    let segments = segment_words("hello, world");
    let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["hello", ",", " ", "world"]);
    assert!(segments[0].word_like);
    assert!(!segments[1].word_like);
    assert!(!segments[2].word_like);
    assert!(segments[3].word_like);
}

// --- word backward ----------------------------------------------------------------------------

#[test]
fn word_backward_skips_trailing_whitespace_then_one_word() {
    // "hello world" with cursor at end → start of "world".
    assert_eq!(find_word_backward("hello world", 11, None), 6);
    // Trailing spaces are skipped too.
    assert_eq!(find_word_backward("hello world   ", 14, None), 6);
    // Cursor 0 stays at 0.
    assert_eq!(find_word_backward("hello", 0, None), 0);
}

#[test]
fn word_backward_stops_at_punctuation_run() {
    // Cursor after "!!!" skips the whole punctuation run.
    assert_eq!(find_word_backward("hi!!!", 5, None), 2);
    // Inside a word with punctuation: stops after the last punctuation
    // char (ASCII boundary preservation).
    assert_eq!(find_word_backward("foo.bar", 7, None), 4);
}

// --- word forward -------------------------------------------------------------------------------

#[test]
fn word_forward_skips_leading_whitespace_then_one_word() {
    assert_eq!(find_word_forward("hello world", 0, None), 5);
    assert_eq!(find_word_forward("   hello", 0, None), 8);
    // Cursor at end stays.
    assert_eq!(find_word_forward("hello", 5, None), 5);
}

#[test]
fn word_forward_stops_at_punctuation_run() {
    // "hi!!!" from 0: only ONE segment is skipped ("hi"), stopping before
    // the punctuation run (upstream moves one segment per invocation).
    assert_eq!(find_word_forward("hi!!!", 0, None), 2);
    // A second forward hop skips the punctuation run.
    assert_eq!(find_word_forward("hi!!!", 2, None), 5);
    // Inside a word with punctuation: stops at the punctuation.
    assert_eq!(find_word_forward("foo.bar", 0, None), 3);
}

// --- atomic segments ------------------------------------------------------------------------------

#[test]
fn atomic_segments_treated_as_single_units() {
    let is_atomic = |segment: &str| segment == "\u{fffc}"; // paste marker
    let text = "a \u{fffc} b";

    // Backward from just after the marker's trailing space: skips the
    // whitespace, then the atomic marker as one unit, landing before it.
    let after_marker_space = 4usize; // 'a',' ','marker',' '
    let backward = find_word_backward(text, after_marker_space, Some(&is_atomic));
    assert_eq!(backward, 2, "landed before the marker");

    // Backward from the very end lands before "b" first (one word per
    // invocation, matching upstream).
    let end = text.chars().count();
    assert_eq!(find_word_backward(text, end, Some(&is_atomic)), 4);

    // Forward hops, one segment each (matching upstream): "a", then the
    // whitespace, then the marker as one unit.
    assert_eq!(find_word_forward(text, 0, Some(&is_atomic)), 1);
    assert_eq!(
        find_word_forward(text, 1, Some(&is_atomic)),
        3,
        "whitespace skipped, marker as one unit"
    );
    assert_eq!(
        find_word_forward(text, 2, Some(&is_atomic)),
        3,
        "skipped the marker as one unit"
    );
}
