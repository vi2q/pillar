//! Parity tests for the editor core (pi v0.84.3 components/editor.ts).

use pillar_tui::editor::{Editor, TextChunk, create_scroll_border, word_wrap_line};

fn raw_segments(text: &str) -> Vec<(String, usize)> {
    let mut offset = 0usize;
    pillar_tui::text_utils::grapheme_clusters(text)
        .into_iter()
        .map(|seg| {
            let start = offset;
            offset += seg.len();
            (seg, start)
        })
        .collect()
}

// --- wordWrapLine -----------------------------------------------------------------------------

#[test]
fn word_wrap_short_line_is_single_chunk() {
    let chunks = word_wrap_line("hello", 10, &raw_segments("hello"));
    assert_eq!(
        chunks,
        vec![TextChunk {
            text: "hello".to_string(),
            start_index: 0,
            end_index: 5,
        }]
    );
}

#[test]
fn word_wrap_empty_line() {
    let chunks = word_wrap_line("", 10, &raw_segments(""));
    assert_eq!(chunks[0].text, "");
}

#[test]
fn word_wrap_zero_width_is_single_empty_chunk() {
    let chunks = word_wrap_line("abc", 0, &raw_segments("abc"));
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "");
}

#[test]
fn word_wrap_breaks_at_word_boundary() {
    let line = "hello world";
    let chunks = word_wrap_line(line, 6, &raw_segments(line));
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    // Upstream slices to the wrap opportunity index, which includes the
    // trailing space of the first chunk.
    assert_eq!(chunks[0].text, "hello ");
    assert_eq!(chunks[0].start_index, 0);
    assert_eq!(chunks[1].text, "world");
    assert_eq!(chunks[1].start_index, 6);
}

#[test]
fn word_wrap_force_breaks_long_words() {
    let line = "aaaaaaaaaaaaaaaa";
    let chunks = word_wrap_line(line, 5, &raw_segments(line));
    // Pure character-level wrapping: three full chunks plus the tail.
    assert_eq!(chunks.len(), 4, "{chunks:?}");
    assert_eq!(chunks[0].text, "aaaaa");
    assert_eq!(chunks[3].text, "a");
}

#[test]
fn word_wrap_keeps_multiple_spaces_with_following_word() {
    let line = "ab    cd";
    let chunks = word_wrap_line(line, 6, &raw_segments(line));
    // Break after the last space: "ab    " then "cd".
    assert_eq!(chunks[0].text, "ab    ");
    assert_eq!(chunks[1].text, "cd");
}

#[test]
fn word_wrap_cjk_breaks_between_characters() {
    let line = "日本語テスト";
    let chunks = word_wrap_line(line, 4, &raw_segments(line));
    // Each wide char is 2 columns → 2 chars per chunk.
    assert!(chunks.len() >= 2, "{chunks:?}");
    assert_eq!(chunks[0].text, "日本");
}

#[test]
fn word_wrap_chunk_indices_cover_the_line() {
    let line = "the quick brown fox jumps";
    let chunks = word_wrap_line(line, 10, &raw_segments(line));
    // Chunks partition the line by byte index.
    for window in chunks.windows(2) {
        assert_eq!(window[0].end_index, window[1].start_index);
    }
    assert_eq!(chunks.last().unwrap().end_index, line.len());
}

// --- createScrollBorder ------------------------------------------------------------------------

#[test]
fn scroll_border_fits_width() {
    let border = create_scroll_border('↑', 5, 40);
    assert!(border.starts_with("─── ↑ 5 more "), "{border:?}");
    assert_eq!(pillar_tui::text_utils::visible_width(&border), 40);
}

#[test]
fn scroll_border_narrow_falls_back_to_ellipsis() {
    let border = create_scroll_border('↓', 12, 6);
    assert!(border.ends_with("..."), "{border:?}");
    assert_eq!(pillar_tui::text_utils::visible_width(&border), 6);
}

// --- editor state -------------------------------------------------------------------------------

#[test]
fn editor_starts_empty() {
    let editor = Editor::new();
    assert_eq!(editor.get_text(), "");
    assert_eq!(editor.get_cursor(), (0, 0));
}

#[test]
fn editor_set_text_normalizes_line_endings_and_tabs() {
    let mut editor = Editor::new();
    editor.set_text("a\r\nb\rc\td");
    assert_eq!(editor.get_text(), "a\nb\nc    d");
}

#[test]
fn editor_get_lines() {
    let mut editor = Editor::new();
    editor.set_text("one\ntwo");
    assert_eq!(
        editor.get_lines(),
        vec!["one".to_string(), "two".to_string()]
    );
}

// --- insertion / undo coalescing ------------------------------------------------------------------

#[test]
fn insert_character_builds_lines() {
    let mut editor = Editor::new();
    for ch in "hello".chars() {
        editor.insert_character(&ch.to_string());
    }
    assert_eq!(editor.get_text(), "hello");
    assert_eq!(editor.get_cursor(), (0, 5));
}

#[test]
fn insert_character_undo_coalescing_word_vs_space() {
    let mut editor = Editor::new();
    editor.insert_character("a");
    editor.insert_character("b");
    editor.undo();
    // "ab" coalesced into one unit.
    assert_eq!(editor.get_text(), "");
    editor.insert_character("a");
    editor.insert_character("b");
    editor.insert_character(" ");
    editor.undo();
    // Space starts a new unit: undo removes only the space.
    assert_eq!(editor.get_text(), "ab");
}

#[test]
fn add_new_line_splits_at_cursor() {
    let mut editor = Editor::new();
    editor.set_text("hello");
    for _ in 0..3 {
        editor.move_cursor(0, -1);
    } // cursor at col 2 ("he|llo")
    editor.add_new_line();
    assert_eq!(
        editor.get_lines(),
        vec!["he".to_string(), "llo".to_string()]
    );
    assert_eq!(editor.get_cursor(), (1, 0));
}

#[test]
fn insert_text_at_cursor_multiline() {
    let mut editor = Editor::new();
    editor.set_text("world");
    for _ in 0..3 {
        editor.move_cursor(0, -1);
    } // col 2: "wo|rld"
    editor.insert_text_at_cursor("a\nb\nc");
    assert_eq!(
        editor.get_lines(),
        vec!["woa".to_string(), "b".to_string(), "crld".to_string()]
    );
    assert_eq!(editor.get_cursor(), (2, 1));
}

#[test]
fn insert_text_at_cursor_is_atomic_for_undo() {
    let mut editor = Editor::new();
    editor.set_text("base");
    editor.insert_text_at_cursor("+more");
    assert_eq!(editor.get_text(), "base+more");
    editor.undo();
    assert_eq!(editor.get_text(), "base");
}

// --- deletion ---------------------------------------------------------------------------------------

#[test]
fn backspace_merges_lines_at_col_zero() {
    let mut editor = Editor::new();
    editor.set_text("one\ntwo");
    editor.move_to_line_start(); // set_text leaves the cursor at the end
    editor.backspace();
    assert_eq!(editor.get_text(), "onetwo");
    assert_eq!(editor.get_cursor(), (0, 3));
}

#[test]
fn forward_delete_merges_next_line_at_end() {
    let mut editor = Editor::new();
    editor.set_text("one\ntwo");
    editor.move_cursor(-1, 0); // up to line 0 (col preserved)
    editor.move_to_line_end(); // (0,3)
    editor.forward_delete();
    assert_eq!(editor.get_text(), "onetwo");
}

#[test]
fn delete_to_start_and_end_of_line() {
    let mut editor = Editor::new();
    editor.set_text("hello world");
    assert_eq!(editor.get_cursor(), (0, 11));
    for _ in 0..5 {
        editor.move_cursor(0, -1);
    } // col 6
    assert_eq!(editor.get_cursor(), (0, 6), "cursor after 5 lefts");
    editor.delete_to_start_of_line();
    assert_eq!(editor.get_text(), "world");
    editor.set_text("hello world");
    for _ in 0..6 {
        editor.move_cursor(0, -1);
    } // col 5, before "world"
    editor.delete_to_end_of_line();
    assert_eq!(editor.get_text(), "hello");
}

#[test]
fn delete_word_backwards_accumulates_kills() {
    let mut editor = Editor::new();
    editor.set_text("one two three");
    editor.move_to_line_end();
    editor.delete_word_backwards();
    assert_eq!(editor.get_text(), "one two ");
    editor.delete_word_backwards();
    assert_eq!(editor.get_text(), "one ");
    // Consecutive kills accumulate; yank restores both.
    assert!(editor.yank());
    assert_eq!(editor.get_text(), "one two three");
}

#[test]
fn delete_word_forward() {
    let mut editor = Editor::new();
    editor.set_text("one two");
    editor.move_to_line_start();
    editor.delete_word_forward();
    assert_eq!(editor.get_text(), " two");
}

// --- kill ring / yank ---------------------------------------------------------------------------------

#[test]
fn yank_and_yank_pop_cycle() {
    let mut editor = Editor::new();
    editor.set_text("alpha beta");
    // Kill "beta".
    editor.move_to_line_end();
    editor.delete_word_backwards();
    assert_eq!(editor.get_text(), "alpha ");
    // A cursor move resets the kill accumulation so the next kill is a
    // separate ring entry.
    editor.move_cursor(0, -1);
    editor.move_cursor(0, 1);
    editor.delete_word_backwards();
    assert_eq!(editor.get_text(), "");
    assert!(editor.yank());
    assert_eq!(editor.get_text(), "alpha ");
    assert!(editor.yank_pop());
    assert_eq!(editor.get_text(), "beta");
}

#[test]
fn yank_pop_requires_preceding_yank() {
    let mut editor = Editor::new();
    editor.set_text("x");
    assert!(!editor.yank_pop());
}

// --- movement ------------------------------------------------------------------------------------------

#[test]
fn move_cursor_right_wraps_to_next_line() {
    let mut editor = Editor::new();
    editor.set_text("ab\ncd");
    editor.move_cursor(-1, 0); // up to line 0
    editor.move_to_line_start();
    for _ in 0..2 {
        editor.move_cursor(0, 1);
    }
    editor.move_cursor(0, 1); // wrap to line 1
    assert_eq!(editor.get_cursor(), (1, 0));
}

#[test]
fn move_cursor_left_wraps_to_prev_line() {
    let mut editor = Editor::new();
    editor.set_text("ab\ncd");
    editor.move_cursor(-1, 0); // up to line 0
    editor.move_cursor(1, 0); // down to line 1 (col preserved at 2)
    editor.move_to_line_start();
    editor.move_cursor(0, -1); // wrap back to end of line 0
    assert_eq!(editor.get_cursor(), (0, 2));
}

#[test]
fn vertical_movement_preserves_column() {
    let mut editor = Editor::new();
    editor.set_text("hello\nworld\n!");
    editor.move_cursor(-2, 0); // up to line 0 col 1
    editor.move_to_line_start();
    for _ in 0..3 {
        editor.move_cursor(0, 1);
    } // col 3 on line 0
    editor.move_cursor(1, 0);
    assert_eq!(editor.get_cursor(), (1, 3));
    editor.move_cursor(1, 0);
    // Short last line clamps to its length (1).
    assert_eq!(editor.get_cursor(), (2, 1));
}

#[test]
fn move_word_boundaries_across_lines() {
    let mut editor = Editor::new();
    editor.set_text("one two\nthree");
    editor.move_to_line_start();
    editor.move_to_line_end(); // (1,5)
    editor.move_word_backwards();
    // One hop back lands at the start of "three".
    assert_eq!(editor.get_cursor(), (1, 0));
    editor.move_word_backwards();
    // Now at start of line 1: moves to end of line 0.
    assert_eq!(editor.get_cursor(), (0, 7));
}

// --- char jump -------------------------------------------------------------------------------------------

#[test]
fn jump_to_char_forward_and_backward() {
    let mut editor = Editor::new();
    editor.set_text("abcabc");
    editor.move_to_line_start();
    editor.jump_to_char('c', true);
    assert_eq!(editor.get_cursor(), (0, 2));
    editor.jump_to_char('c', true);
    assert_eq!(editor.get_cursor(), (0, 5));
    editor.jump_to_char('c', false);
    assert_eq!(editor.get_cursor(), (0, 2));
}

#[test]
fn jump_to_char_multiline_skips_cursor_position() {
    let mut editor = Editor::new();
    editor.set_text("ax\nxa");
    editor.move_to_line_start();
    editor.jump_to_char('a', true);
    // Skips current position (0), finds 'a' at line 1 col 1.
    assert_eq!(editor.get_cursor(), (1, 1));
}

// --- history -----------------------------------------------------------------------------------------------

#[test]
fn history_navigation_and_draft_restore() {
    let mut editor = Editor::new();
    editor.add_to_history("first prompt");
    editor.add_to_history("second prompt");
    // Typing something.
    editor.set_text("draft text");
    // Up → most recent entry ("second prompt"), cursor at start.
    editor.navigate_history(-1);
    assert_eq!(editor.get_text(), "second prompt");
    assert_eq!(editor.get_cursor(), (0, 0));
    // Up again → "first prompt".
    editor.navigate_history(-1);
    assert_eq!(editor.get_text(), "first prompt");
    // Down → back to "second prompt".
    editor.navigate_history(1);
    assert_eq!(editor.get_text(), "second prompt");
    // Down again → restore the draft.
    editor.navigate_history(1);
    assert_eq!(editor.get_text(), "draft text");
}

#[test]
fn history_skips_consecutive_duplicates_and_caps() {
    let mut editor = Editor::new();
    editor.add_to_history("same");
    editor.add_to_history("same");
    editor.add_to_history("same");
    editor.navigate_history(-1);
    assert_eq!(editor.get_text(), "same");
    editor.navigate_history(-1);
    // Only one entry existed.
    assert_eq!(editor.get_text(), "same");
}

#[test]
fn add_to_history_ignores_blank() {
    let mut editor = Editor::new();
    editor.add_to_history("   ");
    editor.navigate_history(-1);
    assert_eq!(editor.get_text(), "");
}

// --- submit -------------------------------------------------------------------------------------------------

#[test]
fn submit_value_expands_paste_markers_and_resets() {
    let mut editor = Editor::new();
    editor.set_text("before ");
    editor.handle_paste(
        "pasted content that is quite long
"
        .repeat(11)
        .as_str(),
    );
    let text = editor.get_text();
    assert!(text.contains("[paste #1"), "{text:?}");
    let submitted = editor.submit_value();
    assert!(
        submitted.starts_with("before pasted content"),
        "{submitted:?}"
    );
    assert_eq!(editor.get_text(), "");
}

#[test]
fn should_submit_on_backslash_enter() {
    let mut editor = Editor::new();
    editor.set_text("cmd \\");
    assert!(editor.should_submit_on_backslash_enter());
    editor.set_text("cmd \\no");
    assert!(!editor.should_submit_on_backslash_enter());
}

// --- paste markers -------------------------------------------------------------------------------------------

#[test]
fn large_paste_becomes_marker() {
    let mut editor = Editor::new();
    let content = "line\n".repeat(12);
    editor.handle_paste(&content);
    let text = editor.get_text();
    assert!(text.contains("[paste #1 +13 lines]"), "{text:?}");
    // Expansion reproduces the pasted content including its trailing
    // newline (the marker replaced "[paste #1 +13 lines]").
    assert!(
        editor.get_expanded_text().ends_with("line\n"),
        "{:?}",
        editor.get_expanded_text()
    );
    assert!(editor.get_expanded_text().starts_with("line\n"));
}

#[test]
fn large_paste_chars_marker() {
    let mut editor = Editor::new();
    let content = "x".repeat(1200);
    editor.handle_paste(&content);
    assert!(
        editor.get_text().contains("[paste #1 1200 chars]"),
        "{}",
        editor.get_text()
    );
}

#[test]
fn small_multiline_paste_inserts_directly() {
    let mut editor = Editor::new();
    editor.handle_paste("a\nb");
    assert_eq!(editor.get_text(), "a\nb");
}

#[test]
fn backspace_on_paste_marker_removes_registry_entry_and_renumbers() {
    let mut editor = Editor::new();
    editor.handle_paste(&"a\n".repeat(11)); // [paste #1 +12 lines]
    editor.move_to_line_end();
    editor.insert_character(" ");
    editor.handle_paste(&"b\n".repeat(11)); // [paste #2 +12 lines]
    let text = editor.get_text();
    assert!(text.contains("[paste #2"), "{text:?}");
    // Delete the second marker.
    editor.backspace();
    assert!(
        !editor.get_text().contains("[paste #2"),
        "{}",
        editor.get_text()
    );
    // Delete the first marker too.
    editor.move_to_line_end();
    editor.backspace(); // the space
    editor.backspace(); // the first marker
    let after = editor.get_text();
    assert!(!after.contains("[paste #"), "{after:?}");
}

// --- visual lines / layout ------------------------------------------------------------------------------------

#[test]
fn visual_line_map_simple_lines() {
    let mut editor = Editor::new();
    editor.set_text("ab\ncd");
    let visual = editor.build_visual_line_map(80);
    assert_eq!(visual.len(), 2);
    assert_eq!(visual[0].logical_line, 0);
    assert_eq!(visual[1].logical_line, 1);
}

#[test]
fn visual_line_map_wraps_long_line() {
    let mut editor = Editor::new();
    editor.set_text("one two three");
    let visual = editor.build_visual_line_map(3);
    assert!(visual.len() > 1, "{visual:?}");
    assert_eq!(visual[0].logical_line, 0);
    assert_eq!(visual[0].start_col, 0);
}

#[test]
fn find_visual_line_at_end_position() {
    let mut editor = Editor::new();
    editor.set_text("hello world");
    let visual = editor.build_visual_line_map(5);
    // Cursor at the very end belongs to the last segment.
    editor.move_to_line_end();
    let index = editor.find_visual_line_at(&visual, 0, 11);
    assert_eq!(index, visual.len() - 1);
}

#[test]
fn layout_text_annotates_cursor_line() {
    let mut editor = Editor::new();
    editor.set_text("first\nsecond");
    let layout = editor.layout_text(80);
    assert_eq!(layout.len(), 2);
    assert!(!layout[0].has_cursor);
    assert!(layout[1].has_cursor);
    assert_eq!(layout[1].cursor_pos, Some(6));
}

#[test]
fn layout_text_empty_editor_single_cursor_line() {
    let editor = Editor::new();
    let layout = editor.layout_text(80);
    assert_eq!(layout.len(), 1);
    assert!(layout[0].has_cursor);
    assert_eq!(layout[0].cursor_pos, Some(0));
}

#[test]
fn layout_text_wraps_and_places_cursor_in_chunk() {
    let mut editor = Editor::new();
    editor.set_text("aaaa bbbb cccc");
    for _ in 0..9 {
        editor.move_cursor(0, -1);
    } // col 5, inside "bbbb"
    let layout = editor.layout_text(9);
    assert!(layout.len() >= 2, "{layout:?}");
    let cursor_chunks: Vec<_> = layout.iter().filter(|l| l.has_cursor).collect();
    assert_eq!(cursor_chunks.len(), 1);
    assert!(cursor_chunks[0].text.contains("bbbb"), "{layout:?}");
}

// --- sticky column ----------------------------------------------------------------------------------------------

#[test]
fn sticky_column_prefers_shorter_target_end() {
    let mut editor = Editor::new();
    editor.set_text("very long first line content\nab\nanother long line here");
    editor.move_cursor(-2, 0); // up to line 0
    editor.move_to_line_start();
    for _ in 0..5 {
        editor.move_cursor(0, 1);
    } // col 5 on line 0
    editor.move_cursor(1, 0); // onto "ab" (len 2) → clamped to end, preferred = 5
    assert_eq!(editor.get_cursor(), (1, 2));
    editor.move_cursor(1, 0); // onto line 2 → sticky col 5 restored
    assert_eq!(editor.get_cursor(), (2, 5));
}

#[test]
fn horizontal_movement_clears_preferred_column() {
    let mut editor = Editor::new();
    editor.set_text("long line one\nab\nlonger line two");
    editor.move_cursor(-2, 0); // up to line 0
    editor.move_to_line_start();
    for _ in 0..13 {
        editor.move_cursor(0, 1);
    } // col 13 on line 0
    editor.move_cursor(1, 0); // clamped on "ab", preferred set
    editor.move_cursor(0, -1); // horizontal move clears preferred
    editor.move_cursor(1, 0);
    // Without sticky col the cursor stays at col 1 (target end).
    assert_eq!(editor.get_cursor(), (2, 1));
}

// --- undo -----------------------------------------------------------------------------------------------------

#[test]
fn undo_restores_state_and_pastes() {
    let mut editor = Editor::new();
    editor.set_text("hello");
    editor.insert_character("!");
    editor.undo();
    assert_eq!(editor.get_text(), "hello");
}

#[test]
fn undo_empty_stack_is_noop() {
    let mut editor = Editor::new();
    assert!(!editor.undo());
}

// --- paste edge cases -------------------------------------------------------------------------------------------

#[test]
fn paste_after_word_char_prepends_space_for_paths() {
    let mut editor = Editor::new();
    editor.set_text("see");
    editor.handle_paste("/tmp/file.txt");
    assert_eq!(editor.get_text(), "see /tmp/file.txt");
}

#[test]
fn paste_strips_control_characters() {
    let mut editor = Editor::new();
    editor.handle_paste("a\u{1}b");
    assert_eq!(editor.get_text(), "ab");
}

#[test]
fn paste_expands_tabs() {
    let mut editor = Editor::new();
    editor.handle_paste("a\tb");
    assert_eq!(editor.get_text(), "a    b");
}

// --- page scroll -------------------------------------------------------------------------------------------------

#[test]
fn page_scroll_moves_by_page_size() {
    let mut editor = Editor::new();
    // 30 empty-ish lines.
    editor.set_text(&{
        let mut lines = Vec::new();
        for i in 0..30 {
            lines.push(format!("line {i}"));
        }
        lines.join("\n")
    });
    editor.terminal_rows = 20; // page size = 6
    editor.move_cursor(-29, 0); // to line 0
    editor.move_to_line_start();
    editor.page_scroll(1);
    let (_, col) = editor.get_cursor();
    assert_eq!(editor.get_cursor().0, 6);
    let _ = col;
}
