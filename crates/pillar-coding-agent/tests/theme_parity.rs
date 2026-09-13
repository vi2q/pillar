//! Parity tests for modes/interactive/theme/theme.ts (pi v0.84.3): colour
//! utilities, the Theme class, and built-in theme loading.

use std::collections::BTreeMap;

use pillar_coding_agent::modes::interactive::theme::{
    BG_COLOR_KEYS, ColorMode, ColorValue, REQUIRED_BG_COLORS, REQUIRED_FG_COLORS, ThemeJson,
    bg_ansi, builtin_themes, create_theme, fg_ansi, get_builtin_theme_names, get_theme_by_name,
    hex_to_256, hex_to_rgb, load_theme_from_path, rgb_to_256,
};

fn sample_theme_json() -> ThemeJson {
    let mut colors = BTreeMap::new();
    for key in [
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
    ] {
        colors.insert(key.to_string(), ColorValue::Hex("#112233".to_string()));
    }
    for key in [
        "selectedBg",
        "userMessageBg",
        "customMessageBg",
        "toolPendingBg",
        "toolSuccessBg",
        "toolErrorBg",
    ] {
        colors.insert(key.to_string(), ColorValue::Hex("#000000".to_string()));
    }
    ThemeJson {
        name: "sample".to_string(),
        vars: BTreeMap::new(),
        colors,
    }
}

#[test]
fn hex_to_rgb_parses_and_rejects() {
    assert_eq!(hex_to_rgb("#ff0000"), Ok((255, 0, 0)));
    assert_eq!(hex_to_rgb("00ff00"), Ok((0, 255, 0)));
    assert_eq!(hex_to_rgb("#112233"), Ok((17, 34, 51)));
    assert!(hex_to_rgb("#fff").is_err());
    assert!(hex_to_rgb("#gggggg").is_err());
}

#[test]
fn rgb_to_256_prefers_the_cube_but_uses_gray_for_neutrals() {
    // Pure primaries land on cube corners.
    assert_eq!(rgb_to_256(255, 0, 0), 196);
    assert_eq!(rgb_to_256(0, 255, 0), 46);
    assert_eq!(rgb_to_256(0, 0, 255), 21);
    // A near-neutral gray uses the 24-step ramp (232 + index 12).
    assert_eq!(rgb_to_256(128, 128, 128), 244);
    // A tinted color keeps the cube even when the gray ramp is close.
    assert_eq!(hex_to_256("#ff0000"), Ok(196));
}

#[test]
fn ansi_helpers_cover_defaults_indices_and_hex() {
    assert_eq!(
        fg_ansi(&ColorValue::Empty, ColorMode::Truecolor),
        Ok("\x1b[39m".to_string())
    );
    assert_eq!(
        bg_ansi(&ColorValue::Empty, ColorMode::Ansi256),
        Ok("\x1b[49m".to_string())
    );
    assert_eq!(
        fg_ansi(&ColorValue::Index(196), ColorMode::Ansi256),
        Ok("\x1b[38;5;196m".to_string())
    );
    assert_eq!(
        bg_ansi(&ColorValue::Index(21), ColorMode::Ansi256),
        Ok("\x1b[48;5;21m".to_string())
    );

    assert_eq!(
        fg_ansi(
            &ColorValue::Hex("#ff0000".to_string()),
            ColorMode::Truecolor
        ),
        Ok("\x1b[38;2;255;0;0m".to_string())
    );
    assert_eq!(
        bg_ansi(&ColorValue::Hex("#ff0000".to_string()), ColorMode::Ansi256),
        Ok("\x1b[48;5;196m".to_string())
    );
    // Ansi16 renders through the 256 path, like upstream `fgAnsi`.
    assert_eq!(
        fg_ansi(&ColorValue::Hex("#ff0000".to_string()), ColorMode::Ansi16),
        Ok("\x1b[38;5;196m".to_string())
    );
    assert!(
        fg_ansi(
            &ColorValue::Hex("primary".to_string()),
            ColorMode::Truecolor
        )
        .is_err()
    );
}

#[test]
fn builtin_themes_load_and_validate() {
    let themes = builtin_themes();
    assert_eq!(
        get_builtin_theme_names(),
        vec!["dark".to_string(), "light".to_string()]
    );
    for (name, theme) in themes {
        assert_eq!(theme.name, *name);
        for key in REQUIRED_FG_COLORS {
            assert!(theme.colors.contains_key(key), "{name} is missing {key}");
        }
        for key in REQUIRED_BG_COLORS {
            assert!(theme.colors.contains_key(key), "{name} is missing {key}");
        }
    }
}

#[test]
fn theme_colors_wrap_text_and_reset_the_single_channel() {
    let dark = get_theme_by_name("dark").expect("dark theme");
    let fg = dark.fg("accent", "hello");
    assert!(fg.starts_with("\x1b[38;"), "{fg:?}");
    assert!(fg.ends_with("hello\x1b[39m"), "{fg:?}");

    let bg = dark.bg("selectedBg", "hello");
    assert!(bg.starts_with("\x1b[4"), "{bg:?}");
    assert!(bg.ends_with("hello\x1b[49m"), "{bg:?}");

    assert_eq!(dark.bold("x"), "\x1b[1mx\x1b[22m");
    assert_eq!(dark.inverse("x"), "\x1b[7mx\x1b[27m");
    assert_eq!(dark.underline("x"), "\x1b[4mx\x1b[24m");
}

#[test]
fn optional_colors_fall_back_to_required_ones() {
    let theme_json = sample_theme_json();
    let theme = create_theme(&theme_json, ColorMode::Truecolor, None);
    // scrollbarThumb/searchMatchBg fall back to selectedBg.
    assert_eq!(theme.bg_ansi("scrollbarThumb"), theme.bg_ansi("selectedBg"));
    assert_eq!(theme.bg_ansi("searchMatchBg"), theme.bg_ansi("selectedBg"));
    // thinkingMax falls back to thinkingXhigh, searchMatchText to text.
    assert_eq!(theme.fg_ansi("thinkingMax"), theme.fg_ansi("thinkingXhigh"));
    assert_eq!(theme.fg_ansi("searchMatchText"), theme.fg_ansi("text"));
    assert_eq!(theme.color_mode(), ColorMode::Truecolor);
    assert_eq!(theme.name(), Some("sample"));
    assert_eq!(BG_COLOR_KEYS.len(), 8);
}

#[test]
fn thinking_and_bash_border_colors_follow_the_level() {
    let dark = get_theme_by_name("dark").expect("dark theme");
    assert_eq!(
        dark.thinking_border_color("high"),
        dark.fg_ansi("thinkingHigh")
    );
    assert_eq!(
        dark.thinking_border_color("max"),
        dark.fg_ansi("thinkingMax")
    );
    assert_eq!(
        dark.thinking_border_color("nonsense"),
        dark.fg_ansi("thinkingOff")
    );
    assert_eq!(dark.bash_mode_border_color(), dark.fg_ansi("bashMode"));
}

#[test]
fn unknown_colors_panic_like_upstream_throws() {
    let dark = get_theme_by_name("dark").expect("dark theme");
    let fg_panicked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dark.fg("nope", "x")));
    assert!(fg_panicked.is_err());
    let bg_panicked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dark.bg("nope", "x")));
    assert!(bg_panicked.is_err());
}

#[test]
fn var_references_resolve_and_cycles_fail() {
    let mut theme_json = sample_theme_json();
    theme_json.vars.insert(
        "primary".to_string(),
        ColorValue::Hex("#ff0000".to_string()),
    );
    theme_json
        .colors
        .insert("accent".to_string(), ColorValue::Hex("primary".to_string()));
    let theme = create_theme(&theme_json, ColorMode::Truecolor, None);
    assert_eq!(theme.fg_ansi("accent"), "\x1b[38;2;255;0;0m");

    // A circular reference makes the theme unbuildable (upstream throws).
    let mut cyclic = sample_theme_json();
    cyclic
        .vars
        .insert("a".to_string(), ColorValue::Hex("b".to_string()));
    cyclic
        .vars
        .insert("b".to_string(), ColorValue::Hex("a".to_string()));
    cyclic
        .colors
        .insert("accent".to_string(), ColorValue::Hex("a".to_string()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        create_theme(&cyclic, ColorMode::Truecolor, None)
    }));
    assert!(result.is_err());
}

#[test]
fn themes_load_from_a_file_and_report_missing_colors() {
    let dir = std::env::temp_dir().join(format!("pillar-theme-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("custom.json");
    let theme_json = sample_theme_json();
    let json = serde_json::json!({
        "name": theme_json.name,
        "colors": theme_json
            .colors
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    ColorValue::Hex(hex) => {
                        serde_json::Value::String(hex.clone())
                    }
                    ColorValue::Index(index) => {
                        serde_json::json!(index)
                    }
                    ColorValue::Empty => {
                        serde_json::Value::String(String::new())
                    }
                };
                (key.clone(), value)
            })
            .collect::<serde_json::Map<String, serde_json::Value>>(),
    });
    std::fs::write(&path, json.to_string()).expect("write theme");

    let theme = load_theme_from_path(&path.to_string_lossy(), ColorMode::Ansi256).expect("theme");
    assert_eq!(theme.name(), Some("sample"));
    assert_eq!(theme.source_path(), Some(path.to_string_lossy().as_ref()));

    // A document missing required colors is rejected with the upstream error.
    std::fs::write(
        &path,
        r##"{"name":"broken","colors":{"accent":"#fff000"}}"##,
    )
    .expect("write broken theme");
    let error =
        load_theme_from_path(&path.to_string_lossy(), ColorMode::Truecolor).expect_err("must fail");
    assert!(error.contains("missing color"), "{error}");

    // A missing file reports a read error.
    assert!(load_theme_from_path("/definitely/not/a/theme.json", ColorMode::Truecolor).is_err());
}
