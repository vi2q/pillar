//! Port of packages/coding-agent/src/core/export-html/index.ts (pi v0.84.3):
//! session export to a self-contained HTML file. The template assets
//! (template.html/css/js plus the vendored marked/highlight scripts) ship
//! with the crate and are substituted into the final document.
//!
//! divergence: upstream derives export colors from the interactive theme
//! registry (`getResolvedThemeColors`); the port accepts explicit export
//! colors or falls back to the upstream dark defaults. The tool
//! pre-rendering hook (`preRenderCustomTools` + TUI renderer bridge) is
//! provided as a data-shaped `RenderedTools` map supplied by the caller.

pub mod ansi_to_html;

pub use ansi_to_html::{ansi_lines_to_html, ansi_to_html};

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::session_entries::SessionEntry;

/// Pre-rendered HTML for a custom tool call and result (upstream
/// `RenderedToolHtml`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct RenderedToolHtml {
    #[serde(rename = "callHtml", skip_serializing_if = "Option::is_none")]
    pub call_html: Option<String>,
    #[serde(
        rename = "resultHtmlCollapsed",
        skip_serializing_if = "Option::is_none"
    )]
    pub result_html_collapsed: Option<String>,
    #[serde(rename = "resultHtmlExpanded", skip_serializing_if = "Option::is_none")]
    pub result_html_expanded: Option<String>,
}

/// Export options (upstream `ExportOptions`).
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub output_path: Option<PathBuf>,
    /// Export color derivation source (upstream derives these from the
    /// theme; the port takes them explicitly or uses the dark defaults).
    pub export_colors: Option<ExportColors>,
    /// Pre-rendered HTML for custom tool calls/results keyed by tool call id.
    pub rendered_tools: Option<std::collections::BTreeMap<String, RenderedToolHtml>>,
}

/// Export background colors (upstream `getThemeExportColors` shape).
#[derive(Debug, Clone, PartialEq)]
pub struct ExportColors {
    pub page_bg: String,
    pub card_bg: String,
    pub info_bg: String,
}

impl Default for ExportColors {
    fn default() -> Self {
        // Upstream derived dark fallbacks.
        Self {
            page_bg: "rgb(24, 24, 30)".to_string(),
            card_bg: "rgb(30, 30, 36)".to_string(),
            info_bg: "rgb(60, 55, 40)".to_string(),
        }
    }
}

/// Tools rendered directly by the HTML template (upstream
/// `TEMPLATE_RENDERED_TOOLS`); other tools need pre-rendered HTML.
pub const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

// ============================================================================
// Color helpers (upstream parseColor / getLuminance / adjustBrightness /
// deriveExportColors)
// ============================================================================

/// Parse a color string to RGB (upstream `parseColor`): `#RRGGBB` and
/// `rgb(r, g, b)` formats.
pub fn parse_color(color: &str) -> Option<(u8, u8, u8)> {
    let color = color.trim();
    if let Some(hex) = color.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    if let Some(rest) = color.strip_prefix("rgb") {
        let rest = rest.trim();
        let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return None;
        }
        let r: u32 = parts[0].parse().ok()?;
        let g: u32 = parts[1].parse().ok()?;
        let b: u32 = parts[2].parse().ok()?;
        if r > 255 || g > 255 || b > 255 {
            return None;
        }
        return Some((r as u8, g as u8, b as u8));
    }
    None
}

/// Relative luminance of a color (0-1, higher = lighter) (upstream
/// `getLuminance`).
pub fn get_luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |c: u8| {
        let s = c as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

/// Adjust color brightness; factor > 1 lightens, < 1 darkens (upstream
/// `adjustBrightness`). Unparseable colors pass through unchanged.
pub fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some((r, g, b)) = parse_color(color) else {
        return color.to_string();
    };
    let adjust = |c: u8| ((c as f64 * factor).round() as i64).clamp(0, 255) as u8;
    format!("rgb({}, {}, {})", adjust(r), adjust(g), adjust(b))
}

/// Derive export background colors from a base color (upstream
/// `deriveExportColors`).
pub fn derive_export_colors(base_color: &str) -> ExportColors {
    let Some((r, g, b)) = parse_color(base_color) else {
        return ExportColors::default();
    };
    let luminance = get_luminance(r, g, b);
    let is_light = luminance > 0.5;
    if is_light {
        ExportColors {
            page_bg: adjust_brightness(base_color, 0.96),
            card_bg: base_color.to_string(),
            info_bg: format!(
                "rgb({}, {}, {})",
                r.saturating_add(10),
                g.saturating_add(5),
                b.saturating_sub(20)
            ),
        }
    } else {
        ExportColors {
            page_bg: adjust_brightness(base_color, 0.7),
            card_bg: adjust_brightness(base_color, 0.85),
            info_bg: format!(
                "rgb({}, {}, {})",
                r.saturating_add(20),
                g.saturating_add(15),
                b
            ),
        }
    }
}

// ============================================================================
// Session data + template substitution
// ============================================================================

/// The session payload embedded into the export (upstream `SessionData`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionData {
    /// Session header JSON, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Value>,
    /// Session entries serialized in the upstream JSONL shapes.
    pub entries: Vec<Value>,
    #[serde(rename = "leafId", skip_serializing_if = "Option::is_none")]
    pub leaf_id: Option<String>,
    #[serde(rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Tool definitions (name/description/parameters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(rename = "renderedTools", skip_serializing_if = "Option::is_none")]
    pub rendered_tools: Option<std::collections::BTreeMap<String, RenderedToolHtml>>,
}

impl SessionData {
    /// Assemble session data from a manager-shaped source (upstream builds
    /// this from SessionManager + AgentState).
    pub fn from_parts(
        header: Option<Value>,
        entries: &[SessionEntry],
        leaf_id: Option<&str>,
        system_prompt: Option<&str>,
        tools: Option<Vec<Value>>,
        rendered_tools: Option<std::collections::BTreeMap<String, RenderedToolHtml>>,
    ) -> Self {
        Self {
            header,
            entries: entries
                .iter()
                .map(crate::core::session_manager::entry_to_json)
                .collect(),
            leaf_id: leaf_id.map(str::to_string),
            system_prompt: system_prompt.map(str::to_string),
            tools,
            rendered_tools,
        }
    }

    fn to_base64(&self) -> String {
        use std::io::Write as _;
        let json = serde_json::to_string(self).unwrap_or_default();
        // Base64-encode the JSON (upstream Buffer.toString("base64")).
        let mut out = String::with_capacity(json.len() * 4 / 3 + 4);
        for chunk in json.as_bytes().chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        let _ = std::io::sink().write_all(b"");
        out
    }
}

fn read_template_asset(name: &str) -> Result<String, String> {
    let path = templates_dir().join(name);
    fs::read_to_string(&path).map_err(|e| format!("Failed to read export template {name}: {e}"))
}

/// The bundled template directory (embedded via include_str! so the crate is
/// self-contained; upstream reads from the installed package directory).
fn templates_dir() -> PathBuf {
    // Files are embedded; this path is only used for error messages.
    PathBuf::from("templates")
}

const TEMPLATE_HTML: &str = include_str!("templates/template.html");
const TEMPLATE_CSS: &str = include_str!("templates/template.css");
const TEMPLATE_JS: &str = include_str!("templates/template.js");
const MARKED_JS: &str = include_str!("templates/marked.min.js");
const HIGHLIGHT_JS: &str = include_str!("templates/highlight.min.js");

/// Core HTML generation shared by both export functions (upstream
/// `generateHtml`): session data is base64-embedded into the template with
/// theme colors injected into the CSS.
pub fn generate_html(
    session_data: &SessionData,
    colors: Option<&ExportColors>,
) -> Result<String, String> {
    let _ = read_template_asset("template.html");
    let export_colors = colors.cloned().unwrap_or_default();

    let css = TEMPLATE_CSS
        .replace("{{THEME_VARS}}", "")
        .replace("{{BODY_BG}}", &export_colors.page_bg)
        .replace("{{CONTAINER_BG}}", &export_colors.card_bg)
        .replace("{{INFO_BG}}", &export_colors.info_bg);

    Ok(TEMPLATE_HTML
        .replace("{{CSS}}", &css)
        .replace("{{JS}}", TEMPLATE_JS)
        .replace("{{SESSION_DATA}}", &session_data.to_base64())
        .replace("{{MARKED_JS}}", MARKED_JS)
        .replace("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS))
}

/// Derive the default output path from a session file name (upstream
/// `${APP_NAME}-session-<basename>.html`).
pub fn default_output_path(session_file: &Path) -> PathBuf {
    let basename = session_file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    PathBuf::from(format!(
        "{}-session-{basename}.html",
        crate::cli::args::APP_NAME
    ))
}

/// Write the export to disk, returning the output path (upstream
/// `exportSessionToHtml`/`exportFromFile` write half).
pub fn write_export(
    html: &str,
    output_path: Option<&Path>,
    input_session_file: &Path,
) -> Result<PathBuf, String> {
    let output = output_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_output_path(input_session_file));
    fs::write(&output, html).map_err(|e| format!("Failed to write export: {e}"))?;
    Ok(output)
}
