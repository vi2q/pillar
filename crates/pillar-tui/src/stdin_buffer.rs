//! Port of packages/tui/src/stdin-buffer.ts (pi v0.84.3): buffers stdin
//! input and emits complete escape sequences, handling partial CSI /
//! OSC / DCS / APC / SS3 sequences that arrive across chunks,
//! bracketed paste, Kitty printable dedup, and timeout flushing.
//!
//! divergences: the EventEmitter and setTimeout are host-driven —
//! [`process_with_clock`] takes the current instant and returns the
//! sequences plus an optional deadline the host should schedule a
//! flush at.
//!
//! [`process_with_clock`]: StdinBuffer::process_with_clock

use std::time::{Duration, Instant};

const ESC: char = '\u{1b}';
const DEFAULT_SEQUENCE_TIMEOUT_MS: u64 = 50;
const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;
const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";

/// Sequence completeness classification (upstream
/// `isCompleteSequence` result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceStatus {
    Complete,
    Incomplete,
    NotEscape,
}

/// Whether a string is a complete escape sequence (upstream
/// `isCompleteSequence`).
pub fn is_complete_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with(ESC) {
        return SequenceStatus::NotEscape;
    }
    let mut chars = data.chars();
    chars.next();
    if data.chars().count() == 1 {
        return SequenceStatus::Incomplete;
    }
    let after_esc: &str = &data[1..];

    if let Some(rest) = after_esc.strip_prefix('[') {
        // Old-style mouse: ESC[M + 3 bytes.
        if rest.starts_with('M') {
            return if data.chars().count() >= 6 {
                SequenceStatus::Complete
            } else {
                SequenceStatus::Incomplete
            };
        }
        return is_complete_csi_sequence(data);
    }
    if after_esc.starts_with(']') {
        return is_complete_osc_sequence(data);
    }
    if after_esc.starts_with('P') {
        return is_complete_dcs_sequence(data);
    }
    if after_esc.starts_with('_') {
        return is_complete_apc_sequence(data);
    }
    if after_esc.starts_with('O') {
        // ESC O followed by a single character.
        return if after_esc.chars().count() >= 2 {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }
    if after_esc.chars().count() == 1 {
        // Meta key: ESC + single char.
        return SequenceStatus::Complete;
    }
    SequenceStatus::Complete
}

/// CSI completeness (upstream `isCompleteCsiSequence`): ends with a
/// final byte 0x40-0x7E, with SGR mouse structure checks.
pub fn is_complete_csi_sequence(data: &str) -> SequenceStatus {
    let Some(payload) = data.strip_prefix("\u{1b}[") else {
        return SequenceStatus::Complete;
    };
    if payload.chars().count() < 1 || data.chars().count() < 3 {
        return SequenceStatus::Incomplete;
    }
    let last_char = payload.chars().last().unwrap();
    let last_char_code = last_char as u32;
    if (0x40..=0x7e).contains(&last_char_code) {
        if payload.starts_with('<') {
            // SGR mouse: <B;X;Y[Mm].
            if is_sgr_mouse_payload(payload) {
                return SequenceStatus::Complete;
            }
            if last_char == 'M' || last_char == 'm' {
                let parts: Vec<&str> = payload[1..payload.len() - 1].split(';').collect();
                if parts.len() == 3
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                {
                    return SequenceStatus::Complete;
                }
            }
            return SequenceStatus::Incomplete;
        }
        return SequenceStatus::Complete;
    }
    SequenceStatus::Incomplete
}

fn is_sgr_mouse_payload(payload: &str) -> bool {
    // /^<\d+;\d+;\d+[Mm]$/
    let Some(body) = payload.strip_prefix('<') else {
        return false;
    };
    let Some(body) = body.strip_suffix(['M', 'm']) else {
        return false;
    };
    let parts: Vec<&str> = body.split(';').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// OSC completeness (upstream `isCompleteOscSequence`): ends with ST
/// or BEL.
pub fn is_complete_osc_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\u{1b}]") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\u{1b}\\") || data.ends_with('\u{7}') {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// DCS completeness (upstream `isCompleteDcsSequence`): ends with ST.
pub fn is_complete_dcs_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\u{1b}P") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\u{1b}\\") {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// APC completeness (upstream `isCompleteApcSequence`): ends with ST.
pub fn is_complete_apc_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with("\u{1b}_") {
        return SequenceStatus::Complete;
    }
    if data.ends_with("\u{1b}\\") {
        SequenceStatus::Complete
    } else {
        SequenceStatus::Incomplete
    }
}

/// Parse an unmodified Kitty printable CSI-u sequence (upstream
/// `parseUnmodifiedKittyPrintableCodepoint`).
fn parse_unmodified_kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    let rest = sequence.strip_prefix("\u{1b}[")?;
    let rest = rest.strip_suffix('u')?;
    let mut parts = rest.split(':');
    let codepoint: u32 = parts.next()?.parse().ok()?;
    // Optional :shifted and :base segments must be well-formed if present.
    let _ = parts.next();
    if !sequence.contains(':') && sequence.matches(';').count() > 0 {
        return None;
    }
    if codepoint >= 32 {
        Some(codepoint)
    } else {
        None
    }
}

/// Split an accumulated buffer into complete sequences (upstream
/// `extractCompleteSequences`).
pub fn extract_complete_sequences(buffer: &str) -> (Vec<String>, String) {
    let mut sequences: Vec<String> = Vec::new();
    let chars: Vec<char> = buffer.chars().collect();
    let mut pos = 0usize;

    while pos < chars.len() {
        let remaining: String = chars[pos..].iter().collect();
        if remaining.starts_with(ESC) {
            let mut seq_end = 1usize;
            loop {
                if seq_end > remaining.chars().count() {
                    return (sequences, remaining);
                }
                let candidate: String = remaining.chars().take(seq_end).collect();
                match is_complete_sequence(&candidate) {
                    SequenceStatus::Complete => {
                        // WezTerm concatenates a raw ESC key press with a
                        // following Kitty CSI-u release: '\x1b\x1b[27;...u'.
                        // When the char after '\x1b\x1b' starts a new escape
                        // sequence, emit only the first ESC.
                        if candidate == "\u{1b}\u{1b}" {
                            let next_char = remaining.chars().nth(seq_end);
                            if matches!(
                                next_char,
                                Some('[') | Some(']') | Some('O') | Some('P') | Some('_')
                            ) {
                                sequences.push(ESC.to_string());
                                pos += 1;
                                break;
                            }
                        }
                        sequences.push(candidate);
                        pos += seq_end;
                        break;
                    }
                    SequenceStatus::Incomplete => seq_end += 1,
                    SequenceStatus::NotEscape => {
                        sequences.push(candidate);
                        pos += seq_end;
                        break;
                    }
                }
            }
        } else {
            sequences.push(remaining.chars().next().unwrap().to_string());
            pos += 1;
        }
    }
    (sequences, String::new())
}

/// The outcome of feeding input (upstream the emitted events plus the
/// pending timer deadline).
pub struct ProcessOutcome {
    /// Complete 'data' sequences.
    pub data: Vec<String>,
    /// Bracketed paste content, if the paste terminator arrived.
    pub paste: Option<String>,
    /// When set, the host should flush at this deadline (upstream
    /// setTimeout → flush).
    pub flush_deadline: Option<Instant>,
}

/// Buffered stdin input (upstream `StdinBuffer`).
pub struct StdinBuffer {
    buffer: String,
    timeout_ms: u64,
    escape_timeout_ms: u64,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
}

impl StdinBuffer {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            timeout_ms: DEFAULT_SEQUENCE_TIMEOUT_MS,
            escape_timeout_ms: DEFAULT_ESCAPE_TIMEOUT_MS,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_kitty_printable_codepoint: None,
        }
    }

    pub fn with_timeouts(sequence_timeout_ms: u64, escape_timeout_ms: u64) -> Self {
        Self {
            timeout_ms: sequence_timeout_ms,
            escape_timeout_ms,
            ..Self::new()
        }
    }

    /// Feed input at `now` (upstream `process` minus the Buffer
    /// high-byte path, which stays host-side).
    pub fn process_with_clock(&mut self, data: &str, now: Instant) -> ProcessOutcome {
        let mut outcome = ProcessOutcome {
            data: Vec::new(),
            paste: None,
            flush_deadline: None,
        };
        self.process_inner(data, now, &mut outcome);
        outcome
    }

    fn process_inner(&mut self, data: &str, now: Instant, outcome: &mut ProcessOutcome) {
        if data.is_empty() && self.buffer.is_empty() {
            self.emit_data_sequence(String::new(), outcome);
            return;
        }
        self.buffer.push_str(data);

        if self.paste_mode {
            self.paste_buffer.push_str(&self.buffer);
            self.buffer.clear();
            if let Some(end_index) = self.paste_buffer.find(BRACKETED_PASTE_END) {
                let pasted = self.paste_buffer[..end_index].to_string();
                let remaining =
                    self.paste_buffer[end_index + BRACKETED_PASTE_END.len()..].to_string();
                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;
                outcome.paste = Some(pasted);
                if !remaining.is_empty() {
                    self.process_inner(&remaining, now, outcome);
                }
            }
            return;
        }

        if let Some(start_index) = self.buffer.find(BRACKETED_PASTE_START) {
            if start_index > 0 {
                let before_paste = self.buffer[..start_index].to_string();
                let (sequences, _) = extract_complete_sequences(&before_paste);
                for sequence in sequences {
                    self.emit_data_sequence(sequence, outcome);
                }
            }
            self.pending_kitty_printable_codepoint = None;
            self.buffer = self.buffer[start_index + BRACKETED_PASTE_START.len()..].to_string();
            self.paste_mode = true;
            self.paste_buffer = std::mem::take(&mut self.buffer);
            if let Some(end_index) = self.paste_buffer.find(BRACKETED_PASTE_END) {
                let pasted = self.paste_buffer[..end_index].to_string();
                let remaining =
                    self.paste_buffer[end_index + BRACKETED_PASTE_END.len()..].to_string();
                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;
                outcome.paste = Some(pasted);
                if !remaining.is_empty() {
                    self.process_inner(&remaining, now, outcome);
                }
            }
            return;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;
        for sequence in sequences {
            self.emit_data_sequence(sequence, outcome);
        }

        if !self.buffer.is_empty() {
            let timeout_ms = if self.buffer == ESC.to_string() {
                self.escape_timeout_ms
            } else {
                self.timeout_ms
            };
            outcome.flush_deadline = Some(now + Duration::from_millis(timeout_ms));
        }
    }

    fn emit_data_sequence(&mut self, sequence: String, outcome: &mut ProcessOutcome) {
        let raw_codepoint = if sequence.chars().count() == 1 {
            sequence.chars().next().map(|c| c as u32)
        } else {
            None
        };
        if raw_codepoint.is_some() && raw_codepoint == self.pending_kitty_printable_codepoint {
            self.pending_kitty_printable_codepoint = None;
            return;
        }
        self.pending_kitty_printable_codepoint =
            parse_unmodified_kitty_printable_codepoint(&sequence);
        outcome.data.push(sequence);
    }

    /// Flush the pending buffer (upstream `flush`): returns the buffered
    /// text as one sequence.
    pub fn flush(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let sequences = vec![std::mem::take(&mut self.buffer)];
        self.pending_kitty_printable_codepoint = None;
        sequences
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }

    pub fn destroy(&mut self) {
        self.clear();
    }
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::new()
    }
}
