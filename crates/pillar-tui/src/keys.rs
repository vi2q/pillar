//! Port of packages/tui/src/keys.ts (pi v0.84.3) — the key-matching core.
//!
//! Matches raw terminal input against key identifiers. Supports legacy
//! sequences, Kitty keyboard protocol (CSI u), and xterm modifyOtherKeys.
//!
//! divergence: terminal input parsing lives in the host; the Kitty protocol
//! active flag defaults to `false` and is set via [`set_kitty_protocol_active`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Global Kitty keyboard protocol state (upstream `_kittyProtocolActive`).
static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Called by the terminal after detecting protocol support.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::SeqCst);
}

/// Query whether Kitty keyboard protocol is currently active.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MOD_SHIFT: u8 = 1;
const MOD_ALT: u8 = 2;
const MOD_CTRL: u8 = 4;
const MOD_SUPER: u8 = 8;
const LOCK_MASK: u8 = 64 + 128; // Caps Lock + Num Lock

fn cp(name: &str) -> i32 {
    match name {
        "escape" => 27,
        "tab" => 9,
        "enter" => 13,
        "space" => 32,
        "backspace" => 127,
        "kpEnter" => 57414,
        _ => unreachable!("unknown codepoint {name}"),
    }
}

const ARROW_UP: i32 = -1;
const ARROW_DOWN: i32 = -2;
const ARROW_RIGHT: i32 = -3;
const ARROW_LEFT: i32 = -4;

const FUNCTIONAL_DELETE: i32 = -10;
const FUNCTIONAL_INSERT: i32 = -11;
const FUNCTIONAL_PAGE_UP: i32 = -12;
const FUNCTIONAL_PAGE_DOWN: i32 = -13;
const FUNCTIONAL_HOME: i32 = -14;
const FUNCTIONAL_END: i32 = -15;

fn symbol_keys() -> &'static std::collections::BTreeSet<char> {
    static SYMBOL_KEYS: OnceLock<std::collections::BTreeSet<char>> = OnceLock::new();
    SYMBOL_KEYS.get_or_init(|| "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?".chars().collect())
}

fn kitty_functional_equivalents(codepoint: i32) -> i32 {
    match codepoint {
        57399 => 48, // KP_0
        57400 => 49, // KP_1
        57401 => 50, // KP_2
        57402 => 51, // KP_3
        57403 => 52, // KP_4
        57404 => 53, // KP_5
        57405 => 54, // KP_6
        57406 => 55, // KP_7
        57407 => 56, // KP_8
        57408 => 57, // KP_9
        57409 => 46, // KP_DECIMAL
        57410 => 47, // KP_DIVIDE
        57411 => 42, // KP_MULTIPLY
        57412 => 45, // KP_SUBTRACT
        57413 => 43, // KP_ADD
        57415 => 61, // KP_EQUAL
        57416 => 44, // KP_SEPARATOR
        57417 => ARROW_LEFT,
        57418 => ARROW_RIGHT,
        57419 => ARROW_UP,
        57420 => ARROW_DOWN,
        57421 => FUNCTIONAL_PAGE_UP,
        57422 => FUNCTIONAL_PAGE_DOWN,
        57423 => FUNCTIONAL_HOME,
        57424 => FUNCTIONAL_END,
        57425 => FUNCTIONAL_INSERT,
        57426 => FUNCTIONAL_DELETE,
        other => other,
    }
}

fn normalize_kitty_functional_codepoint(codepoint: i32) -> i32 {
    kitty_functional_equivalents(codepoint)
}

fn normalize_shifted_letter_identity_codepoint(codepoint: i32, modifier: u8) -> i32 {
    let effective_modifier = modifier & !LOCK_MASK;
    if (effective_modifier & MOD_SHIFT) != 0 && (65..=90).contains(&codepoint) {
        return codepoint + 32;
    }
    codepoint
}

/// Legacy escape sequences per key (upstream `LEGACY_KEY_SEQUENCES`).
fn legacy_sequences(key: &str) -> &'static [&'static str] {
    const UP: &[&str] = &["\x1b[A", "\x1bOA"];
    const DOWN: &[&str] = &["\x1b[B", "\x1bOB"];
    const RIGHT: &[&str] = &["\x1b[C", "\x1bOC"];
    const LEFT: &[&str] = &["\x1b[D", "\x1bOD"];
    const HOME: &[&str] = &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"];
    const END: &[&str] = &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"];
    const INSERT: &[&str] = &["\x1b[2~"];
    const DELETE: &[&str] = &["\x1b[3~"];
    const PAGE_UP: &[&str] = &["\x1b[5~", "\x1b[[5~"];
    const PAGE_DOWN: &[&str] = &["\x1b[6~", "\x1b[[6~"];
    const CLEAR: &[&str] = &["\x1b[E", "\x1bOE"];
    const F1: &[&str] = &["\x1bOP", "\x1b[11~", "\x1b[[A"];
    const F2: &[&str] = &["\x1bOQ", "\x1b[12~", "\x1b[[B"];
    const F3: &[&str] = &["\x1bOR", "\x1b[13~", "\x1b[[C"];
    const F4: &[&str] = &["\x1bOS", "\x1b[14~", "\x1b[[D"];
    const F5: &[&str] = &["\x1b[15~", "\x1b[[E"];
    const F6: &[&str] = &["\x1b[17~"];
    const F7: &[&str] = &["\x1b[18~"];
    const F8: &[&str] = &["\x1b[19~"];
    const F9: &[&str] = &["\x1b[20~"];
    const F10: &[&str] = &["\x1b[21~"];
    const F11: &[&str] = &["\x1b[23~"];
    const F12: &[&str] = &["\x1b[24~"];
    match key {
        "up" => UP,
        "down" => DOWN,
        "right" => RIGHT,
        "left" => LEFT,
        "home" => HOME,
        "end" => END,
        "insert" => INSERT,
        "delete" => DELETE,
        "pageup" => PAGE_UP,
        "pagedown" => PAGE_DOWN,
        "clear" => CLEAR,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        _ => &[],
    }
}

/// Shift+key legacy sequences (upstream `LEGACY_SHIFT_SEQUENCES`).
fn legacy_shift_sequences(key: &str) -> &'static [&'static str] {
    const UP: &[&str] = &["\x1b[a"];
    const DOWN: &[&str] = &["\x1b[b"];
    const RIGHT: &[&str] = &["\x1b[c"];
    const LEFT: &[&str] = &["\x1b[d"];
    const CLEAR: &[&str] = &["\x1b[e"];
    const INSERT: &[&str] = &["\x1b[2$"];
    const DELETE: &[&str] = &["\x1b[3$"];
    const PAGE_UP: &[&str] = &["\x1b[5$"];
    const PAGE_DOWN: &[&str] = &["\x1b[6$"];
    const HOME: &[&str] = &["\x1b[7$"];
    const END: &[&str] = &["\x1b[8$"];
    match key {
        "up" => UP,
        "down" => DOWN,
        "right" => RIGHT,
        "left" => LEFT,
        "clear" => CLEAR,
        "insert" => INSERT,
        "delete" => DELETE,
        "pageup" => PAGE_UP,
        "pagedown" => PAGE_DOWN,
        "home" => HOME,
        "end" => END,
        _ => &[],
    }
}

/// Ctrl+key legacy sequences (upstream `LEGACY_CTRL_SEQUENCES`).
fn legacy_ctrl_sequences(key: &str) -> &'static [&'static str] {
    const UP: &[&str] = &["\x1bOa"];
    const DOWN: &[&str] = &["\x1bOb"];
    const RIGHT: &[&str] = &["\x1bOc"];
    const LEFT: &[&str] = &["\x1bOd"];
    const CLEAR: &[&str] = &["\x1bOe"];
    const INSERT: &[&str] = &["\x1b[2^"];
    const DELETE: &[&str] = &["\x1b[3^"];
    const PAGE_UP: &[&str] = &["\x1b[5^"];
    const PAGE_DOWN: &[&str] = &["\x1b[6^"];
    const HOME: &[&str] = &["\x1b[7^"];
    const END: &[&str] = &["\x1b[8^"];
    match key {
        "up" => UP,
        "down" => DOWN,
        "right" => RIGHT,
        "left" => LEFT,
        "clear" => CLEAR,
        "insert" => INSERT,
        "delete" => DELETE,
        "pageup" => PAGE_UP,
        "pagedown" => PAGE_DOWN,
        "home" => HOME,
        "end" => END,
        _ => &[],
    }
}

const MATCHES_LEGACY_SEQUENCE: fn(&str, &[&str]) -> bool =
    |data, sequences| sequences.contains(&data);

fn matches_legacy_modifier_sequence(data: &str, key: &str, modifier: u8) -> bool {
    match modifier {
        MOD_SHIFT => MATCHES_LEGACY_SEQUENCE(data, legacy_shift_sequences(key)),
        MOD_CTRL => MATCHES_LEGACY_SEQUENCE(data, legacy_ctrl_sequences(key)),
        _ => false,
    }
}

/// Upstream `rawCtrlChar`: control character for a printable key.
fn raw_ctrl_char(key: &str) -> Option<char> {
    let ch = key.chars().next()?;
    let lower = ch.to_ascii_lowercase();
    let code = lower as u32;
    if (97..=122).contains(&code) || matches!(lower, '[' | '\\' | ']' | '_') {
        return char::from_u32(code & 0x1f);
    }
    // Handle - as _ (same physical key on US keyboards)
    if lower == '-' {
        return char::from_u32(31);
    }
    None
}

// ---------------------------------------------------------------------------
// Sequence parsers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyEventType {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct ParsedKittySequence {
    codepoint: i32,
    base_layout_key: Option<i32>,
    modifier: u8,
    #[allow(dead_code)]
    event_type: KeyEventType,
}

fn parse_event_type(event_type_str: Option<&str>) -> KeyEventType {
    match event_type_str.and_then(|value| value.parse::<u32>().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

fn parse_kitty_sequence(data: &str) -> Option<ParsedKittySequence> {
    // CSI u: \x1b[<cp>u, \x1b[<cp>;<mod>u, with optional :shifted/:base/:event
    if let Some(rest) = data.strip_prefix("\x1b[") {
        if let Some(inner) = rest.strip_suffix('u') {
            // Format: codepoint[:shifted[:base]][;<mod>[:event]]
            let mut colon_parts = inner.split(';');
            let key_part = colon_parts.next()?;
            let modifier_part = colon_parts.next().unwrap_or("");
            let mut key_segments = key_part.split(':');
            let codepoint = key_segments.next()?.parse::<i32>().ok()?;
            // shifted/base keys (flag 4) are parsed but unused for matching.
            let _shifted = key_segments.next().filter(|p| !p.is_empty());
            let _base = key_segments.next();
            let mut mod_segments = modifier_part.split(':');
            let mod_value = mod_segments
                .next()
                .unwrap_or("1")
                .parse::<i32>()
                .unwrap_or(1);
            let event = parse_event_type(mod_segments.next());
            return Some(ParsedKittySequence {
                codepoint,
                base_layout_key: None,
                modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
                event_type: event,
            });
        }
        // Arrow keys with modifier: \x1b[1;<mod>A/B/C/D or with :<event>
        if let Some(rest) = rest.strip_suffix(['A', 'B', 'C', 'D']) {
            let mut segments = rest.split(':');
            let first = segments.next()?.to_string();
            let event = parse_event_type(segments.next());
            let mut kv = first.split(';');
            let one = kv.next()?;
            let arrow_char = rest.chars().last()?;
            if one == "1" {
                let mod_value = kv.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(1);
                let codepoint = match arrow_char {
                    'A' => ARROW_UP,
                    'B' => ARROW_DOWN,
                    'C' => ARROW_RIGHT,
                    'D' => ARROW_LEFT,
                    _ => return None,
                };
                return Some(ParsedKittySequence {
                    codepoint,
                    base_layout_key: None,
                    modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
                    event_type: event,
                });
            }
            // Functional keys: \x1b[<num>~ or \x1b[<num>;<mod>~ (handled below)
        }
        if let Some(rest) = rest.strip_suffix('H').or_else(|| rest.strip_suffix('F')) {
            // Home/End with modifier: \x1b[1;<mod>H/F
            let mut segments = rest.split(':');
            let first = segments.next()?.to_string();
            let event = parse_event_type(segments.next());
            let mut kv = first.split(';');
            let one = kv.next()?;
            if one == "1" {
                let mod_value = kv.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(1);
                let codepoint = if rest.ends_with('H') || data.ends_with('H') {
                    FUNCTIONAL_HOME
                } else {
                    FUNCTIONAL_END
                };
                return Some(ParsedKittySequence {
                    codepoint,
                    base_layout_key: None,
                    modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
                    event_type: event,
                });
            }
        }
        if let Some(inner) = rest.strip_suffix('~') {
            // Functional keys: \x1b[<num>~ or \x1b[<num>;<mod>~ with :<event>
            let mut segments = inner.split(':');
            let first = segments.next()?.to_string();
            let event = parse_event_type(segments.next());
            let mut kv = first.split(';');
            let key_num: i32 = kv.next()?.parse().ok()?;
            let mod_value = kv.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(1);
            let codepoint = match key_num {
                2 => FUNCTIONAL_INSERT,
                3 => FUNCTIONAL_DELETE,
                5 => FUNCTIONAL_PAGE_UP,
                6 => FUNCTIONAL_PAGE_DOWN,
                7 => FUNCTIONAL_HOME,
                8 => FUNCTIONAL_END,
                _ => return None,
            };
            return Some(ParsedKittySequence {
                codepoint,
                base_layout_key: None,
                modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
                event_type: event,
            });
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct ParsedModifyOtherKeys {
    codepoint: i32,
    modifier: u8,
}

fn parse_modify_other_keys_sequence(data: &str) -> Option<ParsedModifyOtherKeys> {
    // \x1b[27;<mod>;<keycode>~
    let rest = data.strip_prefix("\x1b[27;")?;
    let rest = rest.strip_suffix('~')?;
    let mut parts = rest.split(';');
    let mod_value: i32 = parts.next()?.parse().ok()?;
    let codepoint: i32 = parts.next()?.parse().ok()?;
    Some(ParsedModifyOtherKeys {
        codepoint,
        modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
    })
}

fn matches_kitty_sequence(data: &str, expected_codepoint: i32, expected_modifier: u8) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_mod = parsed.modifier & !LOCK_MASK;
    let expected_mod = expected_modifier & !LOCK_MASK;
    if actual_mod != expected_mod {
        return false;
    }

    let normalized = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(parsed.codepoint),
        parsed.modifier,
    );
    let normalized_expected = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(expected_codepoint),
        expected_modifier,
    );

    if normalized == normalized_expected {
        return true;
    }

    // Alternate match via base layout key for non-Latin layouts: only when
    // the codepoint is not already a recognized Latin letter or symbol.
    if parsed.base_layout_key == Some(normalize_kitty_functional_codepoint(expected_codepoint)) {
        let is_latin_letter = (97..=122).contains(&normalized);
        let is_known_symbol =
            symbol_keys().contains(&(char::try_from(normalized as u32).unwrap_or('\0')));
        if !is_latin_letter && !is_known_symbol {
            return true;
        }
    }

    false
}

fn matches_modify_other_keys(data: &str, expected_keycode: i32, expected_modifier: u8) -> bool {
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    parsed.codepoint == expected_keycode && parsed.modifier == expected_modifier
}

fn matches_printable_modify_other_keys(
    data: &str,
    expected_keycode: i32,
    expected_modifier: u8,
) -> bool {
    if expected_modifier == 0 {
        return false;
    }
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    if parsed.modifier != expected_modifier {
        return false;
    }
    normalize_shifted_letter_identity_codepoint(parsed.codepoint, parsed.modifier)
        == normalize_shifted_letter_identity_codepoint(expected_keycode, expected_modifier)
}

/// Raw 0x08 (BS) ambiguity: Windows Terminal uses it for Ctrl+Backspace.
fn matches_raw_backspace(data: &str, expected_modifier: u8) -> bool {
    if data == "\x7f" {
        return expected_modifier == 0;
    }
    if data != "\x08" {
        return false;
    }
    if is_windows_terminal_session() {
        expected_modifier == MOD_CTRL
    } else {
        expected_modifier == 0
    }
}

fn is_windows_terminal_session() -> bool {
    std::env::var("WT_SESSION").is_ok()
        && std::env::var("SSH_CONNECTION").is_err()
        && std::env::var("SSH_CLIENT").is_err()
        && std::env::var("SSH_TTY").is_err()
}

// ---------------------------------------------------------------------------
// matchesKey
// ---------------------------------------------------------------------------

/// Parse a key identifier like "ctrl+shift+p" into its key + modifiers.
fn parse_key_id(key_id: &str) -> Option<(String, u8)> {
    let lowered = key_id.to_lowercase();
    let mut parts: Vec<&str> = lowered.split('+').collect();
    if parts.is_empty() {
        return None;
    }
    let key = parts.pop()?.to_string();
    if key.is_empty() {
        return None;
    }
    let modifier = parts.iter().fold(0u8, |acc, part| match *part {
        "ctrl" => acc | MOD_CTRL,
        "shift" => acc | MOD_SHIFT,
        "alt" => acc | MOD_ALT,
        "super" => acc | MOD_SUPER,
        _ => acc,
    });
    Some((key, modifier))
}

fn is_digit_key(key: &str) -> bool {
    matches!(
        key,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
    )
}

/// Match input data against a key identifier string (upstream `matchesKey`).
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some((key, modifier)) = parse_key_id(key_id) else {
        return false;
    };

    match key.as_str() {
        "escape" | "esc" => {
            if modifier != 0 {
                return false;
            }
            data == "\x1b"
                || matches_kitty_sequence(data, cp("escape"), 0)
                || matches_modify_other_keys(data, cp("escape"), 0)
        }
        "space" => {
            if !is_kitty_protocol_active() {
                if modifier == MOD_CTRL && data == "\x00" {
                    return true;
                }
                if modifier == MOD_ALT && data == "\x1b " {
                    return true;
                }
            }
            if modifier == 0 {
                return data == " "
                    || matches_kitty_sequence(data, cp("space"), 0)
                    || matches_modify_other_keys(data, cp("space"), 0);
            }
            matches_kitty_sequence(data, cp("space"), modifier)
                || matches_modify_other_keys(data, cp("space"), modifier)
        }
        "tab" => {
            if modifier == MOD_SHIFT {
                return data == "\x1b[Z"
                    || matches_kitty_sequence(data, cp("tab"), MOD_SHIFT)
                    || matches_modify_other_keys(data, cp("tab"), MOD_SHIFT);
            }
            if modifier == 0 {
                return data == "\t" || matches_kitty_sequence(data, cp("tab"), 0);
            }
            matches_kitty_sequence(data, cp("tab"), modifier)
                || matches_modify_other_keys(data, cp("tab"), modifier)
        }
        "enter" | "return" => {
            if modifier == MOD_SHIFT {
                if matches_kitty_sequence(data, cp("enter"), MOD_SHIFT)
                    || matches_kitty_sequence(data, cp("kpEnter"), MOD_SHIFT)
                {
                    return true;
                }
                if matches_modify_other_keys(data, cp("enter"), MOD_SHIFT) {
                    return true;
                }
            }
            if modifier == 0 {
                return data == "\r"
                    || (!is_kitty_protocol_active() && data == "\n")
                    || data == "\x1bOM"
                    || matches_kitty_sequence(data, cp("enter"), 0)
                    || matches_kitty_sequence(data, cp("kpEnter"), 0);
            }
            matches_kitty_sequence(data, cp("enter"), modifier)
                || matches_kitty_sequence(data, cp("kpEnter"), modifier)
                || matches_modify_other_keys(data, cp("enter"), modifier)
        }
        "backspace" => {
            if modifier == MOD_ALT {
                if data == "\x1b\x7f" || data == "\x1b\x08" {
                    return true;
                }
                return matches_kitty_sequence(data, cp("backspace"), MOD_ALT)
                    || matches_modify_other_keys(data, cp("backspace"), MOD_ALT);
            }
            if modifier == MOD_CTRL {
                if matches_raw_backspace(data, MOD_CTRL) {
                    return true;
                }
                return matches_kitty_sequence(data, cp("backspace"), MOD_CTRL)
                    || matches_modify_other_keys(data, cp("backspace"), MOD_CTRL);
            }
            if modifier == 0 {
                return matches_raw_backspace(data, 0)
                    || matches_kitty_sequence(data, cp("backspace"), 0)
                    || matches_modify_other_keys(data, cp("backspace"), 0);
            }
            matches_kitty_sequence(data, cp("backspace"), modifier)
                || matches_modify_other_keys(data, cp("backspace"), modifier)
        }
        "insert" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("insert"))
                    || matches_kitty_sequence(data, FUNCTIONAL_INSERT, 0);
            }
            if matches_legacy_modifier_sequence(data, "insert", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_INSERT, modifier)
        }
        "delete" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("delete"))
                    || matches_kitty_sequence(data, FUNCTIONAL_DELETE, 0);
            }
            if matches_legacy_modifier_sequence(data, "delete", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_DELETE, modifier)
        }
        "clear" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("clear"));
            }
            matches_legacy_modifier_sequence(data, "clear", modifier)
        }
        "home" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("home"))
                    || matches_kitty_sequence(data, FUNCTIONAL_HOME, 0);
            }
            if matches_legacy_modifier_sequence(data, "home", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_HOME, modifier)
        }
        "end" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("end"))
                    || matches_kitty_sequence(data, FUNCTIONAL_END, 0);
            }
            if matches_legacy_modifier_sequence(data, "end", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_END, modifier)
        }
        "pageup" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("pageup"))
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "pageup", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, modifier)
        }
        "pagedown" => {
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("pagedown"))
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "pagedown", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, modifier)
        }
        "up" => {
            if modifier == MOD_ALT {
                return data == "\x1bp" || matches_kitty_sequence(data, ARROW_UP, MOD_ALT);
            }
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("up"))
                    || matches_kitty_sequence(data, ARROW_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "up", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_UP, modifier)
        }
        "down" => {
            if modifier == MOD_ALT {
                return data == "\x1bn" || matches_kitty_sequence(data, ARROW_DOWN, MOD_ALT);
            }
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("down"))
                    || matches_kitty_sequence(data, ARROW_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "down", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_DOWN, modifier)
        }
        "left" => {
            if modifier == MOD_ALT {
                return data == "\x1b[1;3D"
                    || (!is_kitty_protocol_active() && data == "\x1bB")
                    || data == "\x1bb"
                    || matches_kitty_sequence(data, ARROW_LEFT, MOD_ALT);
            }
            if modifier == MOD_CTRL {
                return data == "\x1b[1;5D"
                    || matches_legacy_modifier_sequence(data, "left", MOD_CTRL)
                    || matches_kitty_sequence(data, ARROW_LEFT, MOD_CTRL);
            }
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("left"))
                    || matches_kitty_sequence(data, ARROW_LEFT, 0);
            }
            if matches_legacy_modifier_sequence(data, "left", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_LEFT, modifier)
        }
        "right" => {
            if modifier == MOD_ALT {
                return data == "\x1b[1;3C"
                    || (!is_kitty_protocol_active() && data == "\x1bF")
                    || data == "\x1bf"
                    || matches_kitty_sequence(data, ARROW_RIGHT, MOD_ALT);
            }
            if modifier == MOD_CTRL {
                return data == "\x1b[1;5C"
                    || matches_legacy_modifier_sequence(data, "right", MOD_CTRL)
                    || matches_kitty_sequence(data, ARROW_RIGHT, MOD_CTRL);
            }
            if modifier == 0 {
                return MATCHES_LEGACY_SEQUENCE(data, legacy_sequences("right"))
                    || matches_kitty_sequence(data, ARROW_RIGHT, 0);
            }
            if matches_legacy_modifier_sequence(data, "right", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_RIGHT, modifier)
        }
        k if k.starts_with('f') && k.len() <= 3 && k[1..].chars().all(|c| c.is_ascii_digit()) => {
            let key = k;
            if modifier != 0 {
                return false;
            }
            MATCHES_LEGACY_SEQUENCE(data, legacy_sequences(key))
        }
        _ => {
            // Single letter/digit keys and symbols
            if key.chars().count() == 1
                && (key.chars().all(|c| c.is_ascii_lowercase())
                    || is_digit_key(&key)
                    || symbol_keys().contains(&key.chars().next().unwrap()))
            {
                let ch = key.chars().next().unwrap();
                let codepoint = ch as i32;
                let raw_ctrl = raw_ctrl_char(&key);
                let is_letter = ch.is_ascii_lowercase();
                let is_digit = is_digit_key(&key);

                if modifier == MOD_CTRL | MOD_ALT && !is_kitty_protocol_active() {
                    if let Some(raw) = raw_ctrl {
                        if data == format!("\x1b{raw}") {
                            return true;
                        }
                    }
                }

                if modifier == MOD_ALT
                    && !is_kitty_protocol_active()
                    && (is_letter || is_digit || symbol_keys().contains(&ch))
                {
                    // Legacy: alt+printable key is ESC followed by the key
                    if data == format!("\x1b{key}") {
                        return true;
                    }
                }

                if modifier == MOD_CTRL {
                    // Legacy: ctrl+key sends the control character
                    if let Some(raw) = raw_ctrl {
                        if data == raw.to_string() {
                            return true;
                        }
                    }
                    return matches_kitty_sequence(data, codepoint, MOD_CTRL)
                        || matches_printable_modify_other_keys(data, codepoint, MOD_CTRL);
                }

                if modifier == MOD_SHIFT | MOD_CTRL {
                    return matches_kitty_sequence(data, codepoint, MOD_SHIFT | MOD_CTRL)
                        || matches_printable_modify_other_keys(
                            data,
                            codepoint,
                            MOD_SHIFT | MOD_CTRL,
                        );
                }

                if modifier == MOD_SHIFT {
                    // Legacy: shift+letter produces uppercase
                    if is_letter && data == ch.to_ascii_uppercase().to_string() {
                        return true;
                    }
                    return matches_kitty_sequence(data, codepoint, MOD_SHIFT)
                        || matches_printable_modify_other_keys(data, codepoint, MOD_SHIFT);
                }

                if modifier != 0 {
                    return matches_kitty_sequence(data, codepoint, modifier)
                        || matches_printable_modify_other_keys(data, codepoint, modifier);
                }

                // Raw char and Kitty sequence (needed for release events)
                return data == key || matches_kitty_sequence(data, codepoint, 0);
            }

            false
        }
    }
}

// ============================================================================
// Printable key decoding (upstream decodeKittyPrintable /
// decodeModifyOtherKeysPrintable / decodePrintableKey)
// ============================================================================

const KITTY_PRINTABLE_ALLOWED_MODIFIERS: u8 = MOD_SHIFT | LOCK_MASK;

/// A parsed CSI-u sequence with the optional shifted key (upstream the
/// csi-u branch of the Kitty parser).
struct ParsedKittyCsiU {
    codepoint: i32,
    shifted_key: Option<i32>,
    modifier: u8,
}

/// Parse `\x1b[<cp>[:<shifted>[:<base>]][;<mod>[:<event>]]u`.
fn parse_kitty_csi_u(data: &str) -> Option<ParsedKittyCsiU> {
    let rest = data.strip_prefix("\u{1b}[")?.strip_suffix('u')?;
    let mut colon_parts = rest.split(';');
    let key_part = colon_parts.next()?;
    let modifier_part = colon_parts.next().unwrap_or("");
    let mut key_segments = key_part.split(':');
    let codepoint = key_segments.next()?.parse::<i32>().ok()?;
    let shifted_key = key_segments
        .next()
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<i32>().ok());
    let _base = key_segments.next();
    let mut mod_segments = modifier_part.split(':');
    let mod_value = mod_segments
        .next()
        .unwrap_or("1")
        .parse::<i32>()
        .unwrap_or(1);
    Some(ParsedKittyCsiU {
        codepoint,
        shifted_key,
        modifier: (mod_value - 1).clamp(0, u8::MAX as i32) as u8,
    })
}

/// Extract the printable character from a Kitty CSI-u sequence (upstream
/// `decodeKittyPrintable`). Only plain or Shift-modified keys are accepted;
/// Ctrl/Alt/Super and unknown modifier bits are rejected.
pub fn decode_kitty_printable(data: &str) -> Option<String> {
    let parsed = parse_kitty_csi_u(data)?;
    let modifier = parsed.modifier;
    if (modifier & !KITTY_PRINTABLE_ALLOWED_MODIFIERS) != 0 {
        return None;
    }
    if (modifier & (MOD_ALT | MOD_CTRL)) != 0 {
        return None;
    }
    let mut effective_codepoint = parsed.codepoint;
    if modifier & MOD_SHIFT != 0 {
        if let Some(shifted) = parsed.shifted_key {
            effective_codepoint = shifted;
        }
    }
    let effective_codepoint = normalize_kitty_functional_codepoint(effective_codepoint);
    if effective_codepoint < 32 {
        return None;
    }
    char::from_u32(effective_codepoint as u32).map(|c| c.to_string())
}

/// Extract the printable character from a modifyOtherKeys sequence (upstream
/// `decodeModifyOtherKeysPrintable`): `\x1b[27;<mod>;<keycode>~`.
pub fn decode_modify_other_keys_printable(data: &str) -> Option<String> {
    let parsed = parse_modify_other_keys_sequence(data)?;
    let modifier = parsed.modifier & !LOCK_MASK;
    if (modifier & !MOD_SHIFT) != 0 {
        return None;
    }
    if parsed.codepoint < 32 {
        return None;
    }
    char::from_u32(parsed.codepoint as u32).map(|c| c.to_string())
}

/// Decode a printable character from either protocol (upstream
/// `decodePrintableKey`).
pub fn decode_printable_key(data: &str) -> Option<String> {
    decode_kitty_printable(data).or_else(|| decode_modify_other_keys_printable(data))
}
