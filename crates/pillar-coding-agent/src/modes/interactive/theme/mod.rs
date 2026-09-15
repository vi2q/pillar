//! Port of packages/coding-agent/src/modes/interactive/theme/theme.ts
//! (pi v0.84.3), first slice: color utilities, the [`Theme`] class, and
//! theme loading from the built-in JSON files.
//!
//! divergences:
//! - upstream validates theme JSON with TypeBox and rejects unknown/missing
//!   keys; the port checks the required colour names by hand (same error
//!   strings) since the schema-driven validator is not ported.
//! - custom/extension-registered themes and terminal background detection
//!   (`theme-controller.ts`, `theme-schema.json`) land later.
//! - `chalk` styling is emitted directly; `ColorMode::Ansi16` renders through
//!   the 256-colour path exactly like upstream (`fgAnsi` only special-cases
//!   `truecolor`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use pillar_tui::editor::EditorTheme;
use pillar_tui::markdown::MarkdownTheme;
use pillar_tui::select_list::SelectListTheme;
use pillar_tui::settings_list::SettingsListTheme;
use pillar_tui::terminal_colors::{RgbColor, TerminalColorScheme};
use serde_json::Value;

use crate::core::source_info::SourceInfo;
use crate::utils::clipboard::ClipboardEnv;

pub mod controller;

/// How many colours the terminal can show (upstream `ColorMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Ansi256,
    Ansi16,
}

impl ColorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ColorMode::Truecolor => "truecolor",
            ColorMode::Ansi256 => "256color",
            ColorMode::Ansi16 => "16color",
        }
    }
}

/// A theme colour value (upstream `ColorValue`): a hex string, a 256-colour
/// index, or `""` for the terminal default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorValue {
    Hex(String),
    Index(u8),
    Empty,
}

impl ColorValue {
    /// Parse a JSON colour value.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(text) if text.is_empty() => Ok(ColorValue::Empty),
            Value::String(text) => Ok(ColorValue::Hex(text.clone())),
            Value::Number(number) => {
                let index = number
                    .as_u64()
                    .filter(|index| *index <= 255)
                    .ok_or_else(|| format!("Invalid color value: {number}"))?;
                Ok(ColorValue::Index(index as u8))
            }
            other => Err(format!("Invalid color value: {other}")),
        }
    }

    fn is_reference(&self) -> Option<&str> {
        match self {
            ColorValue::Hex(text) if !text.starts_with('#') => Some(text),
            _ => None,
        }
    }
}

/// Keys that are background colours (upstream `bgColorKeys`).
pub const BG_COLOR_KEYS: [&str; 8] = [
    "selectedBg",
    "scrollbarThumb",
    "searchMatchBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];

/// Required non-background colour names (upstream `ThemeJsonSchema.colors`
/// minus [`BG_COLOR_KEYS`] and the optional `thinkingMax`).
pub const REQUIRED_FG_COLORS: [&str; 45] = [
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "userMessageText",
    "customMessageText",
    "customMessageLabel",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

/// Required background colour names (upstream the required subset of
/// [`BG_COLOR_KEYS`]: `scrollbarThumb` / `searchMatchBg` are optional).
pub const REQUIRED_BG_COLORS: [&str; 6] = [
    "selectedBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];

/// A parsed theme document (upstream `ThemeJson`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeJson {
    pub name: String,
    pub vars: BTreeMap<String, ColorValue>,
    pub colors: BTreeMap<String, ColorValue>,
    /// Optional explicit colours for HTML exports (upstream `export`).
    pub export: BTreeMap<String, ColorValue>,
}

impl ThemeJson {
    /// Parse and validate a theme document (upstream `parseThemeJson`).
    pub fn parse(label: &str, json: &Value) -> Result<Self, String> {
        let object = json
            .as_object()
            .ok_or_else(|| format!("Invalid {label}: theme must be an object"))?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Invalid {label}: name is required"))?;
        let colors = object
            .get("colors")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("Invalid {label}: colors is required"))?;

        let mut parsed_colors = BTreeMap::new();
        for (key, value) in colors {
            parsed_colors.insert(key.clone(), ColorValue::from_json(value)?);
        }
        for key in REQUIRED_FG_COLORS.iter().chain(REQUIRED_BG_COLORS.iter()) {
            if !parsed_colors.contains_key(*key) {
                return Err(format!("Invalid {label}: missing color \"{key}\""));
            }
        }

        let mut vars = BTreeMap::new();
        if let Some(object) = object.get("vars").and_then(Value::as_object) {
            for (key, value) in object {
                vars.insert(key.clone(), ColorValue::from_json(value)?);
            }
        }

        let mut export = BTreeMap::new();
        if let Some(object) = object.get("export").and_then(Value::as_object) {
            for (key, value) in object {
                export.insert(key.clone(), ColorValue::from_json(value)?);
            }
        }

        Ok(ThemeJson {
            name: name.to_string(),
            vars,
            colors: parsed_colors,
            export,
        })
    }

    /// Parse theme content (upstream `parseThemeJsonContent`).
    pub fn parse_content(label: &str, content: &str) -> Result<Self, String> {
        let json: Value = serde_json::from_str(crate::utils::text::strip_bom(content))
            .map_err(|error| format!("Invalid {label}: {error}"))?;
        Self::parse(label, &json)
    }
}

/// Apply the optional-colour fallbacks (upstream
/// `withThemeColorFallbacks`).
fn with_color_fallbacks(colors: &BTreeMap<String, ColorValue>) -> BTreeMap<String, ColorValue> {
    let mut result = colors.clone();
    let fallback = |result: &mut BTreeMap<String, ColorValue>, key: &str, from: &str| {
        if !result.contains_key(key) {
            if let Some(value) = result.get(from).cloned() {
                result.insert(key.to_string(), value);
            }
        }
    };
    fallback(&mut result, "thinkingMax", "thinkingXhigh");
    fallback(&mut result, "scrollbarThumb", "selectedBg");
    fallback(&mut result, "searchMatchBg", "selectedBg");
    fallback(&mut result, "searchMatchText", "text");
    result
}

/// Resolve a colour that may reference a `vars` entry (upstream
/// `resolveVarRefs`).
fn resolve_var_refs(
    value: &ColorValue,
    vars: &BTreeMap<String, ColorValue>,
    visited: &mut Vec<String>,
) -> Result<ColorValue, String> {
    let Some(reference) = value.is_reference() else {
        return Ok(value.clone());
    };
    if visited.iter().any(|seen| seen == reference) {
        return Err(format!("Circular variable reference detected: {reference}"));
    }
    let Some(target) = vars.get(reference) else {
        return Err(format!("Variable reference not found: {reference}"));
    };
    visited.push(reference.to_string());
    resolve_var_refs(target, vars, visited)
}

/// A resolved theme (upstream `Theme`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    name: Option<String>,
    source_path: Option<String>,
    source_info: Option<SourceInfo>,
    fg_colors: BTreeMap<String, String>,
    bg_colors: BTreeMap<String, String>,
    mode: ColorMode,
}

impl Theme {
    /// Build a theme from resolved foreground/background colours (upstream the
    /// `Theme` constructor, which also applies the optional fallbacks).
    pub fn new(
        fg_colors: BTreeMap<String, ColorValue>,
        bg_colors: BTreeMap<String, ColorValue>,
        mode: ColorMode,
        name: Option<&str>,
        source_path: Option<&str>,
    ) -> Self {
        let fg = with_color_fallbacks(&fg_colors);
        let bg = with_color_fallbacks(&bg_colors);
        let resolve =
            |colors: &BTreeMap<String, ColorValue>,
             ansi: fn(&ColorValue, ColorMode) -> Result<String, String>| {
                colors
                    .iter()
                    .map(|(key, value)| {
                        let sequence = ansi(value, mode)
                            .unwrap_or_else(|error| panic!("Theme color {key}: {error}"));
                        (key.clone(), sequence)
                    })
                    .collect::<BTreeMap<String, String>>()
            };

        Self {
            name: name.map(str::to_string),
            source_path: source_path.map(str::to_string),
            source_info: None,
            fg_colors: resolve(&fg, fg_ansi),
            bg_colors: resolve(&bg, bg_ansi),
            mode,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn source_path(&self) -> Option<&str> {
        self.source_path.as_deref()
    }

    pub fn source_info(&self) -> Option<&SourceInfo> {
        self.source_info.as_ref()
    }

    pub fn color_mode(&self) -> ColorMode {
        self.mode
    }

    /// Colour `text` with a foreground colour (upstream `Theme.fg`).
    pub fn fg(&self, color: &str, text: &str) -> String {
        let ansi = self
            .fg_colors
            .get(color)
            .unwrap_or_else(|| panic!("Unknown theme color: {color}"));
        format!("{ansi}{text}\x1b[39m")
    }

    /// Colour `text` with a background colour (upstream `Theme.bg`).
    pub fn bg(&self, color: &str, text: &str) -> String {
        let ansi = self
            .bg_colors
            .get(color)
            .unwrap_or_else(|| panic!("Unknown theme background color: {color}"));
        format!("{ansi}{text}\x1b[49m")
    }

    pub fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }

    pub fn italic(&self, text: &str) -> String {
        format!("\x1b[3m{text}\x1b[23m")
    }

    pub fn underline(&self, text: &str) -> String {
        format!("\x1b[4m{text}\x1b[24m")
    }

    pub fn inverse(&self, text: &str) -> String {
        format!("\x1b[7m{text}\x1b[27m")
    }

    pub fn strikethrough(&self, text: &str) -> String {
        format!("\x1b[9m{text}\x1b[29m")
    }

    /// The raw foreground ANSI sequence (upstream `getFgAnsi`).
    pub fn fg_ansi(&self, color: &str) -> String {
        self.fg_colors
            .get(color)
            .unwrap_or_else(|| panic!("Unknown theme color: {color}"))
            .clone()
    }

    /// The raw background ANSI sequence (upstream `getBgAnsi`).
    pub fn bg_ansi(&self, color: &str) -> String {
        self.bg_colors
            .get(color)
            .unwrap_or_else(|| panic!("Unknown theme background color: {color}"))
            .clone()
    }

    /// Colour for a thinking level's border (upstream
    /// `getThinkingBorderColor`).
    pub fn thinking_border_color(&self, level: &str) -> String {
        let color = match level {
            "off" => "thinkingOff",
            "minimal" => "thinkingMinimal",
            "low" => "thinkingLow",
            "medium" => "thinkingMedium",
            "high" => "thinkingHigh",
            "xhigh" => "thinkingXhigh",
            "max" => "thinkingMax",
            _ => "thinkingOff",
        };
        self.fg_ansi(color)
    }

    /// Colour for the bash-mode border (upstream `getBashModeBorderColor`).
    pub fn bash_mode_border_color(&self) -> String {
        self.fg_ansi("bashMode")
    }
}

/// Build a theme from a parsed document (upstream `createTheme`).
pub fn create_theme(theme_json: &ThemeJson, mode: ColorMode, source_path: Option<&str>) -> Theme {
    let resolved = with_color_fallbacks(&theme_json.colors);
    let mut fg_colors = BTreeMap::new();
    let mut bg_colors = BTreeMap::new();
    for (key, value) in &resolved {
        let value = resolve_var_refs(value, &theme_json.vars, &mut Vec::new())
            .unwrap_or_else(|error| panic!("{error}"));
        if BG_COLOR_KEYS.contains(&key.as_str()) {
            bg_colors.insert(key.clone(), value);
        } else {
            fg_colors.insert(key.clone(), value);
        }
    }
    Theme::new(
        fg_colors,
        bg_colors,
        mode,
        Some(&theme_json.name),
        source_path,
    )
}

/// The built-in theme documents (upstream `getBuiltinThemes`), embedded from
/// the same JSON files.
pub fn builtin_themes() -> &'static BTreeMap<String, ThemeJson> {
    static BUILTIN: LazyLock<BTreeMap<String, ThemeJson>> = LazyLock::new(|| {
        let mut themes = BTreeMap::new();
        for (label, content) in [
            ("dark", include_str!("dark.json")),
            ("light", include_str!("light.json")),
        ] {
            let theme = ThemeJson::parse_content(label, content)
                .unwrap_or_else(|error| panic!("builtin theme {label}: {error}"));
            themes.insert(theme.name.clone(), theme);
        }
        themes
    });
    &BUILTIN
}

/// Names of the built-in themes.
pub fn get_builtin_theme_names() -> Vec<String> {
    builtin_themes().keys().cloned().collect()
}

/// Look up a built-in theme by name (upstream `loadTheme` for built-ins).
pub fn get_theme_by_name(name: &str) -> Option<Theme> {
    builtin_themes()
        .get(name)
        .map(|theme_json| create_theme(theme_json, ColorMode::Truecolor, None))
}

/// Load a theme from a JSON file (upstream `loadThemeFromPath`).
pub fn load_theme_from_path(theme_path: &str, mode: ColorMode) -> Result<Theme, String> {
    let content = std::fs::read_to_string(theme_path)
        .map_err(|error| format!("Failed to read theme {theme_path}: {error}"))?;
    let theme_json = ThemeJson::parse_content(theme_path, &content)?;
    Ok(create_theme(&theme_json, mode, Some(theme_path)))
}

/// Parse a hex colour (upstream `hexToRgb`).
pub fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), String> {
    let cleaned = hex.replace('#', "");
    if cleaned.len() != 6 {
        return Err(format!("Invalid hex color: {hex}"));
    }
    let parse = |slice: &str| u8::from_str_radix(slice, 16).ok();
    match (
        parse(&cleaned[0..2]),
        parse(&cleaned[2..4]),
        parse(&cleaned[4..6]),
    ) {
        (Some(r), Some(g), Some(b)) => Ok((r, g, b)),
        _ => Err(format!("Invalid hex color: {hex}")),
    }
}

/// The 6x6x6 colour-cube channel values (upstream `CUBE_VALUES`).
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The 24-step grayscale ramp (upstream `GRAY_VALUES`).
fn gray_values() -> [u8; 24] {
    let mut values = [0u8; 24];
    for (index, value) in values.iter_mut().enumerate() {
        *value = (8 + index as u16 * 10) as u8;
    }
    values
}

fn find_closest_cube_index(value: u8) -> usize {
    let mut min_dist = f64::INFINITY;
    let mut min_index = 0;
    for (index, candidate) in CUBE_VALUES.iter().enumerate() {
        let dist = (value as f64 - *candidate as f64).abs();
        if dist < min_dist {
            min_dist = dist;
            min_index = index;
        }
    }
    min_index
}

fn find_closest_gray_index(gray: u8) -> usize {
    let mut min_dist = f64::INFINITY;
    let mut min_index = 0;
    for (index, candidate) in gray_values().iter().enumerate() {
        let dist = (gray as f64 - *candidate as f64).abs();
        if dist < min_dist {
            min_dist = dist;
            min_index = index;
        }
    }
    min_index
}

/// Weighted Euclidean distance (upstream `colorDistance`).
fn color_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> f64 {
    let dr = r1 as f64 - r2 as f64;
    let dg = g1 as f64 - g2 as f64;
    let db = b1 as f64 - b2 as f64;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// Map an RGB triple to the closest 256-colour index (upstream `rgbTo256`).
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let r_index = find_closest_cube_index(r);
    let g_index = find_closest_cube_index(g);
    let b_index = find_closest_cube_index(b);
    let cube_r = CUBE_VALUES[r_index];
    let cube_g = CUBE_VALUES[g_index];
    let cube_b = CUBE_VALUES[b_index];
    let cube_index = (16 + 36 * r_index + 6 * g_index + b_index) as u8;
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);

    let gray = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64).round() as u8;
    let gray_index = find_closest_gray_index(gray);
    let gray_value = gray_values()[gray_index];
    let gray_256_index = (232 + gray_index) as u8;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = max_c - min_c;

    if spread < 10 && gray_dist < cube_dist {
        return gray_256_index;
    }
    cube_index
}

/// Map a hex colour to the closest 256-colour index (upstream `hexTo256`).
pub fn hex_to_256(hex: &str) -> Result<u8, String> {
    let (r, g, b) = hex_to_rgb(hex)?;
    Ok(rgb_to_256(r, g, b))
}

/// Foreground ANSI sequence for a colour (upstream `fgAnsi`).
pub fn fg_ansi(color: &ColorValue, mode: ColorMode) -> Result<String, String> {
    match color {
        ColorValue::Empty => Ok("\x1b[39m".to_string()),
        ColorValue::Index(index) => Ok(format!("\x1b[38;5;{index}m")),
        ColorValue::Hex(hex) if hex.starts_with('#') => {
            if mode == ColorMode::Truecolor {
                let (r, g, b) = hex_to_rgb(hex)?;
                Ok(format!("\x1b[38;2;{r};{g};{b}m"))
            } else {
                Ok(format!("\x1b[38;5;{}m", hex_to_256(hex)?))
            }
        }
        ColorValue::Hex(other) => Err(format!("Invalid color value: {other}")),
    }
}

/// Background ANSI sequence for a colour (upstream `bgAnsi`).
pub fn bg_ansi(color: &ColorValue, mode: ColorMode) -> Result<String, String> {
    match color {
        ColorValue::Empty => Ok("\x1b[49m".to_string()),
        ColorValue::Index(index) => Ok(format!("\x1b[48;5;{index}m")),
        ColorValue::Hex(hex) if hex.starts_with('#') => {
            if mode == ColorMode::Truecolor {
                let (r, g, b) = hex_to_rgb(hex)?;
                Ok(format!("\x1b[48;2;{r};{g};{b}m"))
            } else {
                Ok(format!("\x1b[48;5;{}m", hex_to_256(hex)?))
            }
        }
        ColorValue::Hex(other) => Err(format!("Invalid color value: {other}")),
    }
}

// ============================================================================
// Theme selection and terminal detection (upstream theme.ts, same sections)
// ============================================================================

/// The terminal's background brightness (upstream `TerminalTheme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTheme {
    Dark,
    Light,
}

impl TerminalTheme {
    pub fn as_str(self) -> &'static str {
        match self {
            TerminalTheme::Dark => "dark",
            TerminalTheme::Light => "light",
        }
    }
}

/// An automatic light/dark theme setting (upstream the `light/dark` pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoThemeSetting {
    pub light_theme: String,
    pub dark_theme: String,
}

/// Parse `"<light>/<dark>"` (upstream `parseAutoThemeSetting`): exactly one
/// slash and both sides non-empty.
pub fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<AutoThemeSetting> {
    let theme_setting = theme_setting?;
    let slash_index = theme_setting.find('/')?;
    if theme_setting[slash_index + 1..].contains('/') {
        return None;
    }
    let light_theme = theme_setting[..slash_index].trim();
    let dark_theme = theme_setting[slash_index + 1..].trim();
    if light_theme.is_empty() || dark_theme.is_empty() {
        return None;
    }
    Some(AutoThemeSetting {
        light_theme: light_theme.to_string(),
        dark_theme: dark_theme.to_string(),
    })
}

/// Resolve a theme setting for a terminal brightness (upstream
/// `resolveThemeSetting`): auto pairs pick a side, a stray slash is invalid,
/// anything else is used as-is.
pub fn resolve_theme_setting(
    theme_setting: Option<&str>,
    terminal_theme: TerminalTheme,
) -> Option<String> {
    if let Some(auto) = parse_auto_theme_setting(theme_setting) {
        return Some(match terminal_theme {
            TerminalTheme::Light => auto.light_theme,
            TerminalTheme::Dark => auto.dark_theme,
        });
    }
    let theme_setting = theme_setting?;
    if theme_setting.contains('/') {
        return None;
    }
    Some(theme_setting.to_string())
}

/// Where a background detection result came from (upstream `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThemeSource {
    TerminalBackground,
    ColorFgBg,
    Fallback,
}

impl TerminalThemeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TerminalThemeSource::TerminalBackground => "terminal background",
            TerminalThemeSource::ColorFgBg => "COLORFGBG",
            TerminalThemeSource::Fallback => "fallback",
        }
    }
}

/// Detection confidence (upstream `confidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThemeConfidence {
    High,
    Low,
}

impl TerminalThemeConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            TerminalThemeConfidence::High => "high",
            TerminalThemeConfidence::Low => "low",
        }
    }
}

/// A background detection result (upstream `TerminalThemeDetection`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalThemeDetection {
    pub theme: TerminalTheme,
    pub source: TerminalThemeSource,
    pub detail: String,
    pub confidence: TerminalThemeConfidence,
}

/// The last valid `COLORFGBG` entry as a 256-colour index (upstream
/// `getColorFgBgBackgroundIndex`).
pub fn get_color_fg_bg_background_index(colorfgbg: &str) -> Option<u8> {
    for part in colorfgbg.split(';').rev() {
        if let Ok(index) = part.trim().parse::<u16>() {
            if index <= 255 {
                return Some(index as u8);
            }
        }
    }
    None
}

/// Relative luminance of an RGB colour (upstream `getRgbColorLuminance`).
pub fn rgb_color_luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |channel: u8| {
        let value = channel as f64 / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

/// Relative luminance of a 256-colour index (upstream
/// `getAnsiColorLuminance`).
pub fn ansi_color_luminance(index: u8) -> f64 {
    let (r, g, b) = hex_to_rgb(&ansi256_to_hex(index)).unwrap_or((0, 0, 0));
    rgb_color_luminance(r, g, b)
}

/// Classify a background colour (upstream `getThemeForRgbColor`).
pub fn get_theme_for_rgb_color(r: u8, g: u8, b: u8) -> TerminalTheme {
    if rgb_color_luminance(r, g, b) >= 0.5 {
        TerminalTheme::Light
    } else {
        TerminalTheme::Dark
    }
}

/// Convert a 256-colour index to hex (upstream `ansi256ToHex`): the 16 basic
/// colours are approximations, 16-231 are the cube, 232-255 the gray ramp.
pub fn ansi256_to_hex(index: u8) -> String {
    const BASIC_COLORS: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if index < 16 {
        return BASIC_COLORS[index as usize].to_string();
    }
    if index < 232 {
        let cube_index = index as u32 - 16;
        let r = cube_index / 36;
        let g = (cube_index % 36) / 6;
        let b = cube_index % 6;
        let to_hex = |channel: u32| if channel == 0 { 0 } else { 55 + channel * 40 };
        return format!("#{:02x}{:02x}{:02x}", to_hex(r), to_hex(g), to_hex(b));
    }
    let gray = 8 + (index as u32 - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// Detect the terminal background from the environment (upstream
/// `detectTerminalBackgroundFromEnv`).
pub fn detect_terminal_background_from_env(
    env: &crate::utils::clipboard::ClipboardEnv,
) -> TerminalThemeDetection {
    let colorfgbg = env.get("COLORFGBG").unwrap_or_default();
    if let Some(background) = get_color_fg_bg_background_index(colorfgbg) {
        return TerminalThemeDetection {
            theme: if ansi_color_luminance(background) >= 0.5 {
                TerminalTheme::Light
            } else {
                TerminalTheme::Dark
            },
            source: TerminalThemeSource::ColorFgBg,
            detail: format!("background color index {background}"),
            confidence: TerminalThemeConfidence::High,
        };
    }

    TerminalThemeDetection {
        theme: TerminalTheme::Dark,
        source: TerminalThemeSource::Fallback,
        detail: "no terminal background hint found".to_string(),
        confidence: TerminalThemeConfidence::Low,
    }
}

/// The default theme name for this environment (upstream `getDefaultTheme`).
pub fn get_default_theme() -> String {
    let env = crate::utils::clipboard::ClipboardEnv::from_process();
    detect_terminal_background_from_env(&env)
        .theme
        .as_str()
        .to_string()
}

/// Theme names may not contain `/` (upstream `assertThemeNameIsValid`).
pub fn assert_theme_name_is_valid(name: &str) -> Result<(), String> {
    if name.contains('/') {
        return Err(format!(
            "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        ));
    }
    Ok(())
}

/// Look up a theme document by name: built-ins only for now (custom and
/// registered file-backed themes land with the theme controller).
pub fn load_theme_json(name: &str) -> Result<ThemeJson, String> {
    if let Some(theme) = builtin_themes().get(name) {
        return Ok(theme.clone());
    }
    Err(format!("Theme not found: {name}"))
}

/// Resolved theme colours as CSS-compatible hex strings (upstream
/// `getResolvedThemeColors`).
pub fn get_resolved_theme_colors(
    theme_name: Option<&str>,
) -> Result<BTreeMap<String, String>, String> {
    let name = match theme_name {
        Some(name) => name.to_string(),
        None => current_theme_name().unwrap_or_else(get_default_theme),
    };
    let is_light = name == "light";
    let theme_json = load_theme_json(&name)?;
    let resolved = with_color_fallbacks(&theme_json.colors);
    let default_text = if is_light { "#000000" } else { "#e5e5e7" };

    let mut colors = BTreeMap::new();
    for (key, value) in &resolved {
        let value = resolve_var_refs(value, &theme_json.vars, &mut Vec::new())?;
        let css = match value {
            ColorValue::Index(index) => ansi256_to_hex(index),
            ColorValue::Empty => default_text.to_string(),
            ColorValue::Hex(hex) => hex,
        };
        colors.insert(key.clone(), css);
    }
    Ok(colors)
}

/// Whether a theme is a light theme (upstream `isLightTheme`).
pub fn is_light_theme(theme_name: Option<&str>) -> bool {
    theme_name == Some("light")
}

/// Explicit export colours from a theme document (upstream
/// `getThemeExportColors`); unset or unresolvable entries stay `None`.
pub fn get_theme_export_colors(theme_name: Option<&str>) -> BTreeMap<String, Option<String>> {
    let mut result = BTreeMap::new();
    for key in ["pageBg", "cardBg", "infoBg"] {
        result.insert(key.to_string(), None);
    }
    let name = match theme_name {
        Some(name) => name.to_string(),
        None => current_theme_name().unwrap_or_else(get_default_theme),
    };
    let Ok(theme_json) = load_theme_json(&name) else {
        return result;
    };
    for (key, value) in &theme_json.export {
        let resolved = resolve_var_refs(value, &theme_json.vars, &mut Vec::new());
        let css = match resolved {
            Ok(ColorValue::Index(index)) => Some(ansi256_to_hex(index)),
            Ok(ColorValue::Empty) | Err(_) => None,
            Ok(ColorValue::Hex(hex)) => Some(hex),
        };
        result.insert(key.clone(), css);
    }
    result
}

// ============================================================================
// Global theme instance (upstream theme.ts global section)
// ============================================================================

/// The active theme instance plus the registered theme table.
///
/// divergence: upstream shares the instance through `globalThis` (for dual
/// module loaders) and hot-reloads custom theme files with an `fs` watcher;
/// the port keeps process-wide state in a `RwLock` and does not watch files.
#[derive(Default)]
struct ThemeRegistry {
    current: Option<std::sync::Arc<Theme>>,
    current_name: Option<String>,
    registered: BTreeMap<String, std::sync::Arc<Theme>>,
    on_change: Option<Box<dyn Fn() + Send + Sync>>,
}

static REGISTRY: LazyLock<std::sync::RwLock<ThemeRegistry>> =
    LazyLock::new(|| std::sync::RwLock::new(ThemeRegistry::default()));

/// The active theme (upstream `theme`). Panics when the theme was never
/// initialized, mirroring upstream's proxy error.
pub fn theme() -> std::sync::Arc<Theme> {
    let registry = REGISTRY.read().expect("theme registry");
    registry
        .current
        .clone()
        .expect("Theme not initialized. Call init_theme() first.")
}

/// The active theme name, if any.
pub fn current_theme_name() -> Option<String> {
    REGISTRY
        .read()
        .expect("theme registry")
        .current_name
        .clone()
}

/// Replace the registered theme table (upstream `setRegisteredThemes`).
pub fn set_registered_themes(themes: Vec<std::sync::Arc<Theme>>) -> Result<(), String> {
    let mut registry = REGISTRY.write().expect("theme registry");
    registry.registered.clear();
    for theme in themes {
        if let Some(name) = theme.name() {
            assert_theme_name_is_valid(name)?;
            registry.registered.insert(name.to_string(), theme);
        }
    }
    Ok(())
}

/// Load a theme by name: registered themes win over built-ins (upstream
/// `loadTheme`).
fn load_theme(name: &str) -> Result<Theme, String> {
    if let Some(theme) = REGISTRY
        .read()
        .expect("theme registry")
        .registered
        .get(name)
    {
        return Ok((**theme).clone());
    }
    load_theme_json(name).map(|theme_json| create_theme(&theme_json, ColorMode::Truecolor, None))
}

/// Initialize the active theme, falling back to dark on failure (upstream
/// `initTheme`).
pub fn init_theme(theme_name: Option<&str>) {
    let name = theme_name
        .map(str::to_string)
        .unwrap_or_else(get_default_theme);
    let (theme, resolved_name) = match load_theme(&name) {
        Ok(theme) => (theme, name),
        Err(_) => (
            create_theme(&builtin_themes()["dark"], ColorMode::Truecolor, None),
            "dark".to_string(),
        ),
    };
    let mut registry = REGISTRY.write().expect("theme registry");
    registry.current = Some(std::sync::Arc::new(theme));
    registry.current_name = Some(resolved_name);
}

/// Switch the active theme, falling back to dark on failure (upstream
/// `setTheme`).
pub fn set_theme(name: &str) -> Result<(), String> {
    let (theme, error) = match load_theme(name) {
        Ok(theme) => (theme, None),
        Err(error) => (
            create_theme(&builtin_themes()["dark"], ColorMode::Truecolor, None),
            Some(error),
        ),
    };
    let callback = {
        let mut registry = REGISTRY.write().expect("theme registry");
        registry.current = Some(std::sync::Arc::new(theme));
        registry.current_name = Some(if error.is_none() { name } else { "dark" }.to_string());
        registry.on_change.take()
    };
    if let Some(callback) = callback {
        callback();
        REGISTRY.write().expect("theme registry").on_change = Some(callback);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Install a theme instance directly (upstream `setThemeInstance`).
pub fn set_theme_instance(theme: std::sync::Arc<Theme>) {
    let callback = {
        let mut registry = REGISTRY.write().expect("theme registry");
        registry.current = Some(theme);
        registry.current_name = Some("<in-memory>".to_string());
        registry.on_change.take()
    };
    if let Some(callback) = callback {
        callback();
        REGISTRY.write().expect("theme registry").on_change = Some(callback);
    }
}

/// Register the theme-change callback (upstream `onThemeChange`).
pub fn on_theme_change(callback: Box<dyn Fn() + Send + Sync>) {
    REGISTRY.write().expect("theme registry").on_change = Some(callback);
}

/// File watching is not ported; provided so callers can mirror upstream
/// shutdown.
pub fn stop_theme_watcher() {}

// ============================================================================
// Component theme adapters + theme catalogue (upstream theme.ts tail)
// ============================================================================

/// A theme name plus where it was loaded from (upstream `ThemeInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<String>,
}

/// The user's agent directory: `PILLAR_CODING_AGENT_DIR` (with `~` expansion) or
/// `~/.pillar/agent` (upstream `config.ts::getAgentDir`).
pub fn agent_dir() -> String {
    if let Ok(dir) = std::env::var("PILLAR_CODING_AGENT_DIR") {
        if !dir.is_empty() {
            return expand_tilde_path(&dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
    format!("{}/.pillar/agent", home.trim_end_matches('/'))
}

/// Expand a leading `~` against `$HOME` (upstream `expandTildePath`).
pub fn expand_tilde_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
            return format!("{}{}", home.trim_end_matches('/'), rest);
        }
    }
    path.to_string()
}

/// The user's custom themes directory (upstream `getCustomThemesDir`).
pub fn custom_themes_dir() -> String {
    format!("{}/themes", agent_dir().trim_end_matches('/'))
}

/// The directory holding the shipped theme JSON files (upstream
/// `getThemesDir`).
///
/// divergence: the port embeds `dark.json` / `light.json` with
/// `include_str!`, so this path is informational (the theme list's `path`)
/// and resolves next to the executable or `$PI_PACKAGE_DIR`.
pub fn themes_dir() -> String {
    if let Ok(dir) = std::env::var("PILLAR_PACKAGE_DIR") {
        if !dir.is_empty() {
            return format!("{}/theme", dir.trim_end_matches('/'));
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.to_path_buf()))
        .map(|parent| parent.join("theme").to_string_lossy().to_string())
        .unwrap_or_else(|| "theme".to_string())
}

/// Custom theme files in the agent's theme directory (upstream
/// `getCustomThemeInfos`): invalid files are ignored here, like upstream.
fn custom_theme_infos() -> Vec<ThemeInfo> {
    get_custom_theme_infos_in(&custom_themes_dir())
}

/// [`custom_theme_infos`] for an explicit directory (test seam; upstream
/// reads the directory from `getCustomThemesDir()`).
pub fn get_custom_theme_infos_in(dir: &str) -> Vec<ThemeInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let path_string = path.to_string_lossy().to_string();
        if let Ok(theme) = load_theme_from_path(&path_string, ColorMode::Truecolor) {
            if let Some(name) = theme.name() {
                result.push(ThemeInfo {
                    name: name.to_string(),
                    path: Some(path_string),
                });
            }
        }
    }
    result
}

/// The registered (extension-provided) themes as name/path pairs.
fn registered_theme_infos() -> Vec<ThemeInfo> {
    REGISTRY
        .read()
        .expect("theme registry")
        .registered
        .iter()
        .map(|(name, theme)| ThemeInfo {
            name: name.clone(),
            path: theme.source_path().map(str::to_string),
        })
        .collect()
}

/// Every selectable theme name (upstream `getAvailableThemes`).
pub fn get_available_themes() -> Vec<String> {
    get_available_themes_with_paths()
        .into_iter()
        .map(|info| info.name)
        .collect()
}

/// Every selectable theme with its source path (upstream
/// `getAvailableThemesWithPaths`): built-ins first, then custom files, then
/// registered themes; duplicates keep their first source.
///
/// divergence: upstream sorts with `localeCompare`; the port sorts by code
/// point (identical for the built-in ASCII names).
pub fn get_available_themes_with_paths() -> Vec<ThemeInfo> {
    let themes_dir = themes_dir();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut result: Vec<ThemeInfo> = Vec::new();
    let mut add = |info: ThemeInfo| {
        if seen.insert(info.name.clone()) {
            result.push(info);
        }
    };

    for name in builtin_themes().keys() {
        add(ThemeInfo {
            name: name.clone(),
            path: Some(format!(
                "{}/{}.json",
                themes_dir.trim_end_matches('/'),
                name
            )),
        });
    }
    for info in custom_theme_infos() {
        add(info);
    }
    for info in registered_theme_infos() {
        add(info);
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Language id for a file path's extension (upstream `getLanguageFromPath`).
pub fn get_language_from_path(file_path: &str) -> Option<&'static str> {
    let ext = file_path.rsplit('.').next()?.to_lowercase();
    let lang = match ext.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    };
    Some(lang)
}

/// Highlight code for a markdown code block (upstream `highlightCode`).
///
/// divergence: upstream highlights with `highlight.js` (`cli-highlight`) and
/// falls back to painting every line with `mdCodeBlock` when the language is
/// unknown. No JS/JSX highlighter is ported, so the port always takes that
/// upstream fallback path; a Rust highlighter (e.g. syntect/tree-sitter) is a
/// follow-up.
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<String> {
    let _ = lang;
    code.split('\n')
        .map(|line| theme().fg("mdCodeBlock", line))
        .collect()
}

/// Markdown theme backed by the active theme (upstream `getMarkdownTheme`).
///
/// The closures read the global theme on every call, mirroring upstream's
/// `theme` proxy, so a theme switch affects components built earlier.
pub fn get_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(|text| theme().fg("mdHeading", text)),
        link: Box::new(|text| theme().fg("mdLink", text)),
        link_url: Box::new(|text| theme().fg("mdLinkUrl", text)),
        code: Box::new(|text| theme().fg("mdCode", text)),
        code_block: Box::new(|text| theme().fg("mdCodeBlock", text)),
        code_block_border: Box::new(|text| theme().fg("mdCodeBlockBorder", text)),
        quote: Box::new(|text| theme().fg("mdQuote", text)),
        quote_border: Box::new(|text| theme().fg("mdQuoteBorder", text)),
        hr: Box::new(|text| theme().fg("mdHr", text)),
        list_bullet: Box::new(|text| theme().fg("mdListBullet", text)),
        bold: Box::new(|text| theme().bold(text)),
        italic: Box::new(|text| theme().italic(text)),
        underline: Box::new(|text| theme().underline(text)),
        strikethrough: Box::new(|text| theme().strikethrough(text)),
        highlight_code: Some(Box::new(highlight_code)),
        code_block_indent: None,
    }
}

/// Select-list theme for the active theme (upstream `getSelectListTheme`).
pub fn get_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(|text| theme().fg("accent", text)),
        selected_text: Box::new(|text| theme().fg("accent", text)),
        description: Box::new(|text| theme().fg("muted", text)),
        scroll_info: Box::new(|text| theme().fg("muted", text)),
        no_match: Box::new(|text| theme().fg("muted", text)),
    }
}

/// Editor theme for the active theme (upstream `getEditorTheme`).
pub fn get_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Box::new(|text| theme().fg("borderMuted", text)),
        select_list: get_select_list_theme(),
    }
}

/// Settings-list theme for the active theme (upstream
/// `getSettingsListTheme`).
pub fn get_settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|text, selected| {
            if selected {
                theme().fg("accent", text)
            } else {
                text.to_string()
            }
        }),
        value: Box::new(|text, selected| {
            if selected {
                theme().fg("accent", text)
            } else {
                theme().fg("muted", text)
            }
        }),
        description: Box::new(|text| theme().fg("dim", text)),
        cursor: theme().fg("accent", "→ "),
        hint: Box::new(|text| theme().fg("dim", text)),
    }
}

// ============================================================================
// Terminal theme detection (upstream the detector interfaces + queries)
// ============================================================================

/// A terminal that answers the OSC 11 background query (upstream
/// `TerminalBackgroundThemeDetector`).
pub trait TerminalBackgroundThemeDetector {
    /// The terminal's background colour, if the terminal replied.
    fn query_terminal_background_color(&mut self, timeout: Duration) -> Option<RgbColor>;
}

/// Also answers the color-scheme query (upstream `TerminalAutoThemeDetector`).
///
/// divergence: upstream marks `queryTerminalColorScheme` optional (`?.`) and
/// treats a missing implementation as "no color scheme support"; the port
/// declares it required, so unsupported hosts return `None`.
pub trait TerminalAutoThemeDetector: TerminalBackgroundThemeDetector {
    /// The terminal's light/dark scheme, if the terminal replied.
    fn query_terminal_color_scheme(&mut self, timeout: Duration) -> Option<TerminalTheme>;
}

impl TerminalBackgroundThemeDetector for pillar_tui::tui::TuiBase {
    fn query_terminal_background_color(&mut self, timeout: Duration) -> Option<RgbColor> {
        pillar_tui::tui::TuiBase::query_terminal_background_color(self, timeout)
    }
}

impl TerminalAutoThemeDetector for pillar_tui::tui::TuiBase {
    fn query_terminal_color_scheme(&mut self, timeout: Duration) -> Option<TerminalTheme> {
        pillar_tui::tui::TuiBase::query_terminal_color_scheme(self, timeout).map(|scheme| {
            match scheme {
                TerminalColorScheme::Light => TerminalTheme::Light,
                TerminalColorScheme::Dark => TerminalTheme::Dark,
            }
        })
    }
}

fn process_env() -> ClipboardEnv {
    ClipboardEnv::from_process()
}

/// Detect the terminal background theme by querying OSC 11 and falling back to
/// the environment (upstream `detectTerminalBackgroundTheme`).
///
/// divergence: upstream awaits the query and catches thrown errors; the port's
/// query is blocking and answers `None`, so the same fallback runs.
pub fn detect_terminal_background_theme<D: TerminalBackgroundThemeDetector + ?Sized>(
    ui: &mut D,
    timeout_ms: u64,
    env: Option<&ClipboardEnv>,
) -> TerminalThemeDetection {
    if let Some(rgb) = ui.query_terminal_background_color(Duration::from_millis(timeout_ms)) {
        return TerminalThemeDetection {
            theme: get_theme_for_rgb_color(rgb.r, rgb.g, rgb.b),
            source: TerminalThemeSource::TerminalBackground,
            detail: format!("OSC 11 background rgb({}, {}, {})", rgb.r, rgb.g, rgb.b),
            confidence: TerminalThemeConfidence::High,
        };
    }
    let owned;
    let env = match env {
        Some(env) => env,
        None => {
            owned = process_env();
            &owned
        }
    };
    detect_terminal_background_from_env(env)
}

/// Detect the terminal theme for an automatic (`light/dark`) theme setting
/// (upstream `detectTerminalThemeForAuto`): the color-scheme report wins, then
/// the OSC 11 / `COLORFGBG` detection.
///
/// divergence: upstream starts both queries concurrently; the port's queries
/// are blocking, so the color scheme is asked first and the background query
/// runs only when it reports nothing.
pub fn detect_terminal_theme_for_auto<D: TerminalAutoThemeDetector + ?Sized>(
    ui: &mut D,
    timeout_ms: u64,
    env: Option<&ClipboardEnv>,
) -> TerminalTheme {
    if let Some(scheme) = ui.query_terminal_color_scheme(Duration::from_millis(timeout_ms)) {
        return scheme;
    }
    detect_terminal_background_theme(ui, timeout_ms, env).theme
}
