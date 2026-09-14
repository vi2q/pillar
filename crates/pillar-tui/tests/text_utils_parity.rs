//! Parity tests for tui utils.ts text-metrics core (pi v0.84.3): ANSI
//! extraction, visible width, SGR tracking, and ANSI-preserving wrapping.

use pillar_tui::text_utils::{
    AnsiCodeTracker, apply_background_to_line, extract_ansi_code, get_grapheme_cell_range,
    get_osc8_link_at_column, grapheme_width, strip_terminal_sequences, visible_width,
    wrap_text_with_ansi,
};

fn codes(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            result.push(ansi.code);
            i += ansi.length;
        } else {
            i += 1;
        }
    }
    result
}

// --- ANSI extraction -------------------------------------------------------------------------

#[test]
fn extract_csi_osc_apc_and_2char() {
    assert_eq!(codes("\u{1b}[31mred"), vec!["\u{1b}[31m"]);
    assert_eq!(codes("\u{1b}[0;1;38;5;240mx"), vec!["\u{1b}[0;1;38;5;240m"]);
    assert_eq!(
        codes("a\u{1b}]8;;http://x\u{7}b"),
        vec!["\u{1b}]8;;http://x\u{7}"]
    );
    assert_eq!(
        codes("\u{1b}]8;;http://x\u{1b}\\y"),
        vec!["\u{1b}]8;;http://x\u{1b}\\"]
    );
    assert_eq!(codes("\u{1b}_marker\u{7}"), vec!["\u{1b}_marker\u{7}"]);
    assert_eq!(codes("\u{1b}Kz"), vec!["\u{1b}K"]);
    assert!(codes("plain").is_empty());
    // Unclosed CSI yields nothing.
    assert!(codes("\u{1b}[31").is_empty());
}

#[test]
fn strip_sequences_preserves_text() {
    assert_eq!(strip_terminal_sequences("\u{1b}[31mred\u{1b}[0m"), "red");
    assert_eq!(strip_terminal_sequences("plain"), "plain");
    assert_eq!(
        strip_terminal_sequences("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"),
        "link"
    );
}

// --- visible width -------------------------------------------------------------------------

#[test]
fn visible_width_ascii_and_wide_chars() {
    assert_eq!(visible_width(""), 0);
    assert_eq!(visible_width("hello"), 5);
    // CJK wide chars are 2 columns.
    assert_eq!(visible_width("日本"), 4);
    // Emoji are 2 columns.
    assert_eq!(visible_width("👍"), 2);
    // Combining marks add nothing.
    assert_eq!(visible_width("e\u{301}"), 1);
    // Tabs count as 3.
    assert_eq!(visible_width("\t"), 3);
    // ANSI codes don't count.
    assert_eq!(visible_width("\u{1b}[31mab\u{1b}[0m"), 2);
    // Fullwidth forms.
    assert_eq!(visible_width("Ａ"), 2);
}

#[test]
fn grapheme_width_basics() {
    assert_eq!(grapheme_width("\t"), 3);
    assert_eq!(grapheme_width("a"), 1);
    assert_eq!(grapheme_width("日"), 2);
    assert_eq!(grapheme_width("\u{301}"), 0); // combining mark alone is zero-width
}

// --- SGR tracking ---------------------------------------------------------------------------

#[test]
fn tracker_accumulates_and_resets() {
    let mut tracker = AnsiCodeTracker::new();
    assert_eq!(tracker.get_active_codes(), "");
    tracker.process("\u{1b}[1m");
    assert_eq!(tracker.get_active_codes(), "\u{1b}[1m");
    tracker.process("\u{1b}[31m");
    assert_eq!(tracker.get_active_codes(), "\u{1b}[1;31m");
    // Full reset clears everything.
    tracker.process("\u{1b}[0m");
    assert_eq!(tracker.get_active_codes(), "");
}

#[test]
fn tracker_attribute_specific_resets() {
    let mut tracker = AnsiCodeTracker::new();
    tracker.process("\u{1b}[1;4m"); // bold + underline
    tracker.process("\u{1b}[22m"); // bold off, dim off
    assert_eq!(tracker.get_active_codes(), "\u{1b}[4m");
    tracker.process("\u{1b}[24m"); // underline off
    assert_eq!(tracker.get_active_codes(), "");
}

#[test]
fn tracker_colors() {
    let mut tracker = AnsiCodeTracker::new();
    tracker.process("\u{1b}[38;5;240m");
    assert_eq!(tracker.get_active_codes(), "\u{1b}[38;5;240m");
    tracker.process("\u{1b}[48;2;1;2;3m");
    assert_eq!(tracker.get_active_codes(), "\u{1b}[38;5;240;48;2;1;2;3m");
    tracker.process("\u{1b}[39m"); // default fg
    assert_eq!(tracker.get_active_codes(), "\u{1b}[48;2;1;2;3m");
    tracker.process("\u{1b}[31m");
    assert_eq!(tracker.get_active_codes(), "\u{1b}[31;48;2;1;2;3m");
}

#[test]
fn line_end_reset_only_underline() {
    let mut tracker = AnsiCodeTracker::new();
    assert_eq!(tracker.get_line_end_reset(), "");
    tracker.process("\u{1b}[4m");
    assert_eq!(tracker.get_line_end_reset(), "\u{1b}[24m");
    // Bold does not need a line-end reset (preserves background).
    tracker.clear();
    tracker.process("\u{1b}[1m");
    assert_eq!(tracker.get_line_end_reset(), "");
}

// --- wrapping ----------------------------------------------------------------------------------

#[test]
fn wrap_short_lines_pass_through() {
    assert_eq!(wrap_text_with_ansi("", 10), vec![""]);
    assert_eq!(wrap_text_with_ansi("short", 10), vec!["short"]);
}

#[test]
fn wrap_breaks_at_word_boundaries() {
    let wrapped = wrap_text_with_ansi("hello world foo", 11);
    assert_eq!(wrapped, vec!["hello world", "foo"]);
}

#[test]
fn wrap_breaks_long_tokens_by_characters() {
    let wrapped = wrap_text_with_ansi("abcdefghij", 4);
    assert_eq!(wrapped, vec!["abcd", "efgh", "ij"]);
}

#[test]
fn wrap_strips_trailing_whitespace_per_line() {
    let wrapped = wrap_text_with_ansi("hello   world   x", 8);
    assert!(
        wrapped.iter().all(|line| !line.ends_with(' ')),
        "{wrapped:?}"
    );
}

#[test]
fn wrap_preserves_ansi_across_breaks() {
    let wrapped = wrap_text_with_ansi("\u{1b}[31mhello world red\u{1b}[0m", 11);
    assert_eq!(wrapped.len(), 2);
    // The second line re-opens the active color.
    assert!(wrapped[1].starts_with("\u{1b}[31m"), "{wrapped:?}");
    // The final line keeps its own reset from the input.
    assert!(wrapped[1].ends_with("\u{1b}[0m"), "{wrapped:?}");
}

#[test]
fn wrap_underline_reset_at_line_end() {
    let wrapped = wrap_text_with_ansi("\u{1b}[4mword one two\u{1b}[0m", 9);
    // Interior line ends close the underline.
    assert!(wrapped[0].ends_with("\u{1b}[24m"), "{wrapped:?}");
    // The next line re-opens it.
    assert!(wrapped[1].starts_with("\u{1b}[4m"), "{wrapped:?}");
}

#[test]
fn wrap_handles_newlines_with_style_carry() {
    let wrapped = wrap_text_with_ansi("\u{1b}[31mred\u{1b}[0m\nplain", 20);
    // Lines that fit pass through with their escapes intact.
    assert_eq!(wrapped, vec!["\u{1b}[31mred\u{1b}[0m", "plain"]);
    // Style carries over an explicit newline.
    let wrapped = wrap_text_with_ansi("\u{1b}[31mred\nstill\u{1b}[0m", 20);
    // Each line keeps its raw form; the second re-opens the active color.
    assert_eq!(wrapped, vec!["\u{1b}[31mred", "\u{1b}[31mstill\u{1b}[0m"]);
}

// --- background ----------------------------------------------------------------------------------

#[test]
fn apply_background_pads_to_width() {
    let result = apply_background_to_line("ab", 5, &|text| format!("[{text}]"));
    assert_eq!(result, "[ab   ]");
}

// --- grapheme cell ranges (upstream getGraphemeCellRange) ----------------------------------------

#[test]
fn grapheme_cell_range_maps_columns_to_cells() {
    assert_eq!(get_grapheme_cell_range("abc", 0), Some((0, 1)));
    assert_eq!(get_grapheme_cell_range("abc", 2), Some((2, 3)));
    // Out-of-range columns have no grapheme.
    assert_eq!(get_grapheme_cell_range("abc", 3), None);
    // Escape sequences occupy no cells.
    assert_eq!(get_grapheme_cell_range("\u{1b}[31mab", 1), Some((1, 2)));
    // A double-width grapheme spans two cells.
    assert_eq!(get_grapheme_cell_range("中x", 0), Some((0, 2)));
    assert_eq!(get_grapheme_cell_range("中x", 1), Some((0, 2)));
    assert_eq!(get_grapheme_cell_range("中x", 2), Some((2, 3)));
    assert_eq!(get_grapheme_cell_range("", 0), None);
}

// --- OSC 8 hyperlinks (upstream getOsc8LinkAtColumn) --------------------------------------------

fn link(url: &str, text: &str) -> String {
    format!("\u{1b}]8;;{url}\u{7}{text}\u{1b}]8;;\u{7}")
}

#[test]
fn osc8_link_lookup_follows_open_and_close() {
    let line = format!("{}rest", link("https://example.com", "click"));
    assert_eq!(
        get_osc8_link_at_column(&line, 0),
        Some("https://example.com".to_string())
    );
    assert_eq!(
        get_osc8_link_at_column(&line, 4),
        Some("https://example.com".to_string())
    );
    // Past the closing sequence the link is cleared.
    assert_eq!(get_osc8_link_at_column(&line, 5), None);
    assert_eq!(get_osc8_link_at_column(&line, 8), None);
}

#[test]
fn osc8_link_lookup_handles_st_terminator_and_absent_links() {
    let line = "\u{1b}]8;;https://a.example\u{1b}\\hi";
    assert_eq!(
        get_osc8_link_at_column(line, 1),
        Some("https://a.example".to_string())
    );
    assert_eq!(get_osc8_link_at_column("plain", 1), None);
    // An empty URL closes the current link.
    let closed = format!("{}\u{1b}]8;;\u{7}", link("https://b.example", "x"));
    assert_eq!(get_osc8_link_at_column(&closed, 1), None);
    // Tabs occupy three cells (upstream's special case).
    let tabbed = "\u{1b}]8;;https://c.example\u{7}\t".to_string();
    assert_eq!(
        get_osc8_link_at_column(&tabbed, 2),
        Some("https://c.example".to_string())
    );
    assert_eq!(get_osc8_link_at_column(&tabbed, 3), None);
}
