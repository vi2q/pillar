//! Parity tests for packages/tui/src/terminal.ts (pi v0.84.3): the
//! `Terminal` surface of `ProcessTerminal` with injected I/O.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::process_terminal::{
    NullTerminalIo, ProcessTerminal, TERMINAL_PROGRESS_ACTIVE_SEQUENCE,
    TERMINAL_PROGRESS_CLEAR_SEQUENCE, Terminal, TerminalIo,
};
use pillar_tui::terminal::kitty_keyboard_protocol_query;

#[derive(Clone, Default)]
struct FakeIo {
    writes: Arc<Mutex<Vec<String>>>,
    size: Arc<Mutex<(usize, usize)>>,
    raw_mode: Arc<Mutex<bool>>,
    input: Arc<Mutex<Vec<u8>>>,
}

impl FakeIo {
    fn new(columns: usize, rows: usize) -> Self {
        Self {
            size: Arc::new(Mutex::new((columns, rows))),
            ..Default::default()
        }
    }

    fn written(&self) -> String {
        self.writes.lock().unwrap().join("")
    }

    fn set_size(&self, columns: usize, rows: usize) {
        *self.size.lock().unwrap() = (columns, rows);
    }

    fn push_input(&self, data: &[u8]) {
        self.input.lock().unwrap().extend_from_slice(data);
    }
}

impl TerminalIo for FakeIo {
    fn write(&mut self, data: &str) {
        self.writes.lock().unwrap().push(data.to_string());
    }

    fn size(&self) -> (usize, usize) {
        *self.size.lock().unwrap()
    }

    fn enable_raw_mode(&mut self) -> bool {
        *self.raw_mode.lock().unwrap() = true;
        true
    }

    fn disable_raw_mode(&mut self) {
        *self.raw_mode.lock().unwrap() = false;
    }

    fn read_input(&mut self, buffer: &mut [u8], _timeout: Duration) -> Option<usize> {
        let mut input = self.input.lock().unwrap();
        if input.is_empty() {
            return None;
        }
        let count = buffer.len().min(input.len());
        buffer[..count].copy_from_slice(&input[..count]);
        input.drain(..count);
        Some(count)
    }
}

fn terminal(io: &FakeIo) -> ProcessTerminal {
    ProcessTerminal::with_io(Box::new(io.clone()))
}

#[test]
fn start_enables_raw_mode_bracketed_paste_and_kitty_query() {
    let io = FakeIo::new(100, 30);
    let mut terminal = terminal(&io);
    assert_eq!(terminal.columns(), 100);
    assert_eq!(terminal.rows(), 30);
    assert!(!terminal.kitty_protocol_active());

    terminal.start();
    assert!(*io.raw_mode.lock().unwrap(), "raw mode must be enabled");
    let written = io.written();
    assert!(written.starts_with("\u{1b}[?2004h"), "{written:?}");
    assert!(
        written.contains(&kitty_keyboard_protocol_query()),
        "{written:?}"
    );
}

#[test]
fn cursor_clear_title_and_move_sequences_match_upstream() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);

    terminal.move_by(3);
    terminal.move_by(-2);
    terminal.move_by(0);
    terminal.hide_cursor();
    terminal.show_cursor();
    terminal.clear_line();
    terminal.clear_from_cursor();
    terminal.clear_screen();
    terminal.set_title("hello");

    assert_eq!(
        io.written(),
        "\u{1b}[3B\u{1b}[2A\u{1b}[?25l\u{1b}[?25h\u{1b}[K\u{1b}[J\u{1b}[2J\u{1b}[H\u{1b}]0;hello\u{7}"
    );
}

#[test]
fn stop_restores_the_terminal_state() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    terminal.start();
    io.writes.lock().unwrap().clear();

    terminal.stop();
    let written = io.written();
    assert!(
        written.contains("\u{1b}[?2004l"),
        "bracketed paste off: {written:?}"
    );
    assert!(
        written.contains("\u{1b}[<u"),
        "kitty protocol off: {written:?}"
    );
    assert!(!*io.raw_mode.lock().unwrap(), "raw mode must be restored");
}

#[test]
fn progress_indicator_emits_osc_9_4_and_keeps_alive() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    let now = Instant::now();

    assert!(
        !terminal.progress_keepalive(now),
        "inactive progress is silent"
    );
    terminal.set_progress(true);
    assert_eq!(io.written(), TERMINAL_PROGRESS_ACTIVE_SEQUENCE);

    io.writes.lock().unwrap().clear();
    assert!(!terminal.progress_keepalive(now + Duration::from_millis(999)));
    assert!(terminal.progress_keepalive(now + Duration::from_millis(1001)));
    assert_eq!(io.written(), TERMINAL_PROGRESS_ACTIVE_SEQUENCE);

    io.writes.lock().unwrap().clear();
    terminal.set_progress(false);
    assert_eq!(io.written(), TERMINAL_PROGRESS_CLEAR_SEQUENCE);
    assert!(!terminal.progress_keepalive(now + Duration::from_secs(5)));
}

#[test]
fn input_bytes_become_sequences_and_pastes_are_rewrapped() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    let now = Instant::now();

    // A plain key is one sequence.
    assert_eq!(terminal.feed_input_bytes("a", now), vec!["a".to_string()]);

    // Bracketed paste arrives as one wrapped sequence for the editor.
    let pasted = terminal.feed_input_bytes("\u{1b}[200~line1\nline2\u{1b}[201~", now);
    assert_eq!(
        pasted,
        vec!["\u{1b}[200~line1\nline2\u{1b}[201~".to_string()]
    );

    // An escape sequence is kept intact.
    let arrows = terminal.feed_input_bytes("\u{1b}[A", now);
    assert_eq!(arrows, vec!["\u{1b}[A".to_string()]);
}

#[test]
fn kitty_negotiation_responses_are_consumed_and_flags_enabled() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);

    // A kitty flags response enables the protocol and is not forwarded.
    assert_eq!(terminal.handle_sequence("\u{1b}[?7u"), None);
    assert!(terminal.kitty_protocol_active());

    // Ordinary input is forwarded.
    assert_eq!(terminal.handle_sequence("x"), Some("x".to_string()));
}

#[test]
fn device_attributes_fall_back_to_modify_other_keys() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);

    // A DA response without kitty support enables modifyOtherKeys.
    assert_eq!(terminal.handle_sequence("\u{1b}[?1;0c"), None);
    assert!(!terminal.kitty_protocol_active());
    assert!(io.written().contains("\u{1b}[>4;2m"), "{:?}", io.written());
}

#[test]
fn resize_is_detected_by_comparing_the_reported_size() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    assert!(!terminal.resize_if_changed());

    io.set_size(120, 40);
    assert!(terminal.resize_if_changed());
    assert_eq!(terminal.columns(), 120);
    assert_eq!(terminal.rows(), 40);
    assert!(!terminal.resize_if_changed(), "no further change");
}

#[test]
fn read_input_surfaces_raw_bytes_and_handles_an_idle_terminal() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    assert_eq!(terminal.read_input(Duration::from_millis(1)), None);

    io.push_input(b"\x1b[A");
    assert_eq!(
        terminal.read_input(Duration::from_millis(1)),
        Some("\u{1b}[A".to_string())
    );
}

#[test]
fn escape_timeout_and_normalization_follow_the_environment() {
    let io = FakeIo::new(80, 24);
    let terminal = terminal(&io);
    // The default reassembly window is upstream's 10ms (or 100ms over SSH).
    assert!(terminal.escape_timeout_ms() == 10 || terminal.escape_timeout_ms() == 100);
    // Without Apple Terminal / native modifiers, input passes through.
    assert_eq!(terminal.normalize_input("a"), "a");
    assert_eq!(terminal.normalize_input("\u{1b}[A"), "\u{1b}[A");
}

#[test]
fn null_io_keeps_the_terminal_usable_on_wasm_hosts() {
    let mut terminal = ProcessTerminal::with_io(Box::new(NullTerminalIo));
    // No size from the host: upstream's 80x24 fallback applies.
    assert_eq!(terminal.columns(), 80);
    assert_eq!(terminal.rows(), 24);
    terminal.start();
    terminal.set_title("t");
    terminal.stop();
    assert_eq!(terminal.read_input(Duration::from_millis(1)), None);
}

#[test]
fn a_buffered_partial_sequence_flushes_once_its_deadline_passes() {
    let io = FakeIo::new(80, 24);
    let mut terminal = terminal(&io);
    let start = Instant::now();

    // A lone Escape is held for the escape window (upstream the StdinBuffer
    // `setTimeout` the host has to drive).
    assert!(terminal.feed_input_bytes("\u{1b}", start).is_empty());
    assert!(
        terminal.flush_pending_input(start).is_empty(),
        "not due yet"
    );
    assert_eq!(
        terminal
            .flush_pending_input(start + Duration::from_millis(10))
            .len(),
        1,
        "the lone Escape is released as input"
    );
    assert!(
        terminal
            .flush_pending_input(start + Duration::from_millis(50))
            .is_empty(),
        "flushed once"
    );

    // New input resets the window; a complete sequence never flushes.
    assert!(terminal.feed_input_bytes("\u{1b}[", start).is_empty());
    assert_eq!(
        terminal.feed_input_bytes("A", start),
        vec!["\u{1b}[A".to_string()]
    );
    assert!(terminal
        .flush_pending_input(start + Duration::from_secs(1))
        .is_empty());
}
