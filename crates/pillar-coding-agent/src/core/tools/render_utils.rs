//! Port of packages/coding-agent/src/core/tools/render-utils.ts (pi
//! v0.84.3), the pure-logic half: tool output text shaping for rendering.
//!
//! divergence: the theme/hyperlink/capability-dependent helpers (`linkPath`,
//! `invalidArgText`, `renderToolPath`) depend on the TUI theme system and
//! are not ported; the port covers string shaping and `getTextOutput`
//! without image dimension fallbacks (images render a generic indicator).

use crate::core::truncate::{sanitize_binary_output, strip_ansi};

/// Shorten an absolute path under the home directory to `~/...` (upstream
/// `shortenPath`).
pub fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Coerce an unknown tool argument to a string (upstream `str`): strings
/// pass through, null/undefined become empty, anything else is None.
pub fn coerce_str(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        None | Some(serde_json::Value::Null) => Some(String::new()),
        _ => None,
    }
}

/// Replace tabs with three spaces (upstream `replaceTabs`).
pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// Remove carriage returns from display text (upstream
/// `normalizeDisplayText`).
pub fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

/// One content block of a tool result (upstream the TextContent |
/// ImageContent union subset used by getTextOutput).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultBlock {
    Text(String),
    Image { data: String, mime_type: String },
}

/// Shape a tool result for display (upstream `getTextOutput`): sanitized
/// text blocks joined by newlines; image blocks append a generic indicator
/// line when the terminal cannot show them.
pub fn get_text_output(blocks: &[ToolResultBlock], show_images: bool) -> String {
    let text_blocks: Vec<&ToolResultBlock> = blocks
        .iter()
        .filter(|b| matches!(b, ToolResultBlock::Text(_)))
        .collect();
    let image_count = blocks
        .iter()
        .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
        .count();

    let output = text_blocks
        .iter()
        .map(|block| match block {
            ToolResultBlock::Text(text) => {
                normalize_display_text(&sanitize_binary_output(&strip_ansi(text)))
            }
            _ => unreachable!("filtered"),
        })
        .collect::<Vec<_>>()
        .join("\n");

    // The port's capability probe: images shown only when explicitly enabled.
    let caps_support_images = false;
    if image_count > 0 && (!caps_support_images || !show_images) {
        let mut indicators = Vec::new();
        for block in blocks {
            if let ToolResultBlock::Image { mime_type, .. } = block {
                indicators.push(format!("[image: {mime_type}]"));
            }
        }
        let joined = indicators.join("\n");
        return if output.is_empty() {
            joined
        } else {
            format!("{output}\n{joined}")
        };
    }

    output
}
