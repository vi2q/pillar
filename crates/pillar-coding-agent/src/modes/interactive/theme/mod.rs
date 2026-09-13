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

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde_json::Value;

use crate::core::source_info::SourceInfo;

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

        Ok(ThemeJson {
            name: name.to_string(),
            vars,
            colors: parsed_colors,
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
