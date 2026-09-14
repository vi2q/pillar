//! Port of packages/coding-agent/src/core/tools/render-utils.ts (pi
//! v0.84.3), the pure-logic half: tool output text shaping for rendering.
//!
//! divergence: `getTextOutput` renders a generic image indicator instead of
//! the driver-provided `imageFallback` (no image-dimension probe here), and
//! `linkPath`'s URL is built without percent-encoding.

use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::truncate::{sanitize_binary_output, strip_ansi};
use crate::modes::interactive::theme::Theme;
use pillar_tui::terminal_image::{get_capabilities, hyperlink};

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


/// The theme's error colour around an invalid argument note (upstream
/// `invalidArgText`).
pub fn invalid_arg_text(theme: &Theme) -> String {
    theme.fg("error", "[invalid arg]")
}

/// Wrap styled text in an OSC 8 link to the resolved path, when the terminal
/// supports hyperlinks (upstream `linkPath`).
pub fn link_path(styled_text: &str, raw_path: &str, cwd: &str) -> String {
    if !get_capabilities().hyperlinks {
        return styled_text.to_string();
    }
    let absolute = resolve_to_cwd(raw_path, cwd);
    hyperlink(styled_text, &format!("file://{}", absolute.to_string_lossy()))
}

/// Render a tool's path argument: invalid args, the empty fallback, an
/// ellipsis placeholder, else the linked shortened path (upstream
/// `renderToolPath`).
pub fn render_tool_path(
    raw_path: Option<&str>,
    theme: &Theme,
    cwd: &str,
    empty_fallback: Option<&str>,
) -> String {
    let Some(raw_path) = raw_path else {
        return invalid_arg_text(theme);
    };
    let value = if raw_path.is_empty() {
        empty_fallback
    } else {
        Some(raw_path)
    };
    let Some(value) = value else {
        return theme.fg("toolOutput", "...");
    };
    link_path(&theme.fg("accent", &shorten_path(value)), value, cwd)
}

/// Drop trailing empty lines (upstream `trimTrailingEmptyLines`).
pub fn trim_trailing_empty_lines(lines: &[String]) -> Vec<String> {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    lines[..end].to_vec()
}

/// Path relative to the cwd when it is inside it, absolute otherwise, with
/// POSIX separators (upstream `formatPathRelativeToCwdOrAbsolute`).
pub fn format_path_relative_to_cwd_or_absolute(file_path: &str, cwd: &str) -> String {
    let absolute = resolve_to_cwd(file_path, cwd);
    let cwd_path = std::path::Path::new(cwd);
    let relative = absolute.strip_prefix(cwd_path).ok().map(|rest| {
        rest.to_string_lossy().to_string()
    });
    let display = relative.unwrap_or_else(|| absolute.to_string_lossy().to_string());
    display.replace(std::path::MAIN_SEPARATOR, "/")
}

/// The package root the docs detection uses (upstream `getPackageDir`), and
/// [`get_readme_path`] for the `read` tool's compact classification.
///
/// divergence: the port has no bundled package layout, so the directory comes
/// from `PI_PACKAGE_DIR` or next to the executable.
pub fn package_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("PI_PACKAGE_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The package's README path (upstream `config.ts::getReadmePath`).
pub fn get_readme_path() -> std::path::PathBuf {
    package_dir().join("README.md")
}
