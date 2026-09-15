//! Port of packages/tui/src/terminal.ts (pi v0.84.3) — the `Terminal`
//! interface and `ProcessTerminal`.
//!
//! The pure helpers (`resolveEscapeTimeoutMs`, the kitty negotiation
//! parser/state machine, Apple Terminal input normalization) live in
//! [`crate::terminal`]; this module adds the live terminal.
//!
//! divergences:
//! - I/O is host-driven (the render loop calls `read_input` /
//!   `feed_input_bytes`) instead of upstream's Node callbacks; input is still
//!   parsed by the ported [`StdinBuffer`], so kitty responses and bracketed
//!   paste behave like upstream.
//! - `crossterm` provides raw mode, terminal size and Windows console
//!   handling. Resize is detected by comparing [`Terminal::resize_if_changed`]
//!   instead of a SIGWINCH/`resize` event.
//! - `PILLAR_TUI_WRITE_LOG` tracing, the native Windows VT-input helper and the
//!   native modifier helper (`native-modifiers.ts`) are not ported:
//!   [`is_native_modifier_pressed`] always answers `false`, which is what
//!   upstream does when the helper is unavailable.

#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
use std::time::{Duration, Instant};

use crate::stdin_buffer::StdinBuffer;
use crate::terminal::{
    KeyboardProtocolNegotiator, NegotiationOutcome, is_apple_terminal_session,
    kitty_keyboard_protocol_query, normalize_apple_terminal_input,
    normalize_native_shift_enter_input, resolve_escape_timeout_ms,
};

/// Terminal progress keepalive (upstream `TERMINAL_PROGRESS_KEEPALIVE_MS`).
pub const TERMINAL_PROGRESS_KEEPALIVE_MS: u64 = 1000;
/// OSC 9;4;3 — indeterminate progress (upstream
/// `TERMINAL_PROGRESS_ACTIVE_SEQUENCE`).
pub const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\u{1b}]9;4;3\u{7}";
/// OSC 9;4;0 — clear progress (upstream `TERMINAL_PROGRESS_CLEAR_SEQUENCE`).
pub const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\u{1b}]9;4;0\u{7}";
/// Fallback terminal size when the OS reports none.
pub const FALLBACK_COLUMNS: usize = 80;
pub const FALLBACK_ROWS: usize = 24;

/// A modifier key whose OS state can be queried (upstream `ModifierKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

/// Whether a modifier is currently held (upstream `isNativeModifierPressed`).
///
/// divergence: the native helper is not ported, so this is always `false`
/// (matching upstream when the helper is missing).
pub fn is_native_modifier_pressed(_name: ModifierKey) -> bool {
    false
}

/// How a [`ProcessTerminal`] reaches the operating system.
pub trait TerminalIo: Send {
    fn write(&mut self, data: &str);
    /// Current terminal size; `(0, 0)` when unknown.
    fn size(&self) -> (usize, usize);
    fn enable_raw_mode(&mut self) -> bool;
    fn disable_raw_mode(&mut self);
    /// Read available input; `None` on error or when nothing arrived within
    /// `timeout`.
    fn read_input(&mut self, buffer: &mut [u8], timeout: Duration) -> Option<usize>;
}

/// The terminal the TUI drives (upstream `Terminal`).
pub trait Terminal: Send {
    /// Enter raw mode, enable bracketed paste and query the kitty protocol.
    fn start(&mut self);
    /// Restore the terminal state.
    fn stop(&mut self);
    /// Drain stdin before exiting (upstream `drainInput`).
    fn drain_input(&mut self, max_ms: u64, idle_ms: u64);
    /// Write raw output.
    fn write(&mut self, data: &str);
    /// Read raw input bytes within `timeout` (utf8-lossy).
    fn read_input(&mut self, timeout: Duration) -> Option<String>;
    /// Feed raw input bytes and return the complete input sequences
    /// (bracketed paste is re-wrapped like upstream).
    fn feed_input_bytes(&mut self, data: &str, now: Instant) -> Vec<String>;
    /// Flush a buffered partial escape sequence once its disambiguation
    /// deadline passed (upstream the StdinBuffer schedules that timer itself;
    /// the port's host polls this). The host feeds the returned sequences
    /// through [`Terminal::handle_sequence`] like any other input — without
    /// this a lone Escape or a split escape waits for the next keypress.
    fn flush_pending_input(&mut self, now: Instant) -> Vec<String>;
    /// Handle one parsed sequence: kitty negotiation responses are consumed,
    /// other sequences are returned for dispatch.
    fn handle_sequence(&mut self, sequence: &str) -> Option<String>;
    /// Apply Apple Terminal / native shift+enter normalization (upstream
    /// `forwardInputSequence`).
    fn normalize_input(&self, sequence: &str) -> String;
    fn columns(&self) -> usize;
    fn rows(&self) -> usize;
    /// Re-read the terminal size, answering whether it changed.
    fn resize_if_changed(&mut self) -> bool;
    fn kitty_protocol_active(&self) -> bool;
    /// Move the cursor by `lines` (negative = up).
    fn move_by(&mut self, lines: i64);
    fn hide_cursor(&mut self);
    fn show_cursor(&mut self);
    fn clear_line(&mut self);
    fn clear_from_cursor(&mut self);
    fn clear_screen(&mut self);
    fn set_title(&mut self, title: &str);
    fn set_progress(&mut self, active: bool);
    /// Emit the OSC 9;4 progress keepalive when due; returns whether it wrote.
    fn progress_keepalive(&mut self, now: Instant) -> bool;
}

/// The live process terminal (upstream `ProcessTerminal`).
pub struct ProcessTerminal {
    io: Box<dyn TerminalIo>,
    columns: usize,
    rows: usize,
    kitty_protocol_active: bool,
    modify_other_keys_active: bool,
    keyboard_protocol_pushed: bool,
    negotiator: KeyboardProtocolNegotiator,
    stdin_buffer: StdinBuffer,
    /// When the buffered partial sequence is flushed as input (upstream the
    /// StdinBuffer's `setTimeout`); refreshed on every `feed_input_bytes`.
    pending_flush: Option<Instant>,
    progress_active: bool,
    last_progress_write: Option<Instant>,
    escape_timeout_ms: u64,
    apple_terminal: bool,
}

impl ProcessTerminal {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self::with_io(Box::<ProcessTerminalIo>::default())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self::with_io(Box::new(NullTerminalIo))
    }

    /// Build a terminal over injected I/O (tests, wasm hosts).
    pub fn with_io(io: Box<dyn TerminalIo>) -> Self {
        let (columns, rows) = io.size();
        let escape_timeout_ms = resolve_escape_timeout_ms(
            std::env::var("PILLAR_TUI_ESC_TIMEOUT").ok().as_deref(),
            std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok(),
        );
        let apple_terminal = is_apple_terminal_session(
            std::env::consts::OS,
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        );
        Self {
            io,
            columns: if columns == 0 {
                FALLBACK_COLUMNS
            } else {
                columns
            },
            rows: if rows == 0 { FALLBACK_ROWS } else { rows },
            kitty_protocol_active: false,
            modify_other_keys_active: false,
            keyboard_protocol_pushed: false,
            negotiator: KeyboardProtocolNegotiator::new(),
            stdin_buffer: StdinBuffer::with_timeouts(0, escape_timeout_ms),
            pending_flush: None,
            progress_active: false,
            last_progress_write: None,
            escape_timeout_ms,
            apple_terminal,
        }
    }

    /// The escape-reassembly window this terminal uses (upstream
    /// `resolveEscapeTimeoutMs`).
    pub fn escape_timeout_ms(&self) -> u64 {
        self.escape_timeout_ms
    }

    /// Whether the terminal is treated as an Apple Terminal session.
    pub fn is_apple_terminal(&self) -> bool {
        self.apple_terminal
    }

    /// Overflow bytes the input buffer is holding (upstream
    /// `StdinBuffer.getBuffer`).
    pub fn buffered_input(&self) -> &str {
        self.stdin_buffer.get_buffer()
    }

    fn disable_kitty_protocol(&mut self) {
        if self.keyboard_protocol_pushed || self.kitty_protocol_active {
            self.write("\u{1b}[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_protocol_active = false;
        }
    }

    fn disable_modify_other_keys(&mut self) {
        if !self.modify_other_keys_active {
            return;
        }
        self.write("\u{1b}[>4;0m");
        self.modify_other_keys_active = false;
    }
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

/// The real terminal I/O (crossterm + stdio).
///
/// divergence: stdin is read by a background thread and handed over a
/// channel so [`TerminalIo::read_input`] can honour its timeout (upstream
/// Node gets a `data` callback instead). The thread blocks on stdin for the
/// process lifetime.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct ProcessTerminalIo {
    input: Option<StdinReader>,
    pending: std::collections::VecDeque<u8>,
}

/// The background stdin reader.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct StdinReader {
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ProcessTerminalIo {
    /// Start (once) and return the background reader.
    fn reader(&mut self) -> &mut StdinReader {
        self.input.get_or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match std::io::stdin().lock().read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            if sender.send(buffer[..count].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            StdinReader { receiver }
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl TerminalIo for ProcessTerminalIo {
    fn write(&mut self, data: &str) {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(data.as_bytes());
        let _ = stdout.flush();
    }

    fn size(&self) -> (usize, usize) {
        crossterm::terminal::size()
            .map(|(columns, rows)| (columns as usize, rows as usize))
            .unwrap_or((0, 0))
    }

    fn enable_raw_mode(&mut self) -> bool {
        crossterm::terminal::enable_raw_mode().is_ok()
    }

    fn disable_raw_mode(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }

    /// Read the bytes that arrived within `timeout`, `None` on idle.
    fn read_input(&mut self, buffer: &mut [u8], timeout: Duration) -> Option<usize> {
        if self.pending.is_empty() {
            let chunk = {
                let reader = self.reader();
                reader.receiver.recv_timeout(timeout).ok()?
            };
            self.pending.extend(chunk);
        }
        let count = buffer.len().min(self.pending.len());
        for slot in buffer.iter_mut().take(count) {
            *slot = self.pending.pop_front().expect("checked length");
        }
        Some(count)
    }
}

/// A terminal I/O that does nothing (wasm hosts and tests).
#[derive(Debug, Default)]
pub struct NullTerminalIo;

impl TerminalIo for NullTerminalIo {
    fn write(&mut self, _data: &str) {}
    fn size(&self) -> (usize, usize) {
        (0, 0)
    }
    fn enable_raw_mode(&mut self) -> bool {
        false
    }
    fn disable_raw_mode(&mut self) {}
    fn read_input(&mut self, _buffer: &mut [u8], _timeout: Duration) -> Option<usize> {
        None
    }
}

impl Terminal for ProcessTerminal {
    fn start(&mut self) {
        self.io.enable_raw_mode();
        // Bracketed paste: the terminal wraps pastes in \x1b[200~ ... \x1b[201~.
        self.write("\u{1b}[?2004h");
        self.keyboard_protocol_pushed = true;
        self.write(&kitty_keyboard_protocol_query());
    }

    fn stop(&mut self) {
        if self.progress_active {
            self.set_progress(false);
        }
        self.write("\u{1b}[?2004l");
        self.disable_kitty_protocol();
        self.disable_modify_other_keys();
        self.stdin_buffer.destroy();
        self.io.disable_raw_mode();
    }

    fn drain_input(&mut self, max_ms: u64, idle_ms: u64) {
        // Disable the kitty protocol first so late key releases do not
        // generate new escape sequences (upstream `drainInput`).
        self.disable_kitty_protocol();
        self.disable_modify_other_keys();

        let start = Instant::now();
        let mut last_data = Instant::now();
        let mut buffer = [0u8; 4096];
        while start.elapsed() < Duration::from_millis(max_ms)
            && last_data.elapsed() < Duration::from_millis(idle_ms)
        {
            match self
                .io
                .read_input(&mut buffer, Duration::from_millis(idle_ms))
            {
                None | Some(0) => break,
                Some(_) => last_data = Instant::now(),
            }
        }
    }

    fn write(&mut self, data: &str) {
        self.io.write(data);
    }

    fn read_input(&mut self, timeout: Duration) -> Option<String> {
        let mut buffer = [0u8; 4096];
        let count = self.io.read_input(&mut buffer, timeout)?;
        if count == 0 {
            return None;
        }
        Some(String::from_utf8_lossy(&buffer[..count]).to_string())
    }

    fn feed_input_bytes(&mut self, data: &str, now: Instant) -> Vec<String> {
        let outcome = self.stdin_buffer.process_with_clock(data, now);
        self.pending_flush = outcome.flush_deadline;
        let mut sequences = outcome.data;
        if let Some(paste) = outcome.paste {
            // Re-wrap paste content in the bracketed paste markers the editor
            // expects (upstream the StdinBuffer `paste` handler).
            sequences.push(format!("\u{1b}[200~{paste}\u{1b}[201~"));
        }
        sequences
    }

    fn flush_pending_input(&mut self, now: Instant) -> Vec<String> {
        if self.pending_flush.is_none_or(|deadline| now < deadline) {
            return Vec::new();
        }
        self.pending_flush = None;
        self.stdin_buffer.flush()
    }

    fn handle_sequence(&mut self, sequence: &str) -> Option<String> {
        let outcome = self.negotiator.feed(sequence);
        let mut forward = None;
        if let NegotiationOutcome::Forward = outcome {
            // A buffered partial response that turned out not to be one is
            // released first (upstream flushKeyboardProtocolNegotiationBufferAsInput).
            if let Some(buffered) = self.negotiator.flush_pending() {
                forward = Some(buffered);
            }
            if forward.is_none() {
                forward = Some(sequence.to_string());
            }
        }
        self.kitty_protocol_active = self.negotiator.kitty_protocol_active();
        self.modify_other_keys_active = self.negotiator.modify_other_keys_active();
        if self.modify_other_keys_active {
            self.write("\u{1b}[>4;2m");
        }
        forward
    }

    fn normalize_input(&self, sequence: &str) -> String {
        let detect_shift_enter =
            sequence == "\r" && (self.apple_terminal || cfg!(target_os = "windows"));
        if detect_shift_enter {
            return normalize_native_shift_enter_input(
                sequence,
                true,
                is_native_modifier_pressed(ModifierKey::Shift),
            );
        }
        if self.apple_terminal {
            return normalize_apple_terminal_input(sequence, true, false);
        }
        sequence.to_string()
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn resize_if_changed(&mut self) -> bool {
        let (columns, rows) = self.io.size();
        if columns == 0 || rows == 0 {
            return false;
        }
        if columns == self.columns && rows == self.rows {
            return false;
        }
        self.columns = columns;
        self.rows = rows;
        true
    }

    fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }

    fn move_by(&mut self, lines: i64) {
        if lines > 0 {
            self.write(&format!("\u{1b}[{lines}B"));
        } else if lines < 0 {
            self.write(&format!("\u{1b}[{}A", -lines));
        }
    }

    fn hide_cursor(&mut self) {
        self.write("\u{1b}[?25l");
    }

    fn show_cursor(&mut self) {
        self.write("\u{1b}[?25h");
    }

    fn clear_line(&mut self) {
        self.write("\u{1b}[K");
    }

    fn clear_from_cursor(&mut self) {
        self.write("\u{1b}[J");
    }

    fn clear_screen(&mut self) {
        self.write("\u{1b}[2J\u{1b}[H");
    }

    fn set_title(&mut self, title: &str) {
        self.write(&format!("\u{1b}]0;{title}\u{7}"));
    }

    fn set_progress(&mut self, active: bool) {
        if active {
            self.write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            self.progress_active = true;
            self.last_progress_write = Some(Instant::now());
        } else {
            self.progress_active = false;
            self.last_progress_write = None;
            self.write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }

    fn progress_keepalive(&mut self, now: Instant) -> bool {
        if !self.progress_active {
            return false;
        }
        let due = self.last_progress_write.is_some_and(|last| {
            now.duration_since(last) >= Duration::from_millis(TERMINAL_PROGRESS_KEEPALIVE_MS)
        });
        if due {
            self.write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            self.last_progress_write = Some(now);
        }
        due
    }
}
