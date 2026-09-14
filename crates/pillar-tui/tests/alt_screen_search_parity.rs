//! Parity tests for tui alt-screen-search.ts (pi v0.84.3): corpus
//! building with position mapping, whitespace collapsing, case-insensitive
//! matching, segment coalescing, and match keys.

use pillar_tui::alt_screen_search::{
    AltScreenSearchComponent, find_alt_screen_search_matches, get_alt_screen_search_match_key,
    normalize_query, strip_terminal_sequences,
};
use pillar_tui::text_utils::strip_terminal_sequences as strip_sequences;
use pillar_tui::tui::{Component, Focusable};

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

// --- AltScreenSearchComponent (upstream the overlay component) -----------------------------------

#[test]
fn search_component_renders_label_status_and_input() {
    let mut component = AltScreenSearchComponent::new();
    let lines = component.render(24);
    assert_eq!(lines.len(), 2, "status line + input line");
    // The status line is reversed and pads to the full width.
    assert!(lines[0].starts_with("\u{1b}[7m"), "{:?}", lines[0]);
    assert!(lines[0].ends_with("\u{1b}[27m"), "{:?}", lines[0]);
    let plain = strip_sequences(&lines[0]);
    assert_eq!(plain, format!(" Find transcript{}", " ".repeat(24 - 16)));
    // The input line carries the `> ` prompt.
    assert!(lines[1].starts_with("> "), "{:?}", lines[1]);
}

#[test]
fn search_component_status_shows_the_selected_match() {
    let mut component = AltScreenSearchComponent::new();
    component.handle_input("needle");
    assert_eq!(component.query(), "needle");

    // No results yet.
    let plain = strip_sequences(&component.render(30)[0]);
    assert!(plain.contains("No matches"), "{plain:?}");

    // Second of three matches.
    component.set_result(1, 3);
    assert_eq!(component.result_index(), 1);
    assert_eq!(component.result_count(), 3);
    let plain = strip_sequences(&component.render(30)[0]);
    assert!(plain.contains("2/3"), "{plain:?}");

    // Editing keys reach the input (upstream Input keybindings).
    component.handle_input("\u{7f}");
    assert_eq!(component.query(), "needl");
    component.handle_input("e");
    assert_eq!(component.query(), "needle");
}

#[test]
fn search_component_focus_propagates_to_the_input() {
    let mut component = AltScreenSearchComponent::new();
    assert!(!component.is_focused());
    // The fake cursor only appears while focused.
    let unfocused = component.render(20)[1].clone();
    assert!(!unfocused.contains(pillar_tui::input::CURSOR_MARKER));
    Focusable::set_focused(&mut component, true);
    assert!(component.is_focused());
    let focused = component.render(20)[1].clone();
    assert!(
        focused.contains(pillar_tui::input::CURSOR_MARKER),
        "{focused:?}"
    );
}
