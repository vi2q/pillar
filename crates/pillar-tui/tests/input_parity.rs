//! Parity tests for the Input component (pi v0.84.3
//! components/input.ts) and printable key decoding (keys.ts).

use pillar_tui::tui::RenderLines;
use pillar_tui::edit_support::KillRing;
use pillar_tui::input::{CURSOR_MARKER, Input};
use pillar_tui::keys::{
    decode_kitty_printable, decode_modify_other_keys_printable, decode_printable_key,
};
use pillar_tui::text_utils::visible_width;


// --- Input basics -----------------------------------------------------------------------------

/// The shared frame as owned lines (the parity assertions compare strings).
fn to_vec(lines: RenderLines) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

#[test]
fn input_starts_empty() {
    let input = Input::new();
    assert_eq!(input.get_value(), "");
}

#[test]
fn input_set_value_clamps_cursor() {
    let mut input = Input::new();
    input.set_value("hello");
    input.set_cursor(5);
    input.set_value("hi");
    assert_eq!(input.cursor(), 2);
}

#[test]
fn input_insert_characters() {
    let mut input = Input::new();
    input.insert_character("a");
    input.insert_character("b");
    input.insert_character("c");
    assert_eq!(input.get_value(), "abc");
    assert_eq!(input.cursor(), 3);
}

#[test]
fn input_cursor_movement_by_grapheme() {
    let mut input = Input::new();
    input.set_value("日本");
    input.set_cursor(6);
    input.cursor_left();
    assert_eq!(input.cursor(), 3);
    input.cursor_left();
    assert_eq!(input.cursor(), 0);
    input.cursor_left();
    assert_eq!(input.cursor(), 0);
    input.cursor_right();
    assert_eq!(input.cursor(), 3);
}

#[test]
fn input_backspace_removes_grapheme() {
    let mut input = Input::new();
    input.set_value("日本");
    input.set_cursor(6);
    input.backspace();
    assert_eq!(input.get_value(), "日");
    input.backspace();
    assert_eq!(input.get_value(), "");
    input.backspace();
    assert_eq!(input.get_value(), "");
}

#[test]
fn input_forward_delete() {
    let mut input = Input::new();
    input.set_value("abc");
    input.set_cursor(0);
    input.forward_delete();
    assert_eq!(input.get_value(), "bc");
}

#[test]
fn input_undo_restores_state() {
    let mut input = Input::new();
    input.insert_character("a");
    input.insert_character("b");
    input.undo();
    assert_eq!(input.get_value(), "");
    assert_eq!(input.cursor(), 0);
}

#[test]
fn input_undo_coalesces_consecutive_word_chars() {
    let mut input = Input::new();
    input.insert_character("a");
    input.insert_character("b");
    input.insert_character("c");
    // All three coalesce into one undo unit.
    input.undo();
    assert_eq!(input.get_value(), "");
}

#[test]
fn input_whitespace_breaks_undo_coalescing() {
    let mut input = Input::new();
    input.insert_character("a");
    input.insert_character("b");
    input.insert_character(" ");
    input.undo();
    // Whitespace starts a new unit: undo drops only the space.
    assert_eq!(input.get_value(), "ab");
}

// --- kill ring / yank ---------------------------------------------------------------------------

#[test]
fn input_delete_word_backwards_accumulates_kills() {
    let mut input = Input::new();
    input.set_value("one two three");
    input.set_cursor(13);
    input.delete_word_backwards();
    assert_eq!(input.get_value(), "one two ");
    input.delete_word_backwards();
    assert_eq!(input.get_value(), "one ");
    // Consecutive kills accumulate: yank restores both words.
    input.yank();
    assert_eq!(input.get_value(), "one two three");
}

#[test]
fn input_yank_pop_rotates() {
    let mut ring = KillRing::new();
    ring.push("first", false, false);
    ring.push("second", false, false);
    ring.rotate();
    assert_eq!(ring.peek(), Some("first"));
    ring.rotate();
    assert_eq!(ring.peek(), Some("second"));
}

#[test]
fn input_delete_to_line_start_and_end() {
    let mut input = Input::new();
    input.set_value("hello world");
    input.set_cursor(5);
    input.delete_to_line_end();
    assert_eq!(input.get_value(), "hello");
    input.set_value("hello world");
    input.set_cursor(6);
    input.delete_to_line_start();
    assert_eq!(input.get_value(), "world");
}

// --- paste ---------------------------------------------------------------------------------------

#[test]
fn input_paste_strips_newlines_and_expands_tabs() {
    let mut input = Input::new();
    input.handle_paste("a\r\nb\rc\nd\te");
    assert_eq!(input.get_value(), "abcd    e");
}

#[test]
fn input_bracketed_paste_via_handle_input() {
    let mut input = Input::new();
    input.handle_input("\u{1b}[200~pasted\u{1b}[201~");
    assert_eq!(input.get_value(), "pasted");
}

// --- printable decoding ---------------------------------------------------------------------------

#[test]
fn decode_kitty_printable_plain_letter() {
    assert_eq!(decode_kitty_printable("\u{1b}[97u"), Some("a".to_string()));
}

#[test]
fn decode_kitty_printable_with_shift_prefers_shifted_key() {
    // 'a' (97) with shift → shifted key 'A' (65).
    assert_eq!(
        decode_kitty_printable("\u{1b}[97:65;2u"),
        Some("A".to_string())
    );
}

#[test]
fn decode_kitty_printable_rejects_ctrl_alt() {
    // ctrl (modifier bit 4 → 5 in sequence).
    assert_eq!(decode_kitty_printable("\u{1b}[97;5u"), None);
    // alt (bit 2 → 3).
    assert_eq!(decode_kitty_printable("\u{1b}[97;3u"), None);
}

#[test]
fn decode_kitty_printable_rejects_control_codepoints() {
    assert_eq!(decode_kitty_printable("\u{1b}[27u"), None);
}

#[test]
fn decode_modify_other_keys_printable_sequence() {
    assert_eq!(
        decode_modify_other_keys_printable("\u{1b}[27;1;65~"),
        Some("A".to_string())
    );
    // Ctrl+letter rejected.
    assert_eq!(decode_modify_other_keys_printable("\u{1b}[27;5;65~"), None);
}

#[test]
fn decode_printable_key_falls_back() {
    assert_eq!(decode_printable_key("\u{1b}[97u"), Some("a".to_string()));
    assert_eq!(
        decode_printable_key("\u{1b}[27;1;66~"),
        Some("B".to_string())
    );
    assert_eq!(decode_printable_key("\u{1b}[A"), None);
}

#[test]
fn input_rejects_control_characters() {
    let mut input = Input::new();
    input.handle_input("\u{3}");
    assert_eq!(input.get_value(), "");
}

#[test]
fn input_accepts_unicode() {
    let mut input = Input::new();
    input.handle_input("日");
    assert_eq!(input.get_value(), "日");
}

// --- rendering ------------------------------------------------------------------------------------

#[test]
fn render_short_value_fits_with_cursor() {
    let mut input = Input::new();
    input.set_value("hi");
    let lines = to_vec(input.render(20));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("> "), "{:?}", lines[0]);
    // Cursor (inverse video) sits at the value start, over 'h'.
    assert!(lines[0].contains("\u{1b}[7mh\u{1b}[27m"), "{:?}", lines[0]);
}

#[test]
fn render_prompt_only_when_too_narrow() {
    let input = Input::new();
    assert_eq!(to_vec(input.render(2)), vec!["> ".to_string()]);
}

#[test]
fn render_scrolls_horizontally_when_long() {
    let mut input = Input::new();
    let long = "x".repeat(50);
    input.set_value(&long);
    input.set_cursor(50);
    let lines = to_vec(input.render(20));
    assert_eq!(lines.len(), 1);
    // Line is prompt + scrolled window.
    assert!(lines[0].starts_with("> "), "{:?}", lines[0]);
    assert!(visible_width(&lines[0]) <= 20 + visible_width("\u{1b}[7m \u{1b}[27m") + 1);
}

#[test]
fn render_emits_cursor_marker_when_focused() {
    let mut input = Input::new();
    input.focused = true;
    let lines = to_vec(input.render(20));
    assert!(lines[0].contains(CURSOR_MARKER), "{:?}", lines[0]);
}

#[test]
fn render_no_marker_when_unfocused() {
    let input = Input::new();
    let lines = to_vec(input.render(20));
    assert!(!lines[0].contains(CURSOR_MARKER));
}
