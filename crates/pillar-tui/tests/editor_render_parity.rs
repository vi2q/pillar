//! Parity tests for the editor render pipeline (pi v0.84.3
//! components/editor.ts `render` + its accessors).

use pillar_tui::editor::{Editor, SegmentMode, create_scroll_border};
use pillar_tui::editor_autocomplete::create_autocomplete_list;
use pillar_tui::input::CURSOR_MARKER;
use pillar_tui::select_list::{SelectList, SelectListTheme};
use pillar_tui::text_utils::visible_width;
use pillar_tui::tui::RenderLines;
use pillar_tui::tui::{Component, Focusable};


/// Drop every escape sequence (for border comparisons).
/// The shared frame as owned lines (the parity assertions compare strings).
fn to_vec(lines: RenderLines) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

fn editor_with(text: &str) -> Editor {
    let mut editor = Editor::new();
    editor.set_text(text);
    editor
}

#[test]
fn render_draws_a_bordered_frame_padded_to_width() {
    let mut editor = editor_with("hello");
    let lines = to_vec(editor.render(10));
    // Top rule, one content line, bottom rule.
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], "─".repeat(10));
    assert_eq!(lines[2], "─".repeat(10));
    // The cursor sits at the end: a highlighted space, then padding.
    assert_eq!(lines[1], "hello\u{1b}[7m \u{1b}[0m    ");
    for line in &lines {
        assert!(visible_width(line) <= 10, "{line:?}");
    }

    // Every rendered line keeps the exact width.
    let mut editor = editor_with("hello world this wraps");
    for width in [6usize, 12, 30] {
        for line in to_vec(editor.render(width)) {
            assert_eq!(visible_width(&line), width, "width {width}: {line:?}");
        }
    }
}

#[test]
fn render_marks_the_hardware_cursor_only_when_focused() {
    let mut editor = editor_with("abc");
    let unfocused = to_vec(editor.render(10));
    assert!(!unfocused[1].contains(CURSOR_MARKER), "{:?}", unfocused[1]);

    editor.set_focused(true);
    assert!(editor.is_focused());
    let focused = to_vec(editor.render(10));
    assert!(focused[1].contains(CURSOR_MARKER), "{:?}", focused[1]);
    assert!(
        focused[1].starts_with(&format!("abc{CURSOR_MARKER}\u{1b}[7m")),
        "{:?}",
        focused[1]
    );
}

#[test]
fn render_replaces_the_character_under_the_cursor() {
    let mut editor = editor_with("abc");
    // The cursor column addresses the grapheme it highlights, so one step
    // left from the end lands on "c" and two steps land on "b".
    editor.move_cursor(0, -1);
    let lines = to_vec(editor.render(10));
    assert_eq!(lines[1], "ab\u{1b}[7mc\u{1b}[0m       ", "{:?}", lines[1]);
    editor.move_cursor(0, -1);
    let lines = to_vec(editor.render(10));
    assert_eq!(lines[1], "a\u{1b}[7mb\u{1b}[0mc       ", "{:?}", lines[1]);
    // Replacing (not inserting) keeps the line width.
    assert_eq!(visible_width(&lines[1]), 10);

    // A wide grapheme is replaced as one unit: one step left highlights "x",
    // another highlights the two-column "日".
    let mut editor = editor_with("日x");
    editor.move_cursor(0, -1);
    let lines = to_vec(editor.render(10));
    assert_eq!(lines[1], "日\u{1b}[7mx\u{1b}[0m       ", "{:?}", lines[1]);
    editor.move_cursor(0, -1);
    let lines = to_vec(editor.render(10));
    assert_eq!(lines[1], "\u{1b}[7m日\u{1b}[0mx       ", "{:?}", lines[1]);
    assert_eq!(visible_width(&lines[1]), 10);
}

#[test]
fn render_reserves_a_column_without_padding_and_uses_padding_with_it() {
    // No padding: the last column is reserved for the cursor.
    let mut editor = editor_with("abcdefghij");
    let lines = to_vec(editor.render(8));
    // layoutWidth = 8 - 1 = 7, so the word wraps into two layout lines.
    assert_eq!(lines.len(), 4, "{lines:?}");

    // Padding 2 at width 10 → content width 6 and no reserved column.
    let mut editor = editor_with("abcdef");
    editor.set_padding_x(2);
    assert_eq!(editor.get_padding_x(), 2);
    let lines = to_vec(editor.render(10));
    assert_eq!(lines[0], "─".repeat(10));
    // The cursor overflows into the right padding, so only one pad column
    // remains and the line still fills the width exactly.
    assert_eq!(lines[1], "  abcdef\u{1b}[7m \u{1b}[0m ", "{:?}", lines[1]);
    assert_eq!(visible_width(&lines[1]), 10);

    // The padding is clamped to half the width.
    editor.set_padding_x(99);
    let lines = to_vec(editor.render(10));
    assert_eq!(visible_width(&lines[1]), 10);
    let leading = lines[1].chars().take_while(|c| *c == ' ').count();
    assert_eq!(leading, 4, "clamped to (width - 1) / 2: {:?}", lines[1]);
}

#[test]
fn render_scroll_indicators_stay_within_width_and_keep_their_color() {
    // Mirrors the upstream issue #6962 regression test.
    let width = 10;
    let mut editor = Editor::new();
    editor.set_theme(pillar_tui::editor::EditorTheme {
        border_color: Box::new(|text| format!("\u{1b}[35m{text}\u{1b}[39m")),
        select_list: SelectListTheme::default(),
    });
    let text = (0..20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.set_text(&text);

    editor.render(width);
    for _ in 0..10 {
        editor.move_cursor(-1, 0);
    }

    let lines = to_vec(editor.render(width));
    let top = lines.first().expect("top border");
    let bottom = lines.last().expect("bottom border");
    assert!(strip_ansi(top).contains("─── ↑"), "top indicator: {top:?}");
    assert!(
        strip_ansi(bottom).contains("─── ↓"),
        "bottom indicator: {bottom:?}"
    );
    // The borders carry the theme colour around the indicator.
    assert!(
        top.starts_with("\u{1b}[35m") && top.ends_with("\u{1b}[39m"),
        "{top:?}"
    );
    assert!(
        bottom.starts_with("\u{1b}[35m") && bottom.ends_with("\u{1b}[39m"),
        "{bottom:?}"
    );
    // The indicator is truncated at 10 columns (upstream slices it and
    // appends an ellipsis), so the count is not visible here.
    assert_eq!(strip_ansi(top), "─── ↑ 9...", "{top:?}");
    for line in &lines {
        assert_eq!(visible_width(line), width, "{line:?}");
    }

    // Widened, the indicator shows the hidden counts: 24 terminal rows → 7
    // visible lines, the cursor rests on line 9, so 9 lines are hidden above
    // and 4 below.
    assert_eq!(editor.scroll_offset(), 9);
    let lines = to_vec(editor.render(24));
    let top = strip_ansi(lines.first().expect("top"));
    let bottom = strip_ansi(lines.last().expect("bottom"));
    assert_eq!(
        top,
        format!("─── ↑ 9 more {}", "─".repeat(24 - 13)),
        "{top:?}"
    );
    assert_eq!(
        bottom,
        format!("─── ↓ 4 more {}", "─".repeat(24 - 13)),
        "{bottom:?}"
    );
}

#[test]
fn render_limits_visible_lines_to_thirty_percent_of_the_terminal() {
    let mut editor = Editor::new();
    editor.terminal_rows = 40;
    editor.set_text(
        &(0..40)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let lines = to_vec(editor.render(20));
    // 30% of 40 rows = 12 visible lines, plus two borders.
    assert_eq!(lines.len(), 14, "{lines:?}");

    // A short terminal never drops below five lines.
    editor.terminal_rows = 8;
    let lines = to_vec(editor.render(20));
    assert_eq!(lines.len(), 7, "{lines:?}");
}

#[test]
fn render_composites_the_autocomplete_dropdown() {
    let mut editor = editor_with("/mo");
    editor.set_padding_x(0);
    editor.set_autocomplete_max_visible(5);
    let list: SelectList = create_autocomplete_list(
        "/mo",
        &[
            (
                "/model".to_string(),
                "/model".to_string(),
                Some("pick a model".to_string()),
            ),
            ("/mode".to_string(), "/mode".to_string(), None),
        ],
        5,
    );
    assert!(editor.autocomplete_list().is_none());
    editor.set_autocomplete_list(Some(list));
    assert!(editor.autocomplete_list().is_some());

    let width = 60;
    let lines = to_vec(editor.render(width));
    // frame (border + 1 content + border) + dropdown rows.
    assert!(lines.len() > 3, "{lines:?}");
    let dropdown = &lines[3..];
    assert!(
        dropdown.iter().any(|line| line.contains("/model")),
        "{dropdown:?}"
    );
    // Descriptions only render above 40 columns (upstream's
    // MIN_DESCRIPTION_WIDTH guard).
    assert!(
        dropdown.iter().any(|line| line.contains("pick a model")),
        "{dropdown:?}"
    );
    for line in &lines {
        assert_eq!(visible_width(line), width, "{line:?}");
    }

    // The select-list theme styles the selected row.
    editor.set_theme(pillar_tui::editor::EditorTheme {
        border_color: Box::new(|text| text.to_string()),
        select_list: SelectListTheme {
            selected_text: Box::new(|text| format!("[{text}]")),
            ..SelectListTheme::default()
        },
    });
    let lines = to_vec(editor.render(width));
    // The selected row is styled as a whole (prefix + value + description).
    assert!(
        lines.iter().any(|line| line.contains("[→ /model")),
        "{:?}",
        &lines[3..]
    );

    editor.set_autocomplete_list(None);
    assert_eq!(editor.render(width).len(), 3);
}

#[test]
fn autocomplete_max_visible_is_clamped_like_upstream() {
    let mut editor = Editor::new();
    assert_eq!(editor.get_autocomplete_max_visible(), 5);
    editor.set_autocomplete_max_visible(1);
    assert_eq!(editor.get_autocomplete_max_visible(), 3);
    editor.set_autocomplete_max_visible(50);
    assert_eq!(editor.get_autocomplete_max_visible(), 20);
    editor.set_autocomplete_max_visible(8);
    assert_eq!(editor.get_autocomplete_max_visible(), 8);
}

#[test]
fn scroll_border_truncates_to_narrow_widths() {
    assert_eq!(create_scroll_border('↑', 3, 20), "─── ↑ 3 more ───────");
    // Narrow widths keep the ellipsis inside the budget.
    let narrow = create_scroll_border('↓', 12, 5);
    assert_eq!(visible_width(&narrow), 5, "{narrow:?}");
    assert!(narrow.ends_with("..."), "{narrow:?}");
    assert_eq!(create_scroll_border('↑', 1, 0), "");
}

#[test]
fn the_editor_is_a_focusable_component() {
    let mut editor = editor_with("hi");
    // Component::render delegates to the editor render.
    assert_eq!(Component::render(&mut editor, 10).len(), 3);
    assert!(editor.as_focusable().is_some());
    let focused = editor.as_focusable().expect("focus interface");
    focused.set_focused(true);
    assert!(editor.focused);
    assert!(Focusable::is_focused(&editor));
    // `invalidate` has no cached state to drop.
    editor.invalidate();
    assert_eq!(editor.render(10).len(), 3);
    // SegmentMode stays available for hosts building layout data.
    let _ = SegmentMode::Word;
}
