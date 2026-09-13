//! Parity tests for modes/interactive/theme/theme.ts (pi v0.84.3): colour
//! utilities, the Theme class, and built-in theme loading.

use std::collections::BTreeMap;

use pillar_coding_agent::modes::interactive::theme::{
    BG_COLOR_KEYS, ColorMode, ColorValue, REQUIRED_BG_COLORS, REQUIRED_FG_COLORS, TerminalTheme,
    TerminalThemeConfidence, TerminalThemeSource, ThemeJson, ansi256_to_hex,
    assert_theme_name_is_valid, bg_ansi, builtin_themes, create_theme, current_theme_name,
    detect_terminal_background_from_env, fg_ansi, get_builtin_theme_names,
    get_color_fg_bg_background_index, get_resolved_theme_colors, get_theme_by_name,
    get_theme_export_colors, get_theme_for_rgb_color, hex_to_256, hex_to_rgb, init_theme,
    is_light_theme, load_theme_from_path, load_theme_json, on_theme_change,
    parse_auto_theme_setting, resolve_theme_setting, rgb_color_luminance, rgb_to_256,
    set_registered_themes, set_theme, set_theme_instance, stop_theme_watcher, theme,
};
use pillar_coding_agent::utils::clipboard::ClipboardEnv;

fn env(pairs: &[(&str, &str)]) -> ClipboardEnv {
    ClipboardEnv::new(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

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
        export: BTreeMap::new(),
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

#[test]
fn auto_theme_settings_parse_and_resolve() {
    let auto = parse_auto_theme_setting(Some("light/dark")).expect("auto pair");
    assert_eq!(auto.light_theme, "light");
    assert_eq!(auto.dark_theme, "dark");

    let trimmed = parse_auto_theme_setting(Some(" light / dark ")).expect("trimmed pair");
    assert_eq!(trimmed.light_theme, "light");
    assert_eq!(trimmed.dark_theme, "dark");

    // Anything other than exactly one slash is not an auto setting.
    assert!(parse_auto_theme_setting(None).is_none());
    assert!(parse_auto_theme_setting(Some("a/b/c")).is_none());
    assert!(parse_auto_theme_setting(Some("/dark")).is_none());
    assert!(parse_auto_theme_setting(Some("light/")).is_none());
    assert!(parse_auto_theme_setting(Some("dark")).is_none());

    assert_eq!(
        resolve_theme_setting(Some("light/dark"), TerminalTheme::Light),
        Some("light".to_string())
    );
    assert_eq!(
        resolve_theme_setting(Some("light/dark"), TerminalTheme::Dark),
        Some("dark".to_string())
    );
    assert_eq!(
        resolve_theme_setting(Some("solarized"), TerminalTheme::Dark),
        Some("solarized".to_string())
    );
    assert_eq!(
        resolve_theme_setting(Some("a/b/c"), TerminalTheme::Dark),
        None
    );
    assert_eq!(resolve_theme_setting(None, TerminalTheme::Dark), None);
}

#[test]
fn colorfgbg_and_luminance_detection() {
    assert_eq!(get_color_fg_bg_background_index("0;15"), Some(15));
    assert_eq!(get_color_fg_bg_background_index("15;0"), Some(0));
    // Out-of-range and non-numeric entries are skipped, scanning from the end.
    assert_eq!(get_color_fg_bg_background_index("0;300"), Some(0));
    assert_eq!(get_color_fg_bg_background_index("nope"), None);
    assert_eq!(get_color_fg_bg_background_index(""), None);

    assert!(rgb_color_luminance(0, 0, 0) < 0.001);
    assert!(rgb_color_luminance(255, 255, 255) > 0.999);
    assert_eq!(get_theme_for_rgb_color(255, 255, 255), TerminalTheme::Light);
    assert_eq!(get_theme_for_rgb_color(0, 0, 0), TerminalTheme::Dark);

    // 16 basic colours, the cube, and the gray ramp.
    assert_eq!(ansi256_to_hex(0), "#000000");
    assert_eq!(ansi256_to_hex(15), "#ffffff");
    assert_eq!(ansi256_to_hex(196), "#ff0000");
    assert_eq!(ansi256_to_hex(244), "#808080");
    assert_eq!(ansi256_to_hex(255), "#eeeeee");
}

#[test]
fn terminal_background_detection_from_env() {
    let light = detect_terminal_background_from_env(&env(&[("COLORFGBG", "0;15")]));
    assert_eq!(light.theme, TerminalTheme::Light);
    assert_eq!(light.source, TerminalThemeSource::ColorFgBg);
    assert_eq!(light.confidence, TerminalThemeConfidence::High);
    assert_eq!(light.detail, "background color index 15");

    let fallback = detect_terminal_background_from_env(&env(&[]));
    assert_eq!(fallback.theme, TerminalTheme::Dark);
    assert_eq!(fallback.source, TerminalThemeSource::Fallback);
    assert_eq!(fallback.confidence, TerminalThemeConfidence::Low);
    assert_eq!(fallback.detail, "no terminal background hint found");
    assert_eq!(fallback.source.as_str(), "fallback");
    assert_eq!(
        TerminalThemeSource::TerminalBackground.as_str(),
        "terminal background"
    );
    assert_eq!(TerminalTheme::Light.as_str(), "light");
}

#[test]
fn theme_names_reject_slashes() {
    assert!(assert_theme_name_is_valid("dark").is_ok());
    let error = assert_theme_name_is_valid("light/dark").expect_err("slash is reserved");
    assert!(
        error.contains("Invalid theme name \"light/dark\""),
        "{error}"
    );
    assert!(
        error.contains("automatic light/dark theme settings"),
        "{error}"
    );
}

#[test]
fn export_helpers_produce_css_colors_and_light_flags() {
    let dark = get_resolved_theme_colors(Some("dark")).expect("dark colors");
    assert!(dark["accent"].starts_with('#'), "{}", dark["accent"]);
    assert!(dark.contains_key("selectedBg"));
    // Empty values fall back to the per-brightness default text colour.
    assert_eq!(dark.get("searchMatchText"), dark.get("text"));

    let light = get_resolved_theme_colors(Some("light")).expect("light colors");
    assert!(light.contains_key("text"));

    assert!(is_light_theme(Some("light")));
    assert!(!is_light_theme(Some("dark")));
    assert!(!is_light_theme(None));

    // The built-in dark theme declares its export colours.
    let export = get_theme_export_colors(Some("dark"));
    assert!(
        export["pageBg"]
            .as_deref()
            .is_some_and(|hex| hex.starts_with('#')),
        "{export:?}"
    );
    assert!(
        export["cardBg"]
            .as_deref()
            .is_some_and(|hex| hex.starts_with('#'))
    );
    assert!(
        export["infoBg"]
            .as_deref()
            .is_some_and(|hex| hex.starts_with('#'))
    );
    // An unknown theme yields the same empty shape instead of failing.
    let unknown = get_theme_export_colors(Some("nope"));
    assert_eq!(unknown["pageBg"], None);
    assert_eq!(unknown["cardBg"], None);
    assert!(get_resolved_theme_colors(Some("nope")).is_err());
    assert_eq!(
        load_theme_json("nope").unwrap_err(),
        "Theme not found: nope"
    );
}

/// Global-state test: kept in one test so the registry is only mutated here
/// (tests inside a binary run in parallel).
#[test]
fn global_theme_registry_switches_and_notifies() {
    // Not initialized yet -> upstream's proxy error (panic).
    let uninitialized = std::panic::catch_unwind(theme);
    assert!(uninitialized.is_err());
    assert_eq!(current_theme_name(), None);

    init_theme(Some("light"));
    assert_eq!(current_theme_name(), Some("light".to_string()));
    assert_eq!(theme().name(), Some("light"));

    // An invalid name falls back to dark.
    init_theme(Some("does-not-exist"));
    assert_eq!(current_theme_name(), Some("dark".to_string()));

    let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&notified);
    on_theme_change(Box::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    set_theme("light").expect("switch to light");
    assert_eq!(current_theme_name(), Some("light".to_string()));
    assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 1);

    let error = set_theme("missing").expect_err("missing theme");
    assert!(error.starts_with("Theme not found:"), "{error}");
    assert_eq!(current_theme_name(), Some("dark".to_string()));
    assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Registered themes win over built-ins and can be installed directly.
    let custom = create_theme(&sample_theme_json(), ColorMode::Truecolor, None);
    set_registered_themes(vec![std::sync::Arc::new(custom.clone())]).expect("register");
    set_theme("sample").expect("registered theme");
    assert_eq!(theme().name(), Some("sample"));

    let in_memory = std::sync::Arc::new(custom);
    set_theme_instance(in_memory);
    assert_eq!(current_theme_name(), Some("<in-memory>".to_string()));
    assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 4);

    assert!(set_registered_themes(vec![]).is_ok());
    stop_theme_watcher();
}
