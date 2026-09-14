//! Port of components/keybinding-hints.ts: keybinding label formatting for the
//! UI ("ctrl+c cancel" style hints).

use crate::modes::interactive::theme::theme;
use pillar_tui::keybindings::with_global_keybindings;

/// Formatting options (upstream `KeyTextFormatOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyTextFormatOptions {
    pub capitalize: bool,
}

/// Format one key part: macOS shows "option" instead of "alt" (upstream
/// `formatKeyPart`).
fn format_key_part(part: &str, options: KeyTextFormatOptions) -> String {
    let display_part = if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
        "option"
    } else {
        part
    };
    if options.capitalize {
        let mut chars = display_part.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
            None => String::new(),
        }
    } else {
        display_part.to_string()
    }
}

/// Format a key spec like `ctrl+a/ctrl+b` (upstream `formatKeyText`).
pub fn format_key_text(key: &str, options: KeyTextFormatOptions) -> String {
    key.split('/')
        .map(|chord| {
            chord
                .split('+')
                .map(|part| format_key_part(part, options))
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn format_keys(keys: &[String], options: KeyTextFormatOptions) -> String {
    if keys.is_empty() {
        return String::new();
    }
    format_key_text(&keys.join("/"), options)
}

/// The keys bound to a keybinding (upstream `keyText`).
pub fn key_text(keybinding: &str) -> String {
    let keys = with_global_keybindings(|keybindings| keybindings.get_keys(keybinding));
    format_keys(&keys, KeyTextFormatOptions::default())
}

/// [`key_text`] with capitalized parts (upstream `keyDisplayText`).
pub fn key_display_text(keybinding: &str) -> String {
    let keys = with_global_keybindings(|keybindings| keybindings.get_keys(keybinding));
    format_keys(&keys, KeyTextFormatOptions { capitalize: true })
}

/// Dim keys plus a muted description (upstream `keyHint`).
pub fn key_hint(keybinding: &str, description: &str) -> String {
    let theme = theme();
    format!(
        "{}{}",
        theme.fg("dim", &key_text(keybinding)),
        theme.fg("muted", &format!(" {description}"))
    )
}

/// [`key_hint`] for a raw key spec (upstream `rawKeyHint`).
pub fn raw_key_hint(key: &str, description: &str) -> String {
    let theme = theme();
    format!(
        "{}{}",
        theme.fg(
            "dim",
            &format_key_text(key, KeyTextFormatOptions::default())
        ),
        theme.fg("muted", &format!(" {description}"))
    )
}
