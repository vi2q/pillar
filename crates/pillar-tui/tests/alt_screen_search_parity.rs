//! Parity tests for tui alt-screen-search.ts (pi v0.84.3): corpus
//! building with position mapping, whitespace collapsing, case-insensitive
//! matching, segment coalescing, and match keys.

use pillar_tui::alt_screen_search::{
    find_alt_screen_search_matches, get_alt_screen_search_match_key, normalize_query,
    strip_terminal_sequences,
};

#[test]
fn terminal_sequences_stripped() {
    assert_eq!(strip_terminal_sequences("\u{1b}[31mred\u{1b}[0m"), "red");
    assert_eq!(strip_terminal_sequences("\u{1b}]0;title\u{7}x"), "x");
    assert_eq!(strip_terminal_sequences("\u{1b}]0;title\u{1b}\\x"), "x");
    assert_eq!(strip_terminal_sequences("plain"), "plain");
    // Cursor movement and other 2-char escapes.
    assert_eq!(strip_terminal_sequences("\u{1b}Krest"), "rest");
}

#[test]
fn query_normalization_collapses_whitespace() {
    assert_eq!(normalize_query("  hello   world  "), "hello world");
    assert_eq!(normalize_query("single"), "single");
    assert_eq!(normalize_query("   "), "");
}

#[test]
fn find_matches_single_line_case_insensitive() {
    let lines = vec!["Hello World"];
    let matches = find_alt_screen_search_matches(&lines, "WORLD");
    assert_eq!(matches.len(), 1);
    let segments = &matches[0].segments;
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].row, 0);
    assert_eq!(segments[0].start_col, 6);
    assert_eq!(segments[0].end_col, 11);
}

#[test]
fn whitespace_collapse_spans_line_breaks() {
    // "foo" and "bar" are separated by a line break that acts like one
    // space, so "foo bar" matches across the two lines.
    let lines = vec!["foo", "bar"];
    let matches = find_alt_screen_search_matches(&lines, "foo bar");
    assert_eq!(matches.len(), 1);
    let segments = &matches[0].segments;
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].row, 0);
    assert_eq!(segments[0].start_col, 0);
    assert_eq!(segments[0].end_col, 3);
    assert_eq!(segments[1].row, 1);
    assert_eq!(segments[1].start_col, 0);
    assert_eq!(segments[1].end_col, 3);
}

#[test]
fn multiple_matches_found_in_order() {
    let lines = vec!["abc abc abc"];
    let matches = find_alt_screen_search_matches(&lines, "abc");
    assert_eq!(matches.len(), 3);
    for (index, matched) in matches.iter().enumerate() {
        let start = (index * 4) as u64;
        assert_eq!(matched.segments[0].start_col as u64, start);
    }
}

#[test]
fn no_matches_on_empty_query_or_miss() {
    let lines = vec!["hello"];
    assert!(find_alt_screen_search_matches(&lines, "").is_empty());
    assert!(find_alt_screen_search_matches(&lines, "   ").is_empty());
    assert!(find_alt_screen_search_matches(&lines, "world").is_empty());
    assert!(find_alt_screen_search_matches(&[], "hello").is_empty());
}

#[test]
fn terminal_sequences_do_not_break_matching() {
    let lines = vec!["\u{1b}[32mhello\u{1b}[0m world"];
    let matches = find_alt_screen_search_matches(&lines, "hello world");
    assert_eq!(matches.len(), 1);
    // "hello" spans cols 0..5 (sequences stripped).
    assert_eq!(matches[0].segments[0].end_col, 5);
}

#[test]
fn match_key_uses_first_and_last_segments() {
    let lines = vec!["foo", "bar"];
    let matches = find_alt_screen_search_matches(&lines, "foo bar");
    assert_eq!(get_alt_screen_search_match_key(&matches[0]), "0:0:1:3");
    // Empty matches produce an empty key.
    let empty = pillar_tui::alt_screen_search::AltScreenSearchMatch { segments: vec![] };
    assert_eq!(get_alt_screen_search_match_key(&empty), "");
}

#[test]
fn repeated_matches_do_not_overlap() {
    let lines = vec!["aaaa"];
    let matches = find_alt_screen_search_matches(&lines, "aa");
    // Non-overlapping: "aa" at 0-2 and "aa" at 2-4.
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].segments[0].start_col, 0);
    assert_eq!(matches[1].segments[0].start_col, 2);
}
