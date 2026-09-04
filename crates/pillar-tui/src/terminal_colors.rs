//! Port of packages/tui/src/terminal-colors.ts (pi v0.84.3): OSC 11
//! background-color response parsing and the color-scheme report
//! protocol (CSI ? 997 n).
//!
//! divergences: the query lifecycle (pending replies, timeout timers,
//! terminal writes) stays host-side; the port exposes the pure
//! parsers.

/// An RGB color (upstream `RgbColor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A terminal color scheme (upstream `TerminalColorScheme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16u32.checked_pow(channel.len() as u32)? - 1;
    let value = u32::from_str_radix(channel, 16).ok()?;
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

fn hex_to_rgb(hex: &str) -> RgbColor {
    let normalized = hex.strip_prefix('#').unwrap_or(hex);
    RgbColor {
        r: u8::from_str_radix(&normalized[0..2], 16).unwrap_or(0),
        g: u8::from_str_radix(&normalized[2..4], 16).unwrap_or(0),
        b: u8::from_str_radix(&normalized[4..6], 16).unwrap_or(0),
    }
}

/// Whether the data is an OSC 11 background-color response (upstream
/// `isOsc11BackgroundColorResponse`): `ESC ] 11 ; <value> BEL|ST`.
pub fn is_osc11_background_color_response(data: &str) -> bool {
    let Some(rest) = data.strip_prefix("\u{1b}]11;") else {
        return false;
    };
    rest.ends_with('\u{7}') || rest.ends_with("\u{1b}\\")
}

/// Parse an OSC 11 background-color reply (upstream
/// `parseOsc11BackgroundColor`): `#rrggbb`, `#rrrrggggbbbb` (16-bit),
/// or `rgb:RR/GG/BB` hex channels (any width, scaled to 8-bit).
pub fn parse_osc11_background_color(data: &str) -> Option<RgbColor> {
    let rest = data.strip_prefix("\u{1b}]11;")?;
    let value = rest
        .strip_suffix("\u{1b}\\")
        .or_else(|| rest.strip_suffix('\u{7}'))?
        .trim();

    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex_to_rgb(value));
        }
        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = parse_osc_hex_channel(&hex[0..4])?;
            let g = parse_osc_hex_channel(&hex[4..8])?;
            let b = parse_osc_hex_channel(&hex[8..12])?;
            return Some(RgbColor { r, g, b });
        }
        return None;
    }

    let rgb_value = value
        .strip_prefix("rgba:")
        .or_else(|| value.strip_prefix("rgb:"))
        .or_else(|| value.strip_prefix("RGBA:"))
        .or_else(|| value.strip_prefix("RGB:"))
        .unwrap_or(value);
    let mut parts = rgb_value.split('/');
    let (Some(red), Some(green), Some(blue)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    if parts.next().is_some() {
        return None;
    }
    let r = parse_osc_hex_channel(red)?;
    let g = parse_osc_hex_channel(green)?;
    let b = parse_osc_hex_channel(blue)?;
    Some(RgbColor { r, g, b })
}

/// Parse a color-scheme report (upstream
/// `parseTerminalColorSchemeReport`): repeated `CSI ? 997 ; 1|2 n`
/// sequences; the last one wins (dark=1, light=2).
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    if data.is_empty() || !data.starts_with("\u{1b}[?997;") {
        return None;
    }
    let mut scheme = None;
    let mut rest = data;
    while let Some(segment) = rest.strip_prefix("\u{1b}[?997;") {
        let mut chars = segment.chars();
        let code = chars.next()?;
        if chars.next() != Some('n') {
            return None;
        }
        scheme = Some(match code {
            '1' => TerminalColorScheme::Dark,
            '2' => TerminalColorScheme::Light,
            _ => return None,
        });
        rest = &segment[2..];
    }
    if !rest.is_empty() {
        return None;
    }
    scheme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_16_bit_osc11_rgb_responses() {
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;rgb:0000/8000/ffff\u{7}"),
            Some(RgbColor {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn parses_osc11_hex_responses() {
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;#ffffff\u{1b}\\"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;#000000\u{7}"),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
    }

    #[test]
    fn rejects_non_strict_osc11_responses() {
        assert_eq!(
            parse_osc11_background_color("x\u{1b}]11;#ffffff\u{7}"),
            None
        );
        assert_eq!(parse_osc11_background_color("\u{1b}]10;#ffffff\u{7}"), None);
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;#ffffff\u{7}x"),
            None
        );
    }

    #[test]
    fn detects_osc11_responses() {
        assert!(is_osc11_background_color_response("\u{1b}]11;#ffffff\u{7}"));
        assert!(is_osc11_background_color_response(
            "\u{1b}]11;rgb:1/2/3\u{1b}\\"
        ));
        assert!(!is_osc11_background_color_response(
            "\u{1b}]10;#ffffff\u{7}"
        ));
    }

    #[test]
    fn parses_color_scheme_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\u{1b}[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\u{1b}[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        // Repeated reports: the last one wins.
        assert_eq!(
            parse_terminal_color_scheme_report("\u{1b}[?997;2n\u{1b}[?997;1n\u{1b}[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\u{1b}[?997;1n\u{1b}[?997;2n\u{1b}[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        // Unknown code, query, or junk prefix rejected.
        assert_eq!(parse_terminal_color_scheme_report("\u{1b}[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("\u{1b}[?996n"), None);
        assert_eq!(parse_terminal_color_scheme_report("x\u{1b}[?997;1n"), None);
    }

    #[test]
    fn parses_8_bit_rgb_channels() {
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;rgb:ff/00/80\u{7}"),
            Some(RgbColor {
                r: 255,
                g: 0,
                b: 128
            })
        );
    }

    #[test]
    fn rejects_malformed_rgb_channels() {
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;rgb:zz/00/80\u{7}"),
            None
        );
        assert_eq!(
            parse_osc11_background_color("\u{1b}]11;rgb:ff/00\u{7}"),
            None
        );
        assert_eq!(parse_osc11_background_color("\u{1b}]11;#fff\u{7}"), None);
    }
}
