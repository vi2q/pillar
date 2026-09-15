//! Port of the keyboard-protocol negotiation and input normalization
//! core from packages/tui/src/terminal.ts (pi v0.84.3).
//!
//! divergences: the ProcessTerminal (raw mode, SIGWINCH, Kitty query
//! emission, progress keepalive, Windows VT input) is host-side; the
//! port exposes the negotiation parser, the buffered negotiation state
//! machine, and the shift+enter normalization the tests exercise.

use crate::keys::set_kitty_protocol_active;

const DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS: u32 = 7;
const NATIVE_SHIFT_ENTER_SEQUENCE: &str = "\u{1b}[13;2u";

/// A parsed keyboard-protocol negotiation response (upstream
/// `KeyboardProtocolNegotiationSequence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    /// CSI ? <flags> u — Kitty flags report.
    KittyFlags { flags: u32 },
    /// CSI ? [digits;]* c — Device Attributes (no Kitty support).
    DeviceAttributes,
}

/// Parse a negotiation response (upstream
/// `parseKeyboardProtocolNegotiationSequence`).
pub fn parse_keyboard_protocol_negotiation_sequence(
    sequence: &str,
) -> Option<KeyboardProtocolNegotiationSequence> {
    if let Some(rest) = sequence.strip_prefix("\u{1b}[?") {
        if let Some(flags_str) = rest.strip_suffix('u') {
            if let Ok(flags) = flags_str.parse::<u32>() {
                return Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags });
            }
        }
    }
    // CSI ? [digits;]* c
    if let Some(rest) = sequence.strip_prefix("\u{1b}[?") {
        if let Some(body) = rest.strip_suffix('c') {
            if body.is_empty() || body.chars().all(|c| c.is_ascii_digit() || c == ';') {
                return Some(KeyboardProtocolNegotiationSequence::DeviceAttributes);
            }
        }
    }
    None
}

/// Whether the sequence could grow into a negotiation response (upstream
/// `isKeyboardProtocolNegotiationSequencePrefix`).
pub fn is_keyboard_protocol_negotiation_sequence_prefix(sequence: &str) -> bool {
    sequence == "\u{1b}[" || {
        if let Some(rest) = sequence.strip_prefix("\u{1b}[?") {
            rest.chars().all(|c| c.is_ascii_digit() || c == ';')
        } else {
            false
        }
    }
}

/// Whether the process runs inside Apple Terminal (upstream
/// `isAppleTerminalSession`). The host supplies platform + TERM_PROGRAM.
pub fn is_apple_terminal_session(platform: &str, term_program: Option<&str>) -> bool {
    platform == "darwin" && term_program == Some("Apple_Terminal")
}

/// Rewrite a bare CR into the native shift+enter sequence when the
/// terminal cannot report Shift+Enter itself (upstream
/// `normalizeNativeShiftEnterInput`).
pub fn normalize_native_shift_enter_input(
    data: &str,
    should_detect_native_shift_enter: bool,
    is_shift_pressed: bool,
) -> String {
    if should_detect_native_shift_enter && data == "\r" && is_shift_pressed {
        return NATIVE_SHIFT_ENTER_SEQUENCE.to_string();
    }
    data.to_string()
}

/// Apple Terminal variant (upstream `normalizeAppleTerminalInput`).
pub fn normalize_apple_terminal_input(
    data: &str,
    is_apple_terminal: bool,
    is_shift_pressed: bool,
) -> String {
    normalize_native_shift_enter_input(data, is_apple_terminal, is_shift_pressed)
}

/// Resolve the escape reassembly timeout (upstream
/// `resolveEscapeTimeoutMs`): PILLAR_TUI_ESC_TIMEOUT override, then SSH
/// detection, then the 10ms default.
pub fn resolve_escape_timeout_ms(pi_tui_esc_timeout: Option<&str>, has_ssh: bool) -> u64 {
    const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;
    const DEFAULT_SSH_ESCAPE_TIMEOUT_MS: u64 = 100;
    if let Some(configured) = pi_tui_esc_timeout {
        if let Ok(value) = configured.parse::<u64>() {
            if value > 0 {
                return value;
            }
        }
    }
    if has_ssh {
        return DEFAULT_SSH_ESCAPE_TIMEOUT_MS;
    }
    DEFAULT_ESCAPE_TIMEOUT_MS
}

/// Kitty protocol query emission (upstream `KITTY_KEYBOARD_PROTOCOL_QUERY`).
pub fn kitty_keyboard_protocol_query() -> String {
    format!(
        "\u{1b}[>{}u\u{1b}[?u\u{1b}[c",
        DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS
    )
}

/// Negotiation state machine (upstream the private negotiation-buffer
/// methods of ProcessTerminal). Timers are host-driven via
/// [`pending_flush`] deadlines.
///
/// [`pending_flush`]: KeyboardProtocolNegotiator::pending_flush
pub struct KeyboardProtocolNegotiator {
    buffer: String,
    flushed: Option<String>,
    kitty_protocol_active: bool,
    modify_other_keys_active: bool,
}

/// The outcome of feeding one parsed stdin sequence.
pub enum NegotiationOutcome {
    /// The sequence was consumed as (part of) a negotiation response.
    Consumed,
    /// The sequence may grow into a response; hold it.
    Pending,
    /// Not a negotiation sequence: forward to the input handler.
    Forward,
}

impl KeyboardProtocolNegotiator {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            flushed: None,
            kitty_protocol_active: false,
            modify_other_keys_active: false,
        }
    }

    pub fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }

    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_active
    }

    /// Buffered bytes awaiting the rest of a response (upstream the
    /// negotiation buffer).
    pub fn buffered(&self) -> &str {
        &self.buffer
    }

    /// Feed one complete stdin sequence (upstream
    /// readKeyboardProtocolNegotiationSequence +
    /// handleKeyboardProtocolNegotiationSequence). The host writes the
    /// returned escape commands to the terminal.
    pub fn feed(&mut self, sequence: &str) -> NegotiationOutcome {
        if !self.buffer.is_empty() {
            let buffered = format!("{}{}", self.buffer, sequence);
            if let Some(negotiation) = parse_keyboard_protocol_negotiation_sequence(&buffered) {
                self.buffer.clear();
                return self.apply(negotiation);
            }
            if is_keyboard_protocol_negotiation_sequence_prefix(&buffered) {
                self.buffer = buffered;
                return NegotiationOutcome::Pending;
            }
            // Not a negotiation after all: flush the buffer as input
            // (upstream flushKeyboardProtocolNegotiationBufferAsInput).
            self.flushed = Some(std::mem::take(&mut self.buffer));
            return NegotiationOutcome::Forward;
        }

        if let Some(negotiation) = parse_keyboard_protocol_negotiation_sequence(sequence) {
            return self.apply(negotiation);
        }
        if is_keyboard_protocol_negotiation_sequence_prefix(sequence) {
            self.buffer = sequence.to_string();
            return NegotiationOutcome::Pending;
        }
        NegotiationOutcome::Forward
    }

    fn apply(&mut self, negotiation: KeyboardProtocolNegotiationSequence) -> NegotiationOutcome {
        self.buffer.clear();
        match negotiation {
            KeyboardProtocolNegotiationSequence::KittyFlags { flags } => {
                if flags != 0 {
                    self.modify_other_keys_active = false;
                    if !self.kitty_protocol_active {
                        self.kitty_protocol_active = true;
                        set_kitty_protocol_active(true);
                    }
                } else {
                    self.modify_other_keys_active = true;
                }
            }
            KeyboardProtocolNegotiationSequence::DeviceAttributes => {
                if !self.kitty_protocol_active {
                    self.modify_other_keys_active = true;
                }
            }
        }
        NegotiationOutcome::Consumed
    }

    /// Flush the buffered partial response as input (upstream
    /// `flushKeyboardProtocolNegotiationBufferAsInput`). Returns the
    /// buffered bytes for the host to forward.
    pub fn flush_pending(&mut self) -> Option<String> {
        self.flushed.take()
    }
}

impl Default for KeyboardProtocolNegotiator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kitty_flags_response() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[?1u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags: 1 })
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[?7u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags: 7 })
        );
    }

    #[test]
    fn parse_device_attributes_response() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[?1;0c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[?c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
    }

    #[test]
    fn parse_rejects_other_sequences() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[A"),
            None
        );
        assert_eq!(parse_keyboard_protocol_negotiation_sequence("plain"), None);
        // Kitty CSI-u keys are not negotiation responses.
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\u{1b}[97u"),
            None
        );
    }

    #[test]
    fn prefix_detection() {
        assert!(is_keyboard_protocol_negotiation_sequence_prefix("\u{1b}["));
        assert!(is_keyboard_protocol_negotiation_sequence_prefix("\u{1b}[?"));
        assert!(is_keyboard_protocol_negotiation_sequence_prefix(
            "\u{1b}[?1"
        ));
        assert!(is_keyboard_protocol_negotiation_sequence_prefix(
            "\u{1b}[?1;2"
        ));
        assert!(!is_keyboard_protocol_negotiation_sequence_prefix(
            "\u{1b}[?1;2x"
        ));
        assert!(!is_keyboard_protocol_negotiation_sequence_prefix(
            "\u{1b}[A"
        ));
    }

    #[test]
    fn escape_timeout_resolution() {
        assert_eq!(resolve_escape_timeout_ms(None, false), 10);
        assert_eq!(resolve_escape_timeout_ms(None, true), 100);
        assert_eq!(resolve_escape_timeout_ms(Some("250"), false), 250);
        assert_eq!(resolve_escape_timeout_ms(Some("250"), true), 250);
        // Zero/invalid falls through to the environment default.
        assert_eq!(resolve_escape_timeout_ms(Some("0"), false), 10);
        assert_eq!(resolve_escape_timeout_ms(Some("abc"), false), 10);
    }

    #[test]
    fn native_shift_enter_rewrites_cr() {
        assert_eq!(
            normalize_native_shift_enter_input("\r", true, true),
            "\u{1b}[13;2u"
        );
        // Without shift or detection, CR passes through.
        assert_eq!(normalize_native_shift_enter_input("\r", true, false), "\r");
        assert_eq!(normalize_native_shift_enter_input("\r", false, true), "\r");
        // Other data untouched.
        assert_eq!(normalize_native_shift_enter_input("x", true, true), "x");
    }

    #[test]
    fn apple_terminal_session_detection() {
        assert!(is_apple_terminal_session("darwin", Some("Apple_Terminal")));
        assert!(!is_apple_terminal_session("darwin", Some("iTerm.app")));
        assert!(!is_apple_terminal_session("linux", Some("Apple_Terminal")));
    }

    #[test]
    fn kitty_query_sequence() {
        assert_eq!(
            kitty_keyboard_protocol_query(),
            "\u{1b}[>7u\u{1b}[?u\u{1b}[c"
        );
    }

    #[test]
    fn kitty_flags_enable_kitty_protocol() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        assert!(matches!(
            negotiator.feed("\u{1b}[?7u"),
            NegotiationOutcome::Consumed
        ));
        assert!(negotiator.kitty_protocol_active());
        assert!(!negotiator.modify_other_keys_active());
    }

    #[test]
    fn zero_flags_fall_back_to_modify_other_keys() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        let _ = negotiator.feed("\u{1b}[?0u");
        assert!(!negotiator.kitty_protocol_active());
        assert!(negotiator.modify_other_keys_active());
    }

    #[test]
    fn device_attributes_falls_back_when_no_kitty() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        let _ = negotiator.feed("\u{1b}[?1;0c");
        assert!(!negotiator.kitty_protocol_active());
        assert!(negotiator.modify_other_keys_active());
    }

    #[test]
    fn kitty_response_preempts_modify_other_keys() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        let _ = negotiator.feed("\u{1b}[?1;0c");
        assert!(negotiator.modify_other_keys_active());
        // Kitty arriving later wins.
        let _ = negotiator.feed("\u{1b}[?7u");
        assert!(negotiator.kitty_protocol_active());
        assert!(!negotiator.modify_other_keys_active());
    }

    #[test]
    fn partial_response_is_buffered_then_completed() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        assert!(matches!(
            negotiator.feed("\u{1b}[?1"),
            NegotiationOutcome::Pending
        ));
        assert_eq!(negotiator.buffered(), "\u{1b}[?1");
        assert!(matches!(negotiator.feed("u"), NegotiationOutcome::Consumed));
        assert!(negotiator.kitty_protocol_active());
        assert_eq!(negotiator.buffered(), "");
    }

    #[test]
    fn abandoned_partial_is_flushable() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        let _ = negotiator.feed("\u{1b}[?1");
        // A non-negotiation sequence arrives → flush the buffer.
        assert!(matches!(
            negotiator.feed("\u{1b}[A"),
            NegotiationOutcome::Forward
        ));
        // The buffered bytes are returned for the host to forward.
        assert_eq!(negotiator.flush_pending(), Some("\u{1b}[?1".to_string()));
    }

    #[test]
    fn plain_keys_forward() {
        let mut negotiator = KeyboardProtocolNegotiator::new();
        assert!(matches!(negotiator.feed("a"), NegotiationOutcome::Forward));
        assert!(matches!(
            negotiator.feed("\u{1b}[A"),
            NegotiationOutcome::Forward
        ));
        assert_eq!(negotiator.buffered(), "");
    }
}
