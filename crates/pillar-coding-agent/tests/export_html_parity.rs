//! Parity tests for export-html (pi v0.84.3): ANSI→HTML conversion, color
//! derivation helpers, session data embedding, and template substitution.

use pillar_coding_agent::core::export_html::ansi_to_html::{ansi_lines_to_html, ansi_to_html};
use pillar_coding_agent::core::export_html::{
    ExportColors, ExportOptions, RenderedToolHtml, SessionData, TEMPLATE_RENDERED_TOOLS,
    adjust_brightness, default_output_path, derive_export_colors, generate_html, get_luminance,
    parse_color,
};
use std::collections::BTreeMap;

// --- ansiToHtml ------------------------------------------------------------------

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
    // Reset closes the span; plain text follows unstyled (inside the same
    // trailing segment, after the closing span tag).
    assert!(html.contains("</span> plain"), "{html}");
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
fn reset_codes_22_23_24_clear_partial_styles() {
    let html = ansi_to_html("\x1b[1;3;4mx\x1b[22;23;24my");
    // Bold/dim/italic/underline cleared; no span remains styled with them.
    let second_span = html.split("</span>").nth(1).unwrap();
    assert!(!second_span.contains("font-weight"), "{html}");
}

#[test]
fn color_256_palette() {
    // 16: cube color (0,0,0) -> #000000
    let html = ansi_to_html("\x1b[38;5;16m");
    assert!(html.contains("color:#000000"), "{html}");
    // 196: cube (5,0,0) -> r=55+5*40=255
    let html = ansi_to_html("\x1b[38;5;196m");
    assert!(html.contains("color:#ff0000"), "{html}");
    // 160: cube (4,0,0) -> r=55+4*40=215=0xd7
    let html = ansi_to_html("\x1b[38;5;160m");
    assert!(html.contains("color:#d70000"), "{html}");
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
    let html = ansi_to_html("\x1b[1mx\x1b[my");
    assert!(html.starts_with("<span"), "{html}");
    assert!(html.ends_with("</span>y"), "{html}");
}

#[test]
fn unrecognized_codes_are_ignored() {
    // Codes outside the handled ranges don't panic or emit spans.
    assert_eq!(ansi_to_html("\x1b[99mx"), "x");
    assert_eq!(ansi_to_html("\x1b[5mx"), "x");
}

#[test]
fn style_changes_close_and_reopen_spans() {
    let html = ansi_to_html("\x1b[31mred\x1b[32mgreen");
    assert!(html.contains("color:#800000"), "{html}");
    assert!(html.contains("color:#008000"), "{html}");
    assert_eq!(html.matches("</span><span").count(), 1, "{html}");
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

// --- color helpers -----------------------------------------------------------------

#[test]
fn parse_color_hex_and_rgb() {
    assert_eq!(parse_color("#343541"), Some((0x34, 0x35, 0x41)));
    assert_eq!(parse_color("rgb(10, 20, 30)"), Some((10, 20, 30)));
    assert_eq!(parse_color("rgb(300,0,0)"), None);
    assert_eq!(parse_color("#123"), None);
    assert_eq!(parse_color("nope"), None);
}

#[test]
fn luminance_light_and_dark() {
    // White is maximally light.
    assert!(get_luminance(255, 255, 255) > 0.5);
    // Black is maximally dark.
    assert!(get_luminance(0, 0, 0) < 0.5);
}

#[test]
fn adjust_brightness_clamps_and_passes_through() {
    assert_eq!(
        adjust_brightness("rgb(100, 100, 100)", 0.5),
        "rgb(50, 50, 50)"
    );
    assert_eq!(
        adjust_brightness("rgb(250, 250, 250)", 2.0),
        "rgb(255, 255, 255)"
    );
    // Unparseable colors pass through.
    assert_eq!(adjust_brightness("nope", 2.0), "nope");
}

#[test]
fn derive_export_colors_dark_path() {
    let colors = derive_export_colors("rgb(52, 53, 65)"); // dark base
    assert!(colors.page_bg.contains("rgb("), "{colors:?}");
    // page_bg darker than card_bg.
    assert_ne!(colors.page_bg, colors.card_bg);
    assert!(colors.info_bg.starts_with("rgb(72, 68, 65)"), "{colors:?}");
}

#[test]
fn derive_export_colors_light_path_and_fallback() {
    let colors = derive_export_colors("rgb(255, 255, 255)");
    assert_eq!(colors.card_bg, "rgb(255, 255, 255)");
    let colors = derive_export_colors("nope");
    assert_eq!(colors, ExportColors::default());
}

// --- template + session data ---------------------------------------------------------

#[test]
fn template_rendered_tools_match_upstream() {
    assert_eq!(
        TEMPLATE_RENDERED_TOOLS,
        ["bash", "read", "write", "edit", "ls"]
    );
}

#[test]
fn generate_html_embeds_session_data_and_fills_placeholders() {
    let session_data = SessionData {
        header: Some(serde_json::json!({"id": "sess-1", "cwd": "/tmp"})),
        entries: vec![serde_json::json!({"type": "message"})],
        leaf_id: Some("abc".to_string()),
        system_prompt: Some("be helpful".to_string()),
        tools: None,
        rendered_tools: None,
    };
    let html = generate_html(&session_data, None).unwrap();
    assert!(!html.contains("{{CSS}}"), "CSS placeholder substituted");
    assert!(!html.contains("{{JS}}"), "JS placeholder substituted");
    assert!(
        !html.contains("{{SESSION_DATA}}"),
        "session data substituted"
    );
    assert!(!html.contains("{{MARKED_JS}}"), "marked substituted");
    assert!(!html.contains("{{HIGHLIGHT_JS}}"), "highlight substituted");
    assert!(!html.contains("{{BODY_BG}}"), "bg substituted");
    assert!(
        html.contains("<!DOCTYPE html>") || html.contains("<html"),
        "{html}"
    );
    // The base64 payload sits inside the session-data script tag.
    let b64 = extract_session_payload(&html);
    let decoded = decode_base64(&b64);
    let parsed: serde_json::Value = serde_json::from_str(&decoded).unwrap();
    assert_eq!(parsed["header"]["id"], "sess-1");
    assert_eq!(parsed["leafId"], "abc");
    assert_eq!(parsed["systemPrompt"], "be helpful");
}

/// Extract the base64 payload between the session-data script tag bounds.
fn extract_session_payload(html: &str) -> String {
    let marker = "id=\"session-data\" type=\"application/json\">";
    let start = html.find(marker).expect("session-data tag") + marker.len();
    let end = html[start..].find('<').expect("closing tag") + start;
    html[start..end].trim().to_string()
}

/// Minimal base64 decoder for test verification.
fn decode_base64(input: &str) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = Vec::new();
    let cleaned: Vec<char> = input.chars().filter(|c| *c != '=').collect();
    for chunk in cleaned.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, c)| {
            let v = table.iter().position(|t| *t as char == *c).unwrap() as u32;
            acc | (v << (18 - 6 * i))
        });
        bytes.push((n >> 16) as u8);
        if chunk.len() > 2 {
            bytes.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            bytes.push(n as u8);
        }
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn generate_html_with_explicit_colors() {
    let colors = ExportColors {
        page_bg: "rgb(1, 2, 3)".to_string(),
        card_bg: "rgb(4, 5, 6)".to_string(),
        info_bg: "rgb(7, 8, 9)".to_string(),
    };
    let html = generate_html(&SessionData::default(), Some(&colors)).unwrap();
    assert!(html.contains("rgb(1, 2, 3)"), "{html}");
    assert!(html.contains("rgb(4, 5, 6)"), "{html}");
    assert!(html.contains("rgb(7, 8, 9)"), "{html}");
}

#[test]
fn rendered_tool_html_serializes_with_camel_case() {
    let mut rendered = BTreeMap::new();
    rendered.insert(
        "call-1".to_string(),
        RenderedToolHtml {
            call_html: Some("<b>call</b>".to_string()),
            result_html_collapsed: Some("<i>collapsed</i>".to_string()),
            result_html_expanded: None,
        },
    );
    let session_data = SessionData {
        rendered_tools: Some(rendered),
        ..Default::default()
    };
    let html = generate_html(&session_data, None).unwrap();
    let decoded = decode_base64(&extract_session_payload(&html));
    assert!(decoded.contains("callHtml"), "{decoded}");
    assert!(decoded.contains("resultHtmlCollapsed"), "{decoded}");
    // Absent fields are omitted.
    assert!(!decoded.contains("resultHtmlExpanded"), "{decoded}");
}

#[test]
fn default_output_path_uses_session_basename() {
    let path = default_output_path(std::path::Path::new("/tmp/2026-01-01_abcd.jsonl"));
    assert_eq!(
        path,
        std::path::PathBuf::from("pi-session-2026-01-01_abcd.html")
    );
}

#[test]
fn from_parts_serializes_entries() {
    use pillar_coding_agent::core::session_entries::{SessionEntryBase, SessionMessageEntry};
    let entry =
        pillar_coding_agent::core::session_entries::SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "e1".to_string(),
                parent_id: None,
                timestamp: 1000,
            },
            message: pillar_coding_agent::core::messages::CodingAgentMessage::Base(
                pillar_ai::types::Message::User {
                    content: pillar_ai::types::UserContent::Text("hello".to_string()),
                    timestamp: 1000,
                },
            ),
        });
    let data = SessionData::from_parts(None, &[entry], None, None, None, None);
    assert_eq!(data.entries.len(), 1);
    assert_eq!(data.entries[0]["type"], "message");
    assert_eq!(data.entries[0]["id"], "e1");
    let _ = ExportOptions::default();
}
