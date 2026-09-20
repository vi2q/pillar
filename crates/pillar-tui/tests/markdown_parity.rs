//! Parity tests for the markdown renderer (pi v0.84.3
//! components/markdown.ts). Theme fns here are plain identity wrappers
//! plus ANSI codes where styling matters.

use pillar_tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};

fn theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(passthrough),
        link: Box::new(passthrough),
        link_url: Box::new(passthrough),
        code: Box::new(passthrough),
        code_block: Box::new(passthrough),
        code_block_border: Box::new(passthrough),
        quote: Box::new(passthrough),
        quote_border: Box::new(passthrough),
        hr: Box::new(passthrough),
        list_bullet: Box::new(passthrough),
        bold: Box::new(passthrough),
        italic: Box::new(passthrough),
        strikethrough: Box::new(passthrough),
        underline: Box::new(passthrough),
        highlight_code: None,
        code_block_indent: None,
    }
}

fn passthrough(text: &str) -> String {
    text.to_string()
}

fn opts() -> MarkdownOptions {
    MarkdownOptions {
        render_latex: true,
        ..MarkdownOptions::default()
    }
}

fn render(text: &str, width: usize) -> Vec<String> {
    let mut md = Markdown::new(text, 0, 0, theme(), None, opts());
    md.render(width).iter().map(|line| line.to_string()).collect()
}

fn plain(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            pillar_tui::text_utils::strip_terminal_sequences(l)
                .trim_end()
                .to_string()
        })
        .collect()
}

// --- paragraphs / headings ---------------------------------------------------------------------

#[test]
fn paragraph_renders_text() {
    let lines = render("hello world", 40);
    let p = plain(&lines);
    assert!(p[0].contains("hello world"), "{p:?}");
}

#[test]
fn heading_adds_prefix_at_level_3_plus() {
    let lines = render("### Small heading", 40);
    let p = plain(&lines);
    assert!(p[0].contains("### Small heading"), "{p:?}");
}

#[test]
fn heading_no_prefix_below_level_3() {
    let lines = render("# Big heading", 40);
    let p = plain(&lines);
    assert!(p[0].contains("Big heading"), "{p:?}");
    assert!(!p[0].contains("# Big"), "{p:?}");
}

#[test]
fn paragraph_spacing_before_next_block() {
    let lines = render("first\n\nsecond", 40);
    let p = plain(&lines);
    assert_eq!(p.len(), 3, "{p:?}");
    assert_eq!(p[1], "");
}

// --- inline formatting ---------------------------------------------------------------------------

#[test]
fn bold_renders() {
    let lines = render("**bold text**", 40);
    assert!(lines[0].contains("bold text"), "{:?}", lines[0]);
}

#[test]
fn codespan_renders() {
    let lines = render("run `npm test` now", 40);
    let p = plain(&lines);
    assert!(p[0].contains("npm test"), "{p:?}");
}

#[test]
fn strikethrough_renders() {
    let lines = render("~~gone~~", 40);
    assert!(lines[0].contains("gone"), "{:?}", lines[0]);
}

#[test]
fn link_renders_with_url_fallback() {
    let lines = render("[text](https://example.com)", 40);
    let p = plain(&lines);
    assert!(p[0].contains("text"), "{p:?}");
    assert!(p[0].contains("(https://example.com)"), "{p:?}");
}

#[test]
fn link_matching_text_omits_url() {
    let lines = render("<https://example.com>", 40);
    let p = plain(&lines);
    assert!(p[0].contains("https://example.com"), "{p:?}");
    assert!(!p[0].contains("(("), "{p:?}");
}

// --- code blocks ----------------------------------------------------------------------------------

#[test]
fn code_block_renders_with_fences_and_indent() {
    let lines = render("```js\nconsole.log(1);\n```", 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("```js"), "{p:?}");
    assert!(p[1].starts_with("  console.log"), "{p:?}");
    assert_eq!(p[2], "```");
}

#[test]
fn code_block_custom_indent() {
    let mut theme = theme();
    theme.code_block_indent = Some("    ".to_string());
    let mut md = Markdown::new("```\ncode\n```", 0, 0, theme, None, opts());
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[1].starts_with("    code"), "{p:?}");
}

#[test]
fn code_block_highlight_hook() {
    let mut theme = theme();
    theme.highlight_code = Some(Box::new(|code, lang| {
        code.split('\n').map(|l| format!("[{lang:?}]{l}")).collect()
    }));
    let mut md = Markdown::new("```rust\nfn main() {}\n```", 0, 0, theme, None, opts());
    let lines: Vec<String> = md.render(60).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[1].contains("[Some(\"rust\")]fn main() {}"), "{p:?}");
}

#[test]
fn streaming_partial_closing_fence_does_not_shrink() {
    let full = render("```js\nlet x = 1;\n```", 40);
    let partial = render("```js\nlet x = 1;\n``", 40);
    // The partial closing fence is trimmed so both render the same.
    assert_eq!(
        plain(&full),
        plain(&partial),
        "full={full:?} partial={partial:?}"
    );
}

// --- lists -----------------------------------------------------------------------------------------

#[test]
fn unordered_list_renders_bullets() {
    let lines = render("- one\n- two", 40);
    let p = plain(&lines);
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p[0].starts_with("- one"), "{p:?}");
    assert!(p[1].starts_with("- two"), "{p:?}");
}

#[test]
fn ordered_list_numbers_items() {
    let lines = render("1. first\n2. second", 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("1. first"), "{p:?}");
    assert!(p[1].starts_with("2. second"), "{p:?}");
}

#[test]
fn ordered_list_start_number() {
    let lines = render("5. fifth\n6. sixth", 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("5. fifth"), "{p:?}");
    assert!(p[1].starts_with("6. sixth"), "{p:?}");
}

#[test]
fn preserve_ordered_list_markers_option() {
    let options = MarkdownOptions {
        preserve_ordered_list_markers: true,
        ..opts()
    };
    let mut md = Markdown::new("1. first\n2. second", 0, 0, theme(), None, options);
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[0].starts_with("1. first"), "{p:?}");
}

#[test]
fn nested_list_indents() {
    let lines = render("- parent\n  - child", 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("- parent"), "{p:?}");
    assert!(p[1].starts_with("    - child"), "{p:?}");
}

#[test]
fn task_list_markers() {
    let lines = render("- [x] done\n- [ ] todo", 40);
    let p = plain(&lines);
    assert!(p[0].contains("[x] done"), "{p:?}");
    assert!(p[1].contains("[ ] todo"), "{p:?}");
}

#[test]
fn list_item_wraps_with_continuation_indent() {
    let lines = render(
        "- a very long list item that should wrap onto a second line here",
        20,
    );
    let p = plain(&lines);
    assert!(p.len() >= 2, "{p:?}");
    // Continuation lines align under the item text.
    assert!(p[1].starts_with("  "), "{p:?}");
}

// --- blockquotes -------------------------------------------------------------------------------------

#[test]
fn blockquote_adds_border() {
    let lines = render("> quoted text", 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("│ quoted"), "{p:?}");
}

#[test]
fn blockquote_multi_paragraph() {
    let lines = render("> first\n>\n> second", 40);
    let p = plain(&lines);
    assert!(p.iter().any(|l| l.contains("first")), "{p:?}");
    assert!(p.iter().any(|l| l.contains("second")), "{p:?}");
}

// --- hr / html ------------------------------------------------------------------------------------

#[test]
fn hr_renders_dashes() {
    let lines = render("---", 40);
    let p = plain(&lines);
    assert!(p[0].contains("─"), "{p:?}");
}

#[test]
fn hr_length_is_min_width_80() {
    let lines = render("---", 100);
    let p = plain(&lines);
    let dashes = p[0].chars().filter(|c| *c == '─').count();
    assert_eq!(dashes, 80, "{p:?}");
}

// --- tables -----------------------------------------------------------------------------------------

#[test]
fn table_renders_borders_and_columns() {
    let table = "| A | B |\n|---|---|\n| 1 | 2 |";
    let lines = render(table, 40);
    let p = plain(&lines);
    assert!(p[0].starts_with("┌"), "{p:?}");
    assert!(p.iter().any(|l| l.starts_with("├")), "{p:?}");
    assert!(p.last().unwrap().starts_with("└"), "{p:?}");
    assert!(p.iter().any(|l| l.contains("│ 1 │ 2")), "{p:?}");
}

#[test]
fn table_header_is_bold() {
    let table = "| A | B |\n|---|---|\n| 1 | 2 |";
    let mut theme = theme();
    theme.bold = Box::new(|t| format!("<b>{t}</b>"));
    let mut md = Markdown::new(table, 0, 0, theme, None, opts());
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("<b>A   </b>") || l.contains("<b>A</b>")),
        "{lines:?}"
    );
}

#[test]
fn table_too_narrow_falls_back_to_raw() {
    let table = "| A | B |\n|---|---|\n| 1 | 2 |";
    // width 6 is too narrow for a 2-col table (3n+1=7 overhead).
    let lines = render(table, 6);
    let p = plain(&lines);
    assert!(!p[0].starts_with("┌"), "{p:?}");
}

// --- latex -------------------------------------------------------------------------------------------

#[test]
fn inline_latex_renders() {
    let lines = render("value $x^2$ here", 40);
    let p = plain(&lines);
    assert!(p[0].contains("x"), "{p:?}");
}

#[test]
fn latex_block_renders() {
    let lines = render("$$\nx^2 + y^2\n$$", 40);
    let p = plain(&lines);
    assert!(!p.is_empty(), "{p:?}");
}

#[test]
fn latex_disabled_option_passes_through() {
    let options = MarkdownOptions {
        render_latex: false,
        ..opts()
    };
    let mut md = Markdown::new("value $x$ here", 0, 0, theme(), None, options);
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[0].contains("$x$"), "{p:?}");
}

// --- padding / background / wrapping ------------------------------------------------------------------

#[test]
fn horizontal_padding_wraps_content() {
    let mut md = Markdown::new("hello", 2, 0, theme(), None, opts());
    let lines: Vec<String> = md.render(20).iter().map(|line| line.to_string()).collect();
    assert!(lines[0].starts_with("  hello"), "{:?}", lines[0]);
    // Padded to full width with the right margin.
    assert_eq!(pillar_tui::text_utils::visible_width(&lines[0]), 20);
}

#[test]
fn vertical_padding_adds_empty_lines() {
    let mut md = Markdown::new("hello", 0, 2, theme(), None, opts());
    let lines: Vec<String> = md.render(20).iter().map(|line| line.to_string()).collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0], " ".repeat(20));
    assert_eq!(lines[4], " ".repeat(20));
}

#[test]
fn lines_padded_to_full_width() {
    let lines = render("hello", 20);
    assert_eq!(pillar_tui::text_utils::visible_width(&lines[0]), 20);
}

#[test]
fn background_applied_to_content_and_padding() {
    let style = DefaultTextStyle {
        bg_color: Some(Box::new(|t| format!("[bg]{t}[/bg]"))),
        ..DefaultTextStyle::default()
    };
    let mut md = Markdown::new("hi", 1, 1, theme(), Some(style), opts());
    let lines: Vec<String> = md.render(10).iter().map(|line| line.to_string()).collect();
    assert!(lines.iter().all(|l| l.contains("[bg]")), "{lines:?}");
}

#[test]
fn long_text_wraps_to_width() {
    let lines = render("word ".repeat(20).trim(), 20);
    let p = plain(&lines);
    assert!(p.len() > 1, "{p:?}");
    for line in &p {
        assert!(pillar_tui::text_utils::visible_width(line) <= 20, "{p:?}");
    }
}

#[test]
fn empty_input_renders_nothing() {
    let lines = render("   \n  ", 40);
    assert!(lines.is_empty(), "{lines:?}");
}

// --- caching -----------------------------------------------------------------------------------------

#[test]
fn render_cache_hits_same_width() {
    let mut md = Markdown::new("cached", 0, 0, theme(), None, opts());
    let first = md.render(40);
    let second = md.render(40);
    assert_eq!(first, second);
}

#[test]
fn set_text_invalidates_cache() {
    let mut md = Markdown::new("old", 0, 0, theme(), None, opts());
    let _ = md.render(40);
    md.set_text("new");
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[0].contains("new"), "{p:?}");
}

// --- transform hook -----------------------------------------------------------------------------------

#[test]
fn transform_hook_receives_content_width() {
    let options = MarkdownOptions {
        transform: Some(Box::new(|text, width| format!("{width}::{text}"))),
        ..opts()
    };
    let mut md = Markdown::new("body", 0, 0, theme(), None, options);
    let lines: Vec<String> = md.render(50).iter().map(|line| line.to_string()).collect();
    let p = plain(&lines);
    assert!(p[0].contains("50::body"), "{p:?}");
}

// --- default text style ---------------------------------------------------------------------------------

#[test]
fn default_style_applied_to_paragraphs() {
    let style = DefaultTextStyle {
        color: Some(Box::new(|t| format!("[c]{t}[/c]"))),
        ..DefaultTextStyle::default()
    };
    let mut md = Markdown::new("plain", 0, 0, theme(), Some(style), opts());
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    assert!(lines[0].contains("[c]plain"), "{:?}", lines[0]);
}

#[test]
fn default_style_not_applied_inside_blockquote() {
    let style = DefaultTextStyle {
        color: Some(Box::new(|t| format!("[c]{t}[/c]"))),
        ..DefaultTextStyle::default()
    };
    let mut md = Markdown::new("> quoted", 0, 0, theme(), Some(style), opts());
    let lines: Vec<String> = md.render(40).iter().map(|line| line.to_string()).collect();
    // Quote line contains the text but not the default color wrap.
    assert!(!lines[0].contains("[c]"), "{:?}", lines[0]);
}
