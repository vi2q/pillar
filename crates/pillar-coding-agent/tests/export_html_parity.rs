//! Parity tests for export-html/ansi-to-html.ts (pi v0.84.3): ANSI escape
//! code to HTML conversion — colors, styles, and reset semantics.

use pillar_coding_agent::core::export_html::ansi_to_html::{ansi_lines_to_html, ansi_to_html};

// --- basic styling -----------------------------------------------------------

#[test]
fn plain_text_is_escaped_without_spans() {
    let html = ansi_to_html("a < b & c \" d ' e");
    assert_eq!(html, "a &lt; b &amp; c &quot; d &#039; e");
    assert!(!html.contains("<span"));
}

#[test]
fn bold_and_color_codes_produce_inline_styles() {
    let html = ansi_to_html("\x1b[1;31mred bold\x1b[0m plain");
    assert!(
        html.contains("<span style=\"color:#800000;font-weight:bold\">"),
        "{html}"
    );
    // Reset closes the span; plain text follows unstyled.
    assert!(html.contains("</span> plain"));
}

#[test]
fn standard_foreground_colors_map_to_palette() {
    assert_eq!(
        ansi_to_html("\x1b[32mgreen"),
        "<span style=\"color:#008000\">green</span>"
    );
    // Bright variants (90-97).
    assert_eq!(
        ansi_to_html("\x1b[92mbright green"),
        "<span style=\"color:#00ff00\">bright green</span>"
    );
}

#[test]
fn background_codes_and_defaults() {
    let html = ansi_to_html("\x1b[41mbg\x1b[49mreset-bg");
    assert_eq!(
        html,
        "<span style=\"background-color:#800000\">bg</span>reset-bg"
    );
}

#[test]
fn dim_italic_underline() {
    let html = ansi_to_html("\x1b[2;3;4mstyled\x1b[0m");
    assert!(html.contains("opacity:0.6"), "{html}");
    assert!(html.contains("font-style:italic"), "{html}");
    assert!(html.contains("text-decoration:underline"), "{html}");
}

#[test]
fn color_256_palette() {
    // 16: cube color (0,0,0) -> #000000
    let html = ansi_to_html("\x1b[38;5;16m");
    assert!(html.contains("color:#000000"), "{html}");
    // 244: grayscale 8 + 12*10 = 128 -> #808080
    let html = ansi_to_html("\x1b[38;5;244m");
    assert!(html.contains("color:#808080"), "{html}");
}

#[test]
fn rgb_true_color() {
    let html = ansi_to_html("\x1b[38;2;10;20;30m");
    assert!(html.contains("color:rgb(10,20,30)"), "{html}");
    let html = ansi_to_html("\x1b[48;2;1;2;3m");
    assert!(html.contains("background-color:rgb(1,2,3)"), "{html}");
}

#[test]
fn empty_parameter_body_is_reset() {
    // ESC[m is an implicit reset.
    let html = ansi_to_html("\x1b[m");
    assert_eq!(html, "");
}

#[test]
fn unrecognized_codes_are_ignored() {
    assert_eq!(ansi_to_html("\x1b[99mx"), "x");
}

#[test]
fn ansi_lines_to_html_wraps_divs_and_blank_lines() {
    let lines = vec!["\x1b[1mbold".to_string(), String::new()];
    let html = ansi_lines_to_html(&lines);
    assert!(
        html.contains(
            "<div class=\"ansi-line\"><span style=\"font-weight:bold\">bold</span></div>"
        ),
        "{html}"
    );
    assert!(
        html.contains("<div class=\"ansi-line\">&nbsp;</div>"),
        "{html}"
    );
}
