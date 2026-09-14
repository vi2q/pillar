//! Parity tests for packages/tui/src/tui-alt-screen.ts (pi v0.84.3), first
//! slice: lifecycle sequences, viewport scrolling, search state and frames.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::process_terminal::Terminal;
use pillar_tui::tui::{Component, TuiStopOptions};
use pillar_tui::tui_alt_screen::{SearchSelectionMode, TuiAltScreen, TuiAltScreenOptions};

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
}

struct Lines {
    lines: Vec<String>,
}

impl Component for Lines {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
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

fn screen(terminal: &RecorderTerminal, lines: &[&str]) -> TuiAltScreen {
    let mut screen = TuiAltScreen::new(Box::new(terminal.clone()), TuiAltScreenOptions::default());
    screen.base_mut().set_show_hardware_cursor(false);
    screen.base_mut().add_child(Box::new(Lines {
        lines: lines.iter().map(|line| line.to_string()).collect(),
    }));
    screen
}

#[test]
fn start_enters_the_alt_screen_and_stop_leaves_it() {
    let terminal = RecorderTerminal::new(40, 5);
    let mut screen = screen(&terminal, &["alpha"]);

    screen.start();
    let written = terminal.written();
    assert!(written.contains("\u{1b}[?1049h"), "alt screen: {written:?}");
    assert!(
        written.contains("\u{1b}[?7l"),
        "autowrap disabled: {written:?}"
    );
    assert!(
        written.contains("\u{1b}[2J\u{1b}[H"),
        "home + clear: {written:?}"
    );

    terminal.clear_writes();
    screen.stop(TuiStopOptions::default());
    let written = terminal.written();
    assert!(
        written.contains("\u{1b}[?7h"),
        "autowrap restored: {written:?}"
    );
    assert!(
        written.contains("\u{1b}[?2026h"),
        "synchronized output: {written:?}"
    );
}

#[test]
fn a_frame_paints_the_viewport_and_diffs_afterwards() {
    let terminal = RecorderTerminal::new(20, 4);
    let mut screen = screen(&terminal, &["one", "two", "three"]);
    screen.start();
    terminal.clear_writes();

    screen.do_render().expect("first frame");
    let written = terminal.written();
    assert!(written.contains("one"), "{written:?}");
    assert!(written.contains("three"), "{written:?}");
    assert!(
        written.contains("\u{1b}[2J"),
        "full redraw clears: {written:?}"
    );
    assert_eq!(screen.base().full_redraws(), 1);

    // An unchanged frame repaints nothing.
    terminal.clear_writes();
    screen.do_render().expect("second frame");
    let written = terminal.written();
    assert!(!written.contains("one"), "no repaint: {written:?}");
    assert!(!written.contains("\u{1b}[2J"), "no clear: {written:?}");
}

#[test]
fn scrolling_moves_the_viewport_and_requests_a_render() {
    let terminal = RecorderTerminal::new(20, 2);
    let mut screen = screen(&terminal, &["l0", "l1", "l2", "l3", "l4"]);
    screen.start();
    screen.do_render().expect("frame");
    assert!(screen.is_following_output(), "starts following the end");
    let bottom = screen.viewport_top();

    screen.scroll_by(-2);
    assert!(screen.viewport_top() < bottom);
    assert!(!screen.is_following_output());
    assert!(
        screen
            .base()
            .render_due(Instant::now() + Duration::from_millis(20))
    );

    screen.scroll_to_top();
    assert_eq!(screen.viewport_top(), 0);
    screen.scroll_to_bottom();
    assert_eq!(screen.viewport_top(), bottom);
}

#[test]
fn search_selects_matches_and_scrolls_to_reveal_them() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(&terminal, &["needle a", "b", "needle c", "d"]);
    screen.start();
    screen.do_render().expect("frame");

    screen.open_search();
    screen.update_search_query("needle");
    screen.do_render().expect("search frame");

    let search = screen.active_search().expect("active search");
    assert_eq!(search.matches.len(), 2);
    // A new query selects the first match at/after the anchor row, which is
    // the viewport top when the search opened (upstream `selectionMode:
    // "query"`).
    assert_eq!(search.selected_index, Some(1));
    assert_eq!(search.selection_mode, SearchSelectionMode::Retain);

    // Next wraps around to the first match and reveals it.
    screen.navigate_search(1);
    screen.do_render().expect("next frame");
    assert_eq!(
        screen.active_search().expect("search").selected_index,
        Some(0)
    );

    // Previous wraps back to the later match.
    screen.navigate_search(-1);
    screen.do_render().expect("previous frame");
    assert_eq!(
        screen.active_search().expect("search").selected_index,
        Some(1)
    );

    screen.close_search();
    assert!(screen.active_search().is_none());
}

#[test]
fn an_empty_query_clears_search_state() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(&terminal, &["needle"]);
    screen.start();
    screen.do_render().expect("frame");

    screen.open_search();
    screen.update_search_query("needle");
    screen.do_render().expect("search frame");
    assert_eq!(screen.active_search().expect("search").matches.len(), 1);

    screen.update_search_query("   ");
    screen.do_render().expect("empty query frame");
    let search = screen.active_search().expect("search");
    assert!(search.matches.is_empty());
    assert_eq!(search.selected_index, None);
}

#[test]
fn flashes_render_on_the_last_rows_and_expire() {
    let terminal = RecorderTerminal::new(30, 4);
    let mut screen = screen(&terminal, &["content"]);
    screen.start();

    screen.flash("saved!", Some(50));
    screen.do_render().expect("flash frame");
    let written = terminal.written();
    assert!(written.contains("saved!"), "{written:?}");

    // Expiry is host-driven.
    assert!(screen.expire_flashes(Instant::now() + Duration::from_millis(100)));

    terminal.clear_writes();
    screen.do_render().expect("post-expiry frame");
    let written = terminal.written();
    assert!(!written.contains("saved!"), "{written:?}");
}

#[test]
fn osc133_prompt_markers_drive_scroll_to_prompt_and_are_stripped() {
    let terminal = RecorderTerminal::new(30, 3);
    let mut screen = screen(
        &terminal,
        &[
            "l0",
            "\u{1b}]133;A\u{7}prompt one",
            "l2",
            "\u{1b}]133;A\u{7}prompt two",
            "l4",
            "l5",
            "l6",
        ],
    );
    screen.start();
    screen.do_render().expect("frame");

    screen.scroll_to_top();
    screen.scroll_to_prompt(1);
    assert_eq!(screen.viewport_top(), 1, "first prompt marker");

    screen.scroll_to_prompt(1);
    assert_eq!(screen.viewport_top(), 3, "second prompt marker");

    // Zone prefixes never reach the terminal.
    let written = terminal.written();
    assert!(!written.contains("\u{1b}]133;"), "stripped: {written:?}");
}
