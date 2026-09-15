//! Port of the upstream keybindings tests (pi v0.84.3, packages/tui/test/
//! keybindings.test.ts): default bindings, per-binding user overrides, and
//! direct conflict detection.

use pillar_tui::keybindings::{KeybindingsConfig, KeybindingsManager, tui_keybindings};
use pillar_tui::keys::matches_key;

fn config(entries: &[(&str, Vec<&str>)]) -> KeybindingsConfig {
    entries
        .iter()
        .map(|(id, keys)| {
            (
                id.to_string(),
                keys.iter().map(|key| key.to_string()).collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn keys_list(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|key| key.to_string()).collect()
}

#[test]
fn binds_ctrl_j_as_a_default_newline_alias() {
    let keybindings = KeybindingsManager::new(tui_keybindings(), KeybindingsConfig::new());

    assert_eq!(
        keybindings.get_keys("tui.input.newLine"),
        keys_list(&["shift+enter", "ctrl+j"])
    );
    assert!(keybindings.matches("\n", "tui.input.newLine"));
    assert!(keybindings.matches("\x1b[106;5u", "tui.input.newLine"));
}

#[test]
fn binds_modified_and_unmodified_editor_viewport_navigation() {
    let keybindings = KeybindingsManager::new(tui_keybindings(), KeybindingsConfig::new());

    assert_eq!(
        keybindings.get_keys("tui.editor.cursorLineStart"),
        keys_list(&["home", "ctrl+home", "ctrl+a"])
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.cursorLineEnd"),
        keys_list(&["end", "ctrl+end", "ctrl+e"])
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.pageUp"),
        keys_list(&["pageUp", "ctrl+pageUp"])
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.pageDown"),
        keys_list(&["pageDown", "ctrl+pageDown"])
    );
}

#[test]
fn leaves_dedicated_prompt_history_navigation_unbound_by_default() {
    let keybindings = KeybindingsManager::new(tui_keybindings(), KeybindingsConfig::new());

    assert_eq!(
        keybindings.get_keys("tui.editor.historyPrevious"),
        Vec::<String>::new()
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.historyNext"),
        Vec::<String>::new()
    );
}

#[test]
fn binds_unmodified_terminal_viewport_shortcuts_to_alternate_screen_navigation() {
    let keybindings = KeybindingsManager::new(tui_keybindings(), KeybindingsConfig::new());

    assert_eq!(
        keybindings.get_keys("tui.altScreen.pageUp"),
        keys_list(&["pageUp"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.pageDown"),
        keys_list(&["pageDown"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.halfPageUp"),
        Vec::<String>::new()
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.halfPageDown"),
        Vec::<String>::new()
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.lineUp"),
        Vec::<String>::new()
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.lineDown"),
        Vec::<String>::new()
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.previousPrompt"),
        keys_list(&["ctrl+shift+up", "ctrl+up"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.nextPrompt"),
        keys_list(&["ctrl+shift+down", "ctrl+down"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.search"),
        keys_list(&["ctrl+shift+f"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.searchNext"),
        keys_list(&["enter", "ctrl+g"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.searchPrevious"),
        keys_list(&["shift+enter", "ctrl+shift+g"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.searchClose"),
        keys_list(&["escape"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.top"),
        keys_list(&["home"])
    );
    assert_eq!(
        keybindings.get_keys("tui.altScreen.bottom"),
        keys_list(&["end"])
    );
}

#[test]
fn does_not_evict_selector_confirm_when_input_submit_is_rebound() {
    let keybindings = KeybindingsManager::new(
        tui_keybindings(),
        config(&[("tui.input.submit", vec!["enter", "ctrl+enter"])]),
    );

    assert_eq!(
        keybindings.get_keys("tui.input.submit"),
        keys_list(&["enter", "ctrl+enter"])
    );
    assert_eq!(
        keybindings.get_keys("tui.select.confirm"),
        keys_list(&["enter"])
    );
}

#[test]
fn does_not_evict_cursor_bindings_when_another_action_reuses_the_same_key() {
    let keybindings = KeybindingsManager::new(
        tui_keybindings(),
        config(&[("tui.select.up", vec!["up", "ctrl+p"])]),
    );

    assert_eq!(
        keybindings.get_keys("tui.select.up"),
        keys_list(&["up", "ctrl+p"])
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.cursorUp"),
        keys_list(&["up"])
    );
}

#[test]
fn still_reports_direct_user_binding_conflicts_without_evicting_defaults() {
    let keybindings = KeybindingsManager::new(
        tui_keybindings(),
        config(&[
            ("tui.input.submit", vec!["ctrl+x"]),
            ("tui.select.confirm", vec!["ctrl+x"]),
        ]),
    );

    let conflicts = keybindings.get_conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].key, "ctrl+x");
    assert_eq!(
        conflicts[0].keybindings,
        vec![
            "tui.input.submit".to_string(),
            "tui.select.confirm".to_string()
        ]
    );
    assert_eq!(
        keybindings.get_keys("tui.editor.cursorLeft"),
        keys_list(&["left", "ctrl+b"])
    );
}

#[test]
fn resolved_bindings_cover_every_definition() {
    let keybindings = KeybindingsManager::new(tui_keybindings(), KeybindingsConfig::new());
    let resolved = keybindings.get_resolved_bindings();
    assert_eq!(resolved.len(), tui_keybindings().len());
}

// --- matches_key core matching ----------------------------------------------

#[test]
fn matches_legacy_and_ctrl_keys() {
    assert!(matches_key("\x1b[A", "up"));
    assert!(matches_key("\x1bOA", "up"));
    assert!(matches_key("\x1b", "escape"));
    assert!(matches_key("\t", "tab"));
    assert!(matches_key("\r", "enter"));
    assert!(matches_key("\x07", "ctrl+g"));
    assert!(matches_key("g", "g"));
    assert!(matches_key("G", "shift+g"));
    assert!(matches_key("\x1b[Z", "shift+tab"));
    assert!(!matches_key("\x1b[A", "down"));
    assert!(!matches_key("x", "ctrl+x"));
}

/// Kitty-protocol arrows carry a modifier field and, with flag 2, an event
/// type (`CSI 1;1:1B`) — that is what Ghostty/kitty send once the protocol is
/// negotiated. They must match the plain arrow bindings like upstream's
/// `arrowMatch` does.
#[test]
fn kitty_protocol_arrows_match_the_arrow_bindings() {
    for (sequence, binding) in [
        ("\u{1b}[1;1B", "down"),
        ("\u{1b}[1;1:1B", "down"),
        ("\u{1b}[1;1A", "up"),
        ("\u{1b}[1;1:1A", "up"),
        ("\u{1b}[1;1C", "right"),
        ("\u{1b}[1;1:1C", "right"),
        ("\u{1b}[1;1D", "left"),
        ("\u{1b}[1;1:1D", "left"),
        ("\u{1b}[1;1H", "home"),
        ("\u{1b}[1;1:1F", "end"),
        ("\u{1b}[5;1:1~", "pageUp"),
        ("\u{1b}[6;1:1~", "pageDown"),
    ] {
        assert!(
            matches_key(sequence, binding),
            "{sequence:?} should match {binding}"
        );
    }

    // Modifiers still select only the modified binding.
    assert!(matches_key("\u{1b}[1;2:1B", "shift+down"));
    assert!(!matches_key("\u{1b}[1;2:1B", "down"));

    // A release event parses as the key (the host filters it with
    // `is_key_release` before any action runs).
    assert!(matches_key("\u{1b}[1;1:3B", "down"));
    assert!(pillar_tui::tui::is_key_release("\u{1b}[1;1:3B"));
    assert!(!pillar_tui::tui::is_key_release("\u{1b}[1;1:1B"));
}
