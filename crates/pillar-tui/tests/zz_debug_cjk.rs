//! Temporary reproduction harness for the CJK + inline-code rendering bug.

use pillar_tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use pillar_tui::text_utils::{strip_terminal_sequences, visible_width, wrap_text_with_ansi};

fn passthrough(text: &str) -> String {
    text.to_string()
}

fn ansi_code(text: &str) -> String {
    format!("\x1b[36m{text}\x1b[39m")
}

fn ansi_bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

fn theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(passthrough),
        link: Box::new(passthrough),
        link_url: Box::new(passthrough),
        code: Box::new(ansi_code),
        code_block: Box::new(passthrough),
        code_block_border: Box::new(passthrough),
        quote: Box::new(passthrough),
        quote_border: Box::new(passthrough),
        hr: Box::new(passthrough),
        list_bullet: Box::new(passthrough),
        bold: Box::new(ansi_bold),
        italic: Box::new(passthrough),
        strikethrough: Box::new(passthrough),
        underline: Box::new(passthrough),
        highlight_code: None,
        code_block_indent: None,
    }
}

fn default_style() -> DefaultTextStyle {
    DefaultTextStyle {
        color: Some(Box::new(|t| format!("\x1b[37m{t}\x1b[39m"))),
        ..DefaultTextStyle::default()
    }
}

fn render(text: &str, width: usize) -> Vec<String> {
    let mut md = Markdown::new(
        text,
        0,
        0,
        theme(),
        Some(default_style()),
        MarkdownOptions::default(),
    );
    md.render(width)
}

#[test]
fn wrap_preserves_cjk_and_ansi_code() {
    // Simulates a line: default-styled CJK + an ANSI-styled inline code span.
    let line = "\x1b[37m\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}\u{672b}\u{5c3e} chat \x1b[36mContainer\x1b[39m \u{304c}\u{30ad}\u{30e3}\u{30c3}\u{30b7}\u{30e5}\x1b[39m";
    for width in 8..40 {
        let wrapped = wrap_text_with_ansi(line, width);
        let joined: String = wrapped
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect::<Vec<_>>()
            .join("");
        let joined = joined.replace(' ', "");
        assert!(
            joined.contains("Container"),
            "width={width}: inline code lost; got {wrapped:?}"
        );
        for l in &wrapped {
            assert!(
                visible_width(l) <= width,
                "width={width}: line exceeds width: {l:?} ({} cols)",
                visible_width(l)
            );
        }
    }
}

#[test]
fn markdown_preserves_inline_code_with_cjk() {
    let text = "\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}/\u{672b}\u{5c3e}\u{3060}\u{3051}\u{5909}\u{5316}\u{306e}\u{3068}\u{304d} chat `Container` \u{304c}\u{30ad}\u{30e3}\u{30c3}\u{30b7}\u{30e5}\u{3092}\u{8fd4}\u{3059}\u{ff08}clone \u{3092} `Arc<[String]>` \u{5316}\u{3057}\u{3066}\u{6d88}\u{3059}\u{ff09}\u{3002}";
    for width in 10..60 {
        let lines = render(text, width);
        let joined: String = lines
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect::<Vec<_>>()
            .join("");
        let joined = joined.replace(' ', "");
        assert!(
            joined.contains("Container"),
            "width={width}: `Container` lost; got {joined:?}"
        );
        assert!(
            joined.contains("Arc<[String]>"),
            "width={width}: `Arc` lost; got {joined:?}"
        );
    }
}
