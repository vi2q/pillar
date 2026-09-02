//! Port of packages/coding-agent/src/core/export-html/ansi-to-html.ts (pi
//! v0.84.3): converts terminal ANSI color/style codes to HTML with inline
//! styles. Supports standard and bright foreground/background colors,
//! 256-color palette, RGB true color, bold/dim/italic/underline, and reset.

/// Standard ANSI color palette (0-15), verbatim from upstream.
const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
    "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

/// Convert a 256-color index to a hex color (upstream `color256ToHex`).
fn color256_to_hex(index: u8) -> String {
    // Standard colors (0-15)
    if index < 16 {
        return ANSI_COLORS[index as usize].to_string();
    }
    // Color cube (16-231): 6x6x6 = 216 colors
    if index < 232 {
        let cube_index = index - 16;
        let r = cube_index / 36;
        let g = (cube_index % 36) / 6;
        let b = cube_index % 6;
        let to_component = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
        let to_hex = |n: u8| format!("{:02x}", to_component(n));
        return format!("#{}{}{}", to_hex(r), to_hex(g), to_hex(b));
    }
    // Grayscale (232-255): 24 shades
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// Escape HTML special characters (upstream `escapeHtml`).
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// The current text style while scanning (upstream `TextStyle`).
#[derive(Debug, Clone, Default, PartialEq)]
struct TextStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

fn style_to_inline_css(style: &TextStyle) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(fg) = &style.fg {
        parts.push(format!("color:{fg}"));
    }
    if let Some(bg) = &style.bg {
        parts.push(format!("background-color:{bg}"));
    }
    if style.bold {
        parts.push("font-weight:bold".to_string());
    }
    if style.dim {
        parts.push("opacity:0.6".to_string());
    }
    if style.italic {
        parts.push("font-style:italic".to_string());
    }
    if style.underline {
        parts.push("text-decoration:underline".to_string());
    }
    parts.join(";")
}

fn has_style(style: &TextStyle) -> bool {
    style.fg.is_some()
        || style.bg.is_some()
        || style.bold
        || style.dim
        || style.italic
        || style.underline
}

/// Parse ANSI SGR (Select Graphic Rendition) codes and update the style
/// (upstream `applySgrCode`). Unrecognized codes are ignored.
fn apply_sgr_code(params: &[u64], style: &mut TextStyle) {
    let mut i = 0usize;
    while i < params.len() {
        let code = params[i];
        match code {
            0 => {
                // Reset all
                style.fg = None;
                style.bg = None;
                style.bold = false;
                style.dim = false;
                style.italic = false;
                style.underline = false;
            }
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                // Reset bold/dim
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => {
                // Standard foreground colors
                style.fg = Some(ANSI_COLORS[(code - 30) as usize].to_string());
            }
            38 => {
                // Extended foreground color
                if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                    // 256-color: 38;5;N
                    style.fg = Some(color256_to_hex(params[i + 2] as u8));
                    i += 2;
                } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                    // RGB: 38;2;R;G;B
                    let (r, g, b) = (params[i + 2], params[i + 3], params[i + 4]);
                    style.fg = Some(format!("rgb({r},{g},{b})"));
                    i += 4;
                }
            }
            39 => style.fg = None,
            40..=47 => {
                // Standard background colors
                style.bg = Some(ANSI_COLORS[(code - 40) as usize].to_string());
            }
            48 => {
                // Extended background color
                if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                    style.bg = Some(color256_to_hex(params[i + 2] as u8));
                    i += 2;
                } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                    let (r, g, b) = (params[i + 2], params[i + 3], params[i + 4]);
                    style.bg = Some(format!("rgb({r},{g},{b})"));
                    i += 4;
                }
            }
            49 => style.bg = None,
            90..=97 => {
                // Bright foreground colors
                style.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].to_string());
            }
            100..=107 => {
                // Bright background colors
                style.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].to_string());
            }
            _ => {}
        }
        i += 1;
    }
}

/// One ANSI SGR sequence match: the numeric params plus the total length.
struct SgrMatch {
    params: Vec<u64>,
    /// Byte offset of the sequence start.
    start: usize,
    /// Byte length of the whole sequence.
    len: usize,
}

/// Find the next `\x1b[...m` sequence at or after `from` (upstream uses a
/// global regex; the port scans manually, accepting empty and non-numeric
/// parameter bodies like the `[\d;]*` pattern).
fn next_sgr(text: &str, from: usize) -> Option<SgrMatch> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                let param_str = &text[i + 2..j];
                let params: Vec<u64> = if param_str.is_empty() {
                    vec![0]
                } else {
                    param_str
                        .split(';')
                        .map(|p| p.parse::<u64>().unwrap_or(0))
                        .collect()
                };
                return Some(SgrMatch {
                    params,
                    start: i,
                    len: j + 1 - i,
                });
            }
            // Not a well-formed SGR; continue scanning from i+1.
        }
        i += 1;
    }
    None
}

/// Convert ANSI-escaped text to HTML with inline styles (upstream
/// `ansiToHtml`).
pub fn ansi_to_html(text: &str) -> String {
    let mut style = TextStyle::default();
    let mut result = String::new();
    let mut last_index = 0usize;
    let mut in_span = false;

    let mut scan_from = 0usize;
    while let Some(sgr) = next_sgr(text, scan_from) {
        // Add text before this escape sequence.
        let before_text = &text[last_index..sgr.start];
        if !before_text.is_empty() {
            result.push_str(&escape_html(before_text));
        }

        // Close the existing span if open.
        if in_span {
            result.push_str("</span>");
            in_span = false;
        }

        // Apply the codes.
        apply_sgr_code(&sgr.params, &mut style);

        // Open a new span if we have any styling.
        if has_style(&style) {
            result.push_str(&format!("<span style=\"{}\">", style_to_inline_css(&style)));
            in_span = true;
        }

        last_index = sgr.start + sgr.len;
        scan_from = last_index;
    }

    // Add remaining text.
    let remaining = &text[last_index..];
    if !remaining.is_empty() {
        result.push_str(&escape_html(remaining));
    }

    if in_span {
        result.push_str("</span>");
    }

    result
}

/// Convert an array of ANSI-escaped lines to HTML (upstream
/// `ansiLinesToHtml`): each line is wrapped in a div element; blank lines
/// render as `&nbsp;`.
pub fn ansi_lines_to_html(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| {
            let converted = ansi_to_html(line);
            let inner = if converted.is_empty() {
                "&nbsp;"
            } else {
                &converted
            };
            format!("<div class=\"ansi-line\">{inner}</div>")
        })
        .collect()
}
