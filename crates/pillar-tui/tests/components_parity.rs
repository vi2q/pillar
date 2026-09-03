//! Parity tests for tui components: Text, Spacer, Box, TruncatedText
//! (pi v0.84.3).

use pillar_tui::components::{BoxComponent, Spacer, Text, TruncatedText};

// --- Text ---------------------------------------------------------------------------------------

#[test]
fn text_wraps_and_pads() {
    let mut text = Text::new("hello world foo", 1, 1);
    let lines = text.render(15);
    // paddingY(1) + 2 content lines + paddingY(1)
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], " ".repeat(15));
    // Content lines are padded to full width.
    assert_eq!(lines[1], " hello world   ");
    assert_eq!(lines[2], " foo           ");
    assert_eq!(lines[3], " ".repeat(15));
}

#[test]
fn text_reduces_padding_to_fit() {
    // width 7, paddingX 2 → content width 3: the word hard-breaks.
    let mut text = Text::new("hello", 2, 0);
    let lines = text.render(7);
    assert_eq!(lines, vec!["  hel  ", "  lo   "]);
}

#[test]
fn text_empty_renders_nothing() {
    let mut text = Text::new("", 1, 1);
    assert!(text.render(20).is_empty());
    let mut text = Text::new("   ", 1, 1);
    assert!(text.render(20).is_empty());
}

#[test]
fn text_preserves_explicit_newlines() {
    let mut text = Text::new("a\nb", 0, 0);
    let lines = text.render(10);
    assert_eq!(lines, vec!["a         ", "b         "]);
}

#[test]
fn text_tabs_become_three_spaces() {
    let mut text = Text::new("a\tb", 0, 0);
    let lines = text.render(10);
    assert_eq!(lines[0], "a   b     ");
}

#[test]
fn text_cache_invalidates_on_set_text() {
    let mut text = Text::new("first", 0, 0);
    assert_eq!(text.render(10), vec!["first     "]);
    text.set_text("second");
    assert_eq!(text.render(10), vec!["second    "]);
}

#[test]
fn text_background_applied_and_padded() {
    let mut text = Text::with_bg("ab", 0, 0, Box::new(|t| format!("<{t}>")));
    let lines = text.render(5);
    assert_eq!(lines, vec!["<ab   >"]);
}

// --- Spacer ---------------------------------------------------------------------------------------

#[test]
fn spacer_renders_empty_lines() {
    let mut spacer = Spacer::new(3);
    assert_eq!(spacer.render(10), vec!["", "", ""]);
    spacer.set_lines(1);
    assert_eq!(spacer.render(10), vec![""]);
}

// --- Box ------------------------------------------------------------------------------------------

#[test]
fn box_applies_padding_and_background() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["hello".to_string()]));
    base.set_bg_fn(Some(Box::new(|t| format!("<{t}>"))));
    let lines = base.render(11);
    assert_eq!(lines, vec!["< hello     >"]);
}

#[test]
fn box_top_bottom_padding_and_children() {
    let mut base = BoxComponent::new(1, 1);
    base.add_child(Box::new(|_width| vec!["content".to_string()]));
    let lines = base.render(12);
    // paddingY(1) top + content + paddingY(1) bottom; width 12 with
    // paddingX 1 → content width 10, " content  " + pad.
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], " ".repeat(12));
    assert_eq!(lines[1], " content    ");
    assert_eq!(lines[2], " ".repeat(12));
}

#[test]
fn box_clear_removes_children() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["x".to_string()]));
    base.clear();
    assert!(base.render(10).is_empty());
}

#[test]
fn box_cache_tracks_bg_changes_by_sampling() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["x".to_string()]));
    base.set_bg_fn(Some(Box::new(|t| format!("A{t}A"))));
    let first = base.render(5);
    assert_eq!(first, vec!["A x   A"]);
    // Same bg: cached output stays.
    let cached = base.render(5);
    assert_eq!(cached, vec!["A x   A"]);
    // Changed bg: re-render picks it up (bg change detected by sampling).
    base.set_bg_fn(Some(Box::new(|t| format!("B{t}B"))));
    let second = base.render(5);
    assert_eq!(second, vec!["B x   B"]);
}

// --- TruncatedText ----------------------------------------------------------------------------------

#[test]
fn truncated_text_truncates_with_ellipsis() {
    let component = TruncatedText::new("hello world", 0, 0);
    let lines = component.render(8);
    // finalizeTruncatedResult appends the reset around the ellipsis.
    assert_eq!(lines, vec!["hello\u{1b}[0m...\u{1b}[0m"]);
}

#[test]
fn truncated_text_stops_at_newline() {
    let component = TruncatedText::new("first\nsecond", 0, 0);
    let lines = component.render(20);
    assert_eq!(lines, vec!["first               "]);
}

#[test]
fn truncated_text_pads_and_applies_padding() {
    let component = TruncatedText::new("hi", 1, 1);
    let lines = component.render(10);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1], " hi       ");
}

#[test]
fn truncated_text_wide_chars() {
    let component = TruncatedText::new("日本語", 0, 0);
    let lines = component.render(5);
    // width 5 - ellipsis(3) = 2 target → 日 only, then "..." (wrapped in
    // reset codes like the ASCII case).
    assert_eq!(lines[0], "日\u{1b}[0m...\u{1b}[0m");
}
