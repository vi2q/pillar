//! Parity tests for packages/tui/src/tui-main-screen.ts (pi v0.84.3): the
//! differential renderer's output paths, driven through a recording terminal.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::process_terminal::Terminal;
use pillar_tui::tui::{Component, TuiStopOptions};
use pillar_tui::tui_main_screen::{TuiMainScreen, TuiMainScreenRenderState};

/// A terminal that records writes and reports a fixed size.
#[derive(Default, Clone)]
struct RecorderTerminal {
    writes: Arc<Mutex<Vec<String>>>,
    size: Arc<Mutex<(usize, usize)>>,
}

impl RecorderTerminal {
    fn new(columns: usize, rows: usize) -> Self {
        Self {
            writes: Arc::new(Mutex::new(Vec::new())),
            size: Arc::new(Mutex::new((columns, rows))),
        }
    }

    fn written(&self) -> String {
        self.writes.lock().unwrap().join("")
    }

    fn clear_writes(&self) {
        self.writes.lock().unwrap().clear();
    }

    fn set_size(&self, columns: usize, rows: usize) {
        *self.size.lock().unwrap() = (columns, rows);
    }
}

/// A component serving fixed lines.
struct Lines {
    lines: Vec<String>,
}

impl Component for Lines {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

fn terminal(columns: usize, rows: usize) -> RecorderTerminal {
    RecorderTerminal::new(columns, rows)
}

impl Terminal for RecorderTerminal {
    fn start(&mut self) {}
    fn stop(&mut self) {}
    fn drain_input(&mut self, _max_ms: u64, _idle_ms: u64) {}
    fn write(&mut self, data: &str) {
        self.writes.lock().unwrap().push(data.to_string());
    }
    fn read_input(&mut self, _timeout: Duration) -> Option<String> {
        None
    }
    fn feed_input_bytes(&mut self, data: &str, _now: Instant) -> Vec<String> {
        vec![data.to_string()]
    }
    fn flush_pending_input(&mut self, _now: Instant) -> Vec<String> {
        Vec::new()
    }
    fn handle_sequence(&mut self, sequence: &str) -> Option<String> {
        Some(sequence.to_string())
    }
    fn normalize_input(&self, sequence: &str) -> String {
        sequence.to_string()
    }
    fn columns(&self) -> usize {
        self.size.lock().unwrap().0
    }
    fn rows(&self) -> usize {
        self.size.lock().unwrap().1
    }
    fn resize_if_changed(&mut self) -> bool {
        false
    }
    fn kitty_protocol_active(&self) -> bool {
        false
    }
    fn move_by(&mut self, _lines: i64) {}
    fn hide_cursor(&mut self) {
        self.write("\u{1b}[?25l");
    }
    fn show_cursor(&mut self) {
        self.write("\u{1b}[?25h");
    }
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
    fn set_progress(&mut self, _active: bool) {}
    fn progress_keepalive(&mut self, _now: Instant) -> bool {
        false
    }
}

fn screen(terminal: &RecorderTerminal, lines: &[&str]) -> TuiMainScreen {
    let mut screen = TuiMainScreen::new(Box::new(terminal.clone()));
    screen.base_mut().start();
    screen.base_mut().set_show_hardware_cursor(false);
    screen.base_mut().set_clear_on_shrink(true);
    screen.base_mut().add_child(Box::new(Lines {
        lines: lines.iter().map(|line| line.to_string()).collect(),
    }));
    screen
}

#[test]
fn first_render_writes_every_line_without_clearing() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha", "beta"]);
    terminal.clear_writes();

    screen.do_render().expect("render");
    let written = terminal.written();
    assert!(written.contains("alpha"), "{written:?}");
    assert!(written.contains("beta"), "{written:?}");
    assert!(written.contains("\u{1b}[?2026h"), "synchronized start");
    assert!(written.contains("\u{1b}[?2026l"), "synchronized end");
    assert!(
        !written.contains("\u{1b}[2J"),
        "first render must not clear"
    );
    assert_eq!(screen.base().full_redraws(), 1);
}

#[test]
fn unchanged_content_writes_no_frame() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha", "beta"]);
    screen.do_render().expect("first render");
    terminal.clear_writes();

    screen.do_render().expect("second render");
    let written = terminal.written();
    assert!(!written.contains("alpha"), "{written:?}");
    assert!(!written.contains("beta"), "{written:?}");
    assert!(
        !written.contains("\u{1b}[?2026h"),
        "no frame for unchanged content: {written:?}"
    );
}

#[test]
fn a_single_changed_line_is_repainted_differentially() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha", "beta", "gamma"]);
    screen.do_render().expect("first render");
    terminal.clear_writes();

    // Replace the middle line.
    screen.base_mut().clear();
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec!["alpha".to_string(), "BETA".to_string(), "gamma".to_string()],
    }));
    screen.do_render().expect("differential render");

    let written = terminal.written();
    assert!(written.contains("\u{1b}[2K"), "clears the line first");
    assert!(written.contains("BETA"), "{written:?}");
    assert!(
        !written.contains("alpha"),
        "unchanged lines are not repainted: {written:?}"
    );
    assert!(!written.contains("\u{1b}[2J"), "no full clear: {written:?}");
    assert_eq!(screen.base().full_redraws(), 1);
}

#[test]
fn appending_lines_scrolls_instead_of_clearing() {
    let terminal = terminal(40, 3);
    let mut screen = screen(&terminal, &["one", "two"]);
    screen.do_render().expect("first render");
    terminal.clear_writes();

    screen.base_mut().clear();
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
    }));
    screen.do_render().expect("append render");

    let written = terminal.written();
    assert!(written.contains("three"), "{written:?}");
    assert!(
        !written.contains("\u{1b}[2J"),
        "appends scroll, they do not clear"
    );
}

#[test]
fn shrinking_content_clears_when_clear_on_shrink_is_on() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["one", "two", "three", "four"]);
    screen.do_render().expect("first render");
    terminal.clear_writes();

    screen.base_mut().clear();
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec!["one".to_string()],
    }));
    screen.do_render().expect("shrink render");

    let written = terminal.written();
    assert!(
        written.contains("\u{1b}[2J\u{1b}[H\u{1b}[3J"),
        "clear on shrink: {written:?}"
    );
    assert_eq!(screen.base().full_redraws(), 2);
}

#[test]
fn width_changes_force_a_full_clear() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha", "beta"]);
    screen.do_render().expect("first render");
    terminal.clear_writes();

    terminal.set_size(20, 10);
    screen.do_render().expect("resize render");

    let written = terminal.written();
    assert!(
        written.contains("\u{1b}[2J"),
        "width change clears: {written:?}"
    );
    assert!(written.contains("alpha"));
}

#[test]
fn a_line_wider_than_the_terminal_fails_and_restores_the_terminal() {
    let terminal = terminal(5, 10);
    let mut screen = screen(&terminal, &["ok"]);
    screen.do_render().expect("first render");
    // The width check lives on the differential path (upstream checks while
    // repainting a changed line, not on the first full render).
    screen.base_mut().clear();
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec!["far too wide".to_string()],
    }));
    let error = screen.do_render().expect_err("overflow");
    assert!(error.contains("exceeds terminal width"), "{error}");
    assert!(
        error.contains("visibleWidth()"),
        "the error keeps upstream's guidance: {error}"
    );
    // `stop()` restored the cursor before surfacing the fault.
    assert!(
        terminal.written().contains("\u{1b}[?25h"),
        "{:?}",
        terminal.written()
    );
}

#[test]
fn render_state_is_captured_restored_and_reset() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha"]);
    screen.do_render().expect("render");
    let state: TuiMainScreenRenderState = screen.capture_render_state();
    assert_eq!(state.previous_lines, vec!["alpha\u{1b}[0m\u{1b}]8;;\u{7}"]);
    assert_eq!(state.previous_width, 40);
    assert_eq!(state.previous_height, 10);

    screen.reset_render_state();
    let reset = screen.capture_render_state();
    assert!(reset.previous_lines.is_empty());
    assert_eq!(reset.previous_width, 0);

    screen.restore_render_state(&state);
    let restored = screen.capture_render_state();
    assert_eq!(restored.previous_lines, state.previous_lines);
    assert_eq!(restored.previous_width, 40);
}

#[test]
fn stopping_moves_the_cursor_past_the_content() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &["alpha", "beta"]);
    screen.do_render().expect("render");
    terminal.clear_writes();

    screen.before_terminal_stop(TuiStopOptions::default());
    let written = terminal.written();
    assert!(written.starts_with(' '), "{written:?}");
    assert!(written.ends_with("\r\n"), "{written:?}");

    // Preserving the screen leaves the cursor alone.
    terminal.clear_writes();
    screen.before_terminal_stop(TuiStopOptions {
        preserve_screen: true,
    });
    assert!(terminal.written().is_empty());
}

#[test]
fn hardware_cursor_follows_the_marker_when_enabled() {
    let terminal = terminal(40, 10);
    let mut screen = screen(&terminal, &[]);
    screen.base_mut().set_show_hardware_cursor(true);
    screen.base_mut().clear();
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec![format!("ab{}cd", pillar_tui::tui::CURSOR_MARKER)],
    }));
    terminal.clear_writes();

    screen.do_render().expect("render");
    let written = terminal.written();
    assert!(written.contains("\u{1b}[?25h"), "cursor shown: {written:?}");
    // The marker is stripped and the column is positioned (2 visible columns ->
    // absolute column 3).
    assert!(!written.contains("_pi:c"), "marker stripped: {written:?}");
    assert!(written.contains("\u{1b}[3G"), "{written:?}");
}
