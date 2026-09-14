//! Parity tests for the component theme adapters and theme catalogue in
//! modes/interactive/theme/theme.ts (pi v0.84.3): `getEditorTheme` /
//! `getSelectListTheme` / `getSettingsListTheme` / `getMarkdownTheme` /
//! `highlightCode` / `getLanguageFromPath` / `getAvailableThemes(WithPaths)`.
//!
//! These tests install a process-wide theme, so they are serialized with a
//! mutex (and kept in their own binary so they cannot disturb the
//! uninitialized-registry assertions in `theme_parity.rs`).

use std::sync::{Arc, Mutex};

use pillar_coding_agent::modes::interactive::theme::{
    ColorMode, REQUIRED_BG_COLORS, REQUIRED_FG_COLORS, ThemeJson, create_theme,
    get_available_themes, get_available_themes_with_paths, get_custom_theme_infos_in,
    get_editor_theme, get_language_from_path, get_markdown_theme, get_select_list_theme,
    get_settings_list_theme, get_theme_by_name, init_theme, set_registered_themes, theme,
};

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn install_dark() -> Arc<pillar_coding_agent::modes::interactive::theme::Theme> {
    init_theme(Some("dark"));
    Arc::new(get_theme_by_name("dark").expect("dark theme"))
}

/// A theme document with every required colour set, as JSON.
fn sample_theme_value(name: &str) -> serde_json::Value {
    let colors: serde_json::Map<String, serde_json::Value> = REQUIRED_FG_COLORS
        .iter()
        .chain(REQUIRED_BG_COLORS.iter())
        .map(|key| {
            (
                (*key).to_string(),
                serde_json::Value::String("#112233".to_string()),
            )
        })
        .collect();
    serde_json::json!({ "name": name, "colors": colors })
}

fn sample_theme_json(name: &str) -> ThemeJson {
    ThemeJson::parse("sample", &sample_theme_value(name)).expect("sample theme")
}

// --- getLanguageFromPath --------------------------------------------------------------------------

#[test]
fn language_lookup_matches_the_upstream_table() {
    let cases: &[(&str, &str)] = &[
        ("a.ts", "typescript"),
        ("a.tsx", "typescript"),
        ("a.jsx", "javascript"),
        ("a.mjs", "javascript"),
        ("a.cjs", "javascript"),
        ("a.py", "python"),
        ("a.rb", "ruby"),
        ("a.rs", "rust"),
        ("a.go", "go"),
        ("a.java", "java"),
        ("a.kt", "kotlin"),
        ("a.swift", "swift"),
        ("a.c", "c"),
        ("a.h", "c"),
        ("a.cpp", "cpp"),
        ("a.cc", "cpp"),
        ("a.cxx", "cpp"),
        ("a.hpp", "cpp"),
        ("a.cs", "csharp"),
        ("a.php", "php"),
        ("a.sh", "bash"),
        ("a.bash", "bash"),
        ("a.zsh", "bash"),
        ("a.fish", "fish"),
        ("a.ps1", "powershell"),
        ("a.sql", "sql"),
        ("a.html", "html"),
        ("a.htm", "html"),
        ("a.css", "css"),
        ("a.scss", "scss"),
        ("a.sass", "sass"),
        ("a.less", "less"),
        ("a.json", "json"),
        ("a.yaml", "yaml"),
        ("a.yml", "yaml"),
        ("a.toml", "toml"),
        ("a.xml", "xml"),
        ("a.md", "markdown"),
        ("a.markdown", "markdown"),
        ("a.dockerfile", "dockerfile"),
        ("a.makefile", "makefile"),
        ("a.cmake", "cmake"),
        ("a.lua", "lua"),
        ("a.perl", "perl"),
        ("a.r", "r"),
        ("a.scala", "scala"),
        ("a.clj", "clojure"),
        ("a.ex", "elixir"),
        ("a.exs", "elixir"),
        ("a.erl", "erlang"),
        ("a.hs", "haskell"),
        ("a.ml", "ocaml"),
        ("a.vim", "vim"),
        ("a.graphql", "graphql"),
        ("a.proto", "protobuf"),
        ("a.tf", "hcl"),
        ("a.hcl", "hcl"),
    ];
    for (path, expected) in cases {
        assert_eq!(get_language_from_path(path), Some(*expected), "{path}");
    }
    // The extension is matched case-insensitively; unknown ones yield none.
    assert_eq!(get_language_from_path("A.TS"), Some("typescript"));
    // A dotless name is its own "extension" (upstream `split(".").pop()`),
    // which is how the makefile/dockerfile entries are reachable.
    assert_eq!(get_language_from_path("Makefile"), Some("makefile"));
    assert_eq!(get_language_from_path("Dockerfile"), Some("dockerfile"));
    assert_eq!(get_language_from_path("archive.tar.gz"), None);
    assert_eq!(get_language_from_path(""), None);
}

// --- component themes -----------------------------------------------------------------------------

#[test]
fn select_list_theme_maps_the_active_theme() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let dark = install_dark();
    let select_list = get_select_list_theme();
    assert_eq!(
        (select_list.selected_prefix)("x"),
        dark.fg("accent", "x")
    );
    assert_eq!((select_list.selected_text)("x"), dark.fg("accent", "x"));
    assert_eq!((select_list.description)("x"), dark.fg("muted", "x"));
    assert_eq!((select_list.scroll_info)("x"), dark.fg("muted", "x"));
    assert_eq!((select_list.no_match)("x"), dark.fg("muted", "x"));
}

#[test]
fn editor_theme_uses_the_muted_border_and_select_list() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let dark = install_dark();
    let editor = get_editor_theme();
    assert_eq!((editor.border_color)("─"), dark.fg("borderMuted", "─"));
    assert_eq!(
        (editor.select_list.selected_text)("x"),
        dark.fg("accent", "x")
    );
}

#[test]
fn settings_list_theme_highlights_only_the_selected_row() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let dark = install_dark();
    let settings = get_settings_list_theme();
    // An unselected label is passed through untouched.
    assert_eq!((settings.label)("Name", false), "Name");
    assert_eq!((settings.label)("Name", true), dark.fg("accent", "Name"));
    assert_eq!((settings.value)("on", false), dark.fg("muted", "on"));
    assert_eq!((settings.value)("on", true), dark.fg("accent", "on"));
    assert_eq!((settings.description)("hint"), dark.fg("dim", "hint"));
    assert_eq!((settings.hint)("hint"), dark.fg("dim", "hint"));
    assert_eq!(settings.cursor, dark.fg("accent", "→ "));
}

#[test]
fn markdown_theme_maps_every_element_and_its_highlighter() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let dark = install_dark();
    let markdown = get_markdown_theme();
    assert_eq!((markdown.heading)("t"), dark.fg("mdHeading", "t"));
    assert_eq!((markdown.link)("t"), dark.fg("mdLink", "t"));
    assert_eq!((markdown.link_url)("t"), dark.fg("mdLinkUrl", "t"));
    assert_eq!((markdown.code)("t"), dark.fg("mdCode", "t"));
    assert_eq!((markdown.code_block)("t"), dark.fg("mdCodeBlock", "t"));
    assert_eq!(
        (markdown.code_block_border)("t"),
        dark.fg("mdCodeBlockBorder", "t")
    );
    assert_eq!((markdown.quote)("t"), dark.fg("mdQuote", "t"));
    assert_eq!((markdown.quote_border)("t"), dark.fg("mdQuoteBorder", "t"));
    assert_eq!((markdown.hr)("t"), dark.fg("mdHr", "t"));
    assert_eq!((markdown.list_bullet)("t"), dark.fg("mdListBullet", "t"));
    assert_eq!((markdown.bold)("t"), "\u{1b}[1mt\u{1b}[22m");
    assert_eq!((markdown.italic)("t"), "\u{1b}[3mt\u{1b}[23m");
    assert_eq!((markdown.underline)("t"), "\u{1b}[4mt\u{1b}[24m");
    assert_eq!((markdown.strikethrough)("t"), "\u{1b}[9mt\u{1b}[29m");
    // `getMarkdownTheme` leaves the code-block indent unset (interactive-mode
    // adds the settings value).
    assert_eq!(markdown.code_block_indent, None);
    assert!(markdown.highlight_code.is_some());
}

#[test]
fn highlight_code_uses_the_upstream_no_language_fallback() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let dark = install_dark();
    let markdown = get_markdown_theme();
    let highlight = markdown.highlight_code.as_ref().expect("highlighter");
    // divergence: no JS highlighter is ported, so every language takes
    // upstream's "unknown language" path (plain `mdCodeBlock` painting).
    assert_eq!(
        highlight("let x = 1;\nlet y = 2;", Some("rust")),
        vec![
            dark.fg("mdCodeBlock", "let x = 1;"),
            dark.fg("mdCodeBlock", "let y = 2;"),
        ]
    );
    assert_eq!(highlight("", None), vec![dark.fg("mdCodeBlock", "")]);
}

// --- theme catalogue ------------------------------------------------------------------------------

#[test]
fn available_themes_list_builtins_sorted_with_paths() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let _ = set_registered_themes(vec![]);
    let names = get_available_themes();
    assert_eq!(names, vec!["dark".to_string(), "light".to_string()]);

    let infos = get_available_themes_with_paths();
    assert_eq!(infos.len(), 2);
    let dark = infos.iter().find(|info| info.name == "dark").expect("dark");
    let path = dark.path.as_deref().expect("path");
    assert!(path.ends_with("/theme/dark.json"), "{path}");
    assert_eq!(infos[0].name, "dark", "sorted by name");
}

#[test]
fn registered_themes_join_the_catalogue_with_their_source_path() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let _ = set_registered_themes(vec![]);
    let registered = create_theme(
        &sample_theme_json("zeta"),
        ColorMode::Truecolor,
        Some("/tmp/zeta.json"),
    );
    set_registered_themes(vec![Arc::new(registered)]).expect("register");

    let infos = get_available_themes_with_paths();
    let zeta = infos.iter().find(|info| info.name == "zeta").expect("zeta");
    assert_eq!(zeta.path.as_deref(), Some("/tmp/zeta.json"));
    // Sorted: built-ins first alphabetically, the registered theme last here.
    assert_eq!(
        infos.iter().map(|info| info.name.as_str()).collect::<Vec<_>>(),
        vec!["dark", "light", "zeta"]
    );

    let _ = set_registered_themes(vec![]);
}

#[test]
fn custom_theme_files_are_discovered_and_invalid_ones_ignored() {
    let dir = std::env::temp_dir().join(format!("pillar-theme-adapters-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let valid = dir.join("mine.json");
    std::fs::write(&valid, sample_theme_value("mine").to_string()).expect("write theme");

    // A non-JSON file and an invalid JSON theme are skipped.
    std::fs::write(dir.join("notes.txt"), "hello").expect("write text");
    std::fs::write(dir.join("broken.json"), r##"{"name":"broken"}"##).expect("write broken");

    let infos = get_custom_theme_infos_in(&dir.to_string_lossy());
    assert_eq!(infos.len(), 1, "{infos:?}");
    assert_eq!(infos[0].name, "mine");
    assert_eq!(
        infos[0].path.as_deref(),
        Some(valid.to_string_lossy().as_ref())
    );

    // A missing directory yields an empty list (upstream returns early).
    assert!(get_custom_theme_infos_in("/definitely/not/a/dir").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tilde_expansion_matches_upstream() {
    use pillar_coding_agent::modes::interactive::theme::{agent_dir, expand_tilde_path};
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(expand_tilde_path("~"), home);
    assert_eq!(expand_tilde_path("~/themes"), format!("{home}/themes"));
    assert_eq!(expand_tilde_path("~user/themes"), "~user/themes");
    assert_eq!(expand_tilde_path("/abs/path"), "/abs/path");
    // The agent dir honours PI_CODING_AGENT_DIR when it is set.
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        if !dir.is_empty() {
            assert_eq!(agent_dir(), expand_tilde_path(&dir));
        }
    } else {
        assert_eq!(agent_dir(), format!("{home}/.pi/agent"));
    }
    // The global accessor is initialized by the adapters' helpers too.
    let _guard = THEME_LOCK.lock().expect("theme lock");
    init_theme(Some("light"));
    assert_eq!(theme().name(), Some("light"));
}
