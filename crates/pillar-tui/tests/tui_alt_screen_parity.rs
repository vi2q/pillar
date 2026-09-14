//! Parity tests for packages/tui/src/tui-alt-screen.ts (pi v0.84.3), first
//! slice: lifecycle sequences, viewport scrolling, search state and frames.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::process_terminal::Terminal;
use pillar_tui::tui::{Component, TuiStopOptions};
use pillar_tui::tui_alt_screen::{
    SearchSelectionMode, TuiAltScreen, TuiAltScreenOptions, is_mouse_sequence,
    parse_sgr_mouse_event, parse_wheel_event,
};

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
    // Type into the overlay's input, the way `handleTerminalInput` would
    // dispatch to the focused search component.
    screen.base_mut().handle_terminal_input("needle");
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
    screen.base_mut().handle_terminal_input("needle");
    screen.do_render().expect("search frame");
    assert_eq!(screen.active_search().expect("search").matches.len(), 1);

    // Backspacing the query away clears the matches.
    for _ in 0.."needle".len() {
        screen.base_mut().handle_terminal_input("\u{7f}");
    }
    screen.do_render().expect("empty query frame");
    let search = screen.active_search().expect("search");
    assert!(search.matches.is_empty());
    assert_eq!(search.selected_index, None);
}

#[test]
fn flashes_composite_right_aligned_and_expire() {
    let terminal = RecorderTerminal::new(30, 4);
    let mut screen = screen(&terminal, &["content"]);
    screen.start();

    screen.flash("saved!", Some(50));
    screen.do_render().expect("flash frame");
    let written = terminal.written();
    // The flash keeps the row's content and lands right-aligned on the first
    // viewport row (upstream `compositeFlashes`).
    assert!(written.contains("saved!"), "{written:?}");
    let flash_row = written
        .split("\u{1b}[2K")
        .find(|segment| segment.contains("saved!"))
        .expect("flash row");
    assert!(flash_row.starts_with("content"), "{flash_row:?}");

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

#[test]
fn wheel_and_sgr_mouse_sequences_parse_like_upstream() {
    // SGR wheel up (button 64) at 1-based (10, 5) -> 0-based (9, 4).
    assert_eq!(
        parse_wheel_event("\u{1b}[<64;10;5M"),
        Some(pillar_tui::tui_alt_screen::WheelEvent {
            direction: -1,
            x: 9,
            y: 4
        })
    );
    // SGR wheel down (button 65).
    assert_eq!(
        parse_wheel_event("\u{1b}[<65;1;1M").map(|event| event.direction),
        Some(1)
    );
    // A non-wheel SGR button is not a wheel event.
    assert!(parse_wheel_event("\u{1b}[<0;10;5M").is_none());
    // Legacy X10 encoding: ESC [ M <button> <x> <y>.
    let legacy = format!(
        "\u{1b}[M{}{}{}",
        char::from_u32(64 + 32).expect("button"),
        char::from_u32(33 + 3).expect("x"),
        char::from_u32(33 + 2).expect("y")
    );
    assert_eq!(
        parse_wheel_event(&legacy),
        Some(pillar_tui::tui_alt_screen::WheelEvent {
            direction: -1,
            x: 3,
            y: 2
        })
    );

    let press = parse_sgr_mouse_event("\u{1b}[<0;5;2M").expect("press");
    assert_eq!(press.button, 0);
    assert_eq!((press.x, press.y), (4, 1));
    assert!(!press.release);
    let release = parse_sgr_mouse_event("\u{1b}[<0;5;2m").expect("release");
    assert!(release.release);

    assert!(is_mouse_sequence("\u{1b}[<0;5;2M"));
    assert!(is_mouse_sequence("\u{1b}[M\u{20}\u{21}\u{22}"));
    assert!(!is_mouse_sequence("\u{1b}[A"));
}

#[test]
fn wheel_routing_scrolls_the_viewport_and_consumes_input() {
    let terminal = RecorderTerminal::new(20, 2);
    let mut screen = screen(&terminal, &["l0", "l1", "l2", "l3", "l4"]);
    screen.start();
    screen.do_render().expect("frame");
    assert!(screen.is_following_output());

    // Wheel up scrolls back one line per notch and consumes the sequence.
    // (5 lines in a 2-row viewport: the end is at the top of 3.)
    let consumed = screen.handle_viewport_input("\u{1b}[<64;1;1M");
    assert!(consumed.is_some_and(|result| result.consume));
    assert!(!screen.is_following_output());
    assert_eq!(screen.viewport_top(), 2);

    // Wheel down returns to the end.
    screen.handle_viewport_input("\u{1b}[<65;1;1M");
    assert_eq!(screen.viewport_top(), 3);
}

#[test]
fn viewport_keybindings_scroll_and_are_consumed() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(&terminal, &["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"]);
    screen.start();
    screen.do_render().expect("frame");
    let bottom = screen.viewport_top();
    assert!(bottom > 0);

    // PageUp scrolls by viewport height minus the 4-line overlap.
    let consumed = screen.handle_viewport_input("\u{1b}[5~");
    assert!(consumed.is_some_and(|result| result.consume));
    assert!(screen.viewport_top() < bottom);

    // Home jumps to the top; End returns to the end.
    screen.handle_viewport_input("\u{1b}[H");
    assert_eq!(screen.viewport_top(), 0);
    screen.handle_viewport_input("\u{1b}[F");
    assert_eq!(screen.viewport_top(), bottom);

    // A plain key is not consumed by the viewport.
    assert!(screen.handle_viewport_input("x").is_none());
}

#[test]
fn focus_events_are_consumed_and_clear_transient_state() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(&terminal, &["a", "b", "c"]);
    screen.start();
    screen.do_render().expect("frame");

    let consumed = screen.handle_viewport_input("\u{1b}[O");
    assert!(consumed.is_some_and(|result| result.consume));
    assert!(!screen.is_scrollbar_dragging());
    let consumed = screen.handle_viewport_input("\u{1b}[I");
    assert!(consumed.is_some_and(|result| result.consume));
}

#[test]
fn the_frame_records_scrollbar_hit_geometry() {
    let terminal = RecorderTerminal::new(20, 4);
    let mut screen = screen(
        &terminal,
        &["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"],
    );
    // Hidden scrollbars are not hittable, so make it always visible the way
    // the interactive mode configures its viewport.
    screen
        .scroll_view_mut()
        .set_scrollbar(pillar_tui::loaders::ScrollViewScrollbar::Always);
    screen.start();
    screen.do_render().expect("frame");

    let hit = screen.scroll_hit().expect("hit snapshot");
    assert_eq!(hit.rect.2, 20, "viewport width");
    assert_eq!(hit.rect.3, 4, "viewport height");
    let geometry = hit.scrollbar.expect("scrollbar geometry");

    // A primary-button press inside the thumb starts a drag.
    let press = format!(
        "\u{1b}[<0;{};{}M",
        geometry.column + 1,
        geometry.thumb_top + 1
    );
    let consumed = screen.handle_viewport_input(&press);
    assert!(consumed.is_some_and(|result| result.consume));
    assert!(screen.is_scrollbar_dragging());

    // Motion drags the thumb; release ends the drag.
    let motion = format!(
        "\u{1b}[<32;{};{}M",
        geometry.column + 1,
        geometry.track_top + geometry.track_height
    );
    screen.handle_viewport_input(&motion);
    let release = format!(
        "\u{1b}[<0;{};{}m",
        geometry.column + 1,
        geometry.thumb_top + 1
    );
    screen.handle_viewport_input(&release);
    assert!(!screen.is_scrollbar_dragging());
}

#[test]
fn start_enables_mouse_tracking_when_configured() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = TuiAltScreen::new(
        Box::new(terminal.clone()),
        TuiAltScreenOptions {
            mouse: Some(true),
            ..Default::default()
        },
    );
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec!["a".to_string()],
    }));
    screen.start();
    let written = terminal.written();
    // Either the button-motion or the all-motion sequence, depending on the
    // multiplexer detection of this environment.
    assert!(
        written.contains("\u{1b}[?1006h") && written.contains("\u{1b}[?1000h"),
        "{written:?}"
    );

    // A disabled mouse leaves tracking alone.
    let plain = RecorderTerminal::new(20, 3);
    let mut screen = TuiAltScreen::new(
        Box::new(plain.clone()),
        TuiAltScreenOptions {
            mouse: Some(false),
            ..Default::default()
        },
    );
    screen.start();
    assert!(
        !plain.written().contains("\u{1b}[?1000h"),
        "{:?}",
        plain.written()
    );
}

// --- text selection (upstream the selection state machine) ---------------------------------------

fn sgr_press(x: usize, y: usize) -> String {
    format!("\u{1b}[<0;{};{}M", x + 1, y + 1)
}

fn sgr_motion(x: usize, y: usize) -> String {
    format!("\u{1b}[<32;{};{}M", x + 1, y + 1)
}

fn sgr_release(x: usize, y: usize) -> String {
    format!("\u{1b}[<0;{};{}m", x + 1, y + 1)
}

fn mouse_event(sequence: &str) -> pillar_tui::tui_alt_screen::SgrMouseEvent {
    parse_sgr_mouse_event(sequence).expect("mouse event")
}

#[test]
fn dragging_selects_text_and_copy_on_select_copies_it() {
    let copied: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&copied);
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = TuiAltScreen::new(
        Box::new(terminal.clone()),
        TuiAltScreenOptions {
            copy_selection: Some(Box::new(move |text: &str| {
                sink.lock().unwrap().push(text.to_string());
                true
            })),
            ..Default::default()
        },
    );
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec![
            "alpha bravo".to_string(),
            "charlie delta".to_string(),
            "echo".to_string(),
        ],
    }));
    screen.start();
    screen.do_render().expect("frame");
    assert!(!screen.has_active_selection());

    // Press at column 0, drag to column 4, release: "alpha" is selected.
    screen.handle_viewport_input(&sgr_press(0, 0));
    screen.handle_viewport_input(&sgr_motion(4, 0));
    assert_eq!(screen.active_selection_text().as_deref(), Some("alpha"));
    screen.handle_viewport_input(&sgr_release(4, 0));

    assert_eq!(copied.lock().unwrap().as_slice(), ["alpha".to_string()]);
    // Copy-on-select flashes its result.
    assert_eq!(screen.active_selection_text().as_deref(), Some("alpha"));

    // The frame inverse-videos the selected slice.
    terminal.clear_writes();
    screen.do_render().expect("selection frame");
    let written = terminal.written();
    assert!(written.contains("\u{1b}[7malpha\u{1b}[27m"), "{written:?}");

    // A focus-out only clears an in-progress drag (upstream checks
    // `selectionPressActive`), so the finished selection survives.
    screen.handle_viewport_input("\u{1b}[O");
    assert!(screen.has_active_selection());

    // Starting a new drag and losing focus drops the selection.
    screen.handle_viewport_input(&sgr_press(0, 0));
    screen.handle_viewport_input(&sgr_motion(4, 0));
    screen.handle_viewport_input("\u{1b}[O");
    assert!(!screen.has_active_selection());
}

#[test]
fn multi_line_drag_joins_rows_case() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(
        &terminal,
        &["alpha bravo", "charlie delta", "echo"],
    );
    screen.start();
    screen.do_render().expect("frame");

    // From column 6 of row 0 ("bravo" onwards) to column 2 of row 1. The
    // grapheme under the release column is included (upstream rounds the
    // end up to a grapheme cell range).
    screen.handle_viewport_input(&sgr_press(6, 0));
    screen.handle_viewport_input(&sgr_motion(2, 1));
    assert_eq!(
        screen.active_selection_text().as_deref(),
        Some("bravo\ncha")
    );
}

#[test]
fn double_and_triple_click_select_word_then_line() {
    let terminal = RecorderTerminal::new(20, 3);
    let mut screen = screen(&terminal, &["alpha bravo", "charlie", "echo"]);
    screen.start();
    screen.do_render().expect("frame");

    let base = Instant::now();
    let point = sgr_press(2, 0);

    // First click: a caret, not a selection.
    screen.handle_selection_mouse_event_at(&mouse_event(&point), base);
    assert!(!screen.has_active_selection());

    // Second click within the double-click window selects the word.
    screen.handle_selection_mouse_event_at(
        &mouse_event(&point),
        base + Duration::from_millis(100),
    );
    assert_eq!(screen.active_selection_text().as_deref(), Some("alpha"));

    // Third click selects the line.
    screen.handle_selection_mouse_event_at(
        &mouse_event(&point),
        base + Duration::from_millis(200),
    );
    assert_eq!(
        screen.active_selection_text().as_deref(),
        Some("alpha bravo")
    );

    // Outside the window the count restarts at a caret.
    screen.handle_selection_mouse_event_at(
        &mouse_event(&point),
        base + Duration::from_millis(5_000),
    );
    assert!(!screen.has_active_selection());
}

#[test]
fn word_selection_keeps_paths_and_kebab_tokens_whole() {
    let terminal = RecorderTerminal::new(40, 3);
    let mut subject = screen(&terminal, &["see src/a-b/c here"]);
    subject.start();
    subject.do_render().expect("frame");

    let base = Instant::now();
    let point = sgr_press(6, 0);
    subject.handle_selection_mouse_event_at(&mouse_event(&point), base);
    subject.handle_selection_mouse_event_at(
        &mouse_event(&point),
        base + Duration::from_millis(50),
    );
    // `/` and `-` join segments, so the path stays whole.
    assert_eq!(
        subject.active_selection_text().as_deref(),
        Some("src/a-b/c")
    );

    // A `.` is not a joiner (upstream `TERMINAL_WORD_SELECTION_JOINERS`).
    let dotted_terminal = RecorderTerminal::new(40, 3);
    let mut dotted = screen(&dotted_terminal, &["see a-b.c here"]);
    dotted.start();
    dotted.do_render().expect("frame");
    let dotted_point = sgr_press(4, 0);
    dotted.handle_selection_mouse_event_at(&mouse_event(&dotted_point), base);
    dotted.handle_selection_mouse_event_at(
        &mouse_event(&dotted_point),
        base + Duration::from_millis(50),
    );
    assert_eq!(dotted.active_selection_text().as_deref(), Some("a-b"));
}

#[test]
fn clicking_an_osc8_link_activates_it() {
    let opened: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&opened);
    let terminal = RecorderTerminal::new(30, 3);
    let mut screen = TuiAltScreen::new(
        Box::new(terminal.clone()),
        TuiAltScreenOptions {
            open_url: Some(Box::new(move |url: &str| {
                sink.lock().unwrap().push(url.to_string());
            })),
            ..Default::default()
        },
    );
    screen.base_mut().add_child(Box::new(Lines {
        lines: vec![format!("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7} tail")],
    }));
    screen.start();
    screen.do_render().expect("frame");

    screen.handle_viewport_input(&sgr_press(1, 0));
    screen.handle_viewport_input(&sgr_release(1, 0));
    assert_eq!(
        opened.lock().unwrap().as_slice(),
        ["https://example.com".to_string()]
    );
    // Activating a link clears the selection it started from.
    assert!(!screen.has_active_selection());
}

#[test]
fn dragging_past_the_viewport_edge_arms_auto_scroll() {
    let terminal = RecorderTerminal::new(20, 2);
    let mut screen = screen(&terminal, &["l0", "l1", "l2", "l3", "l4"]);
    screen.start();
    screen.do_render().expect("frame");
    assert_eq!(screen.viewport_top(), 3, "at the end");

    screen.handle_viewport_input(&sgr_press(0, 0));
    // Dragging above the viewport top arms an upward auto-scroll.
    screen.handle_viewport_input(&sgr_motion(0, 0));
    assert!(screen.selection_auto_scroll_active());

    screen.auto_scroll_selection();
    assert_eq!(screen.viewport_top(), 2);
    assert!(screen.selection_auto_scroll_active());

    screen.stop_selection_auto_scroll();
    assert!(!screen.selection_auto_scroll_active());
    assert!(!screen.has_active_selection());
}

#[test]
fn selection_respects_the_scroll_view_geometry() {
    let terminal = RecorderTerminal::new(20, 2);
    let mut screen = screen(&terminal, &["l0", "l1", "l2", "l3", "l4"]);
    screen.start();
    screen.do_render().expect("frame");
    // At the end, the top row of the viewport is content row 3.
    assert_eq!(screen.viewport_top(), 3);

    // Press on the viewport's first row and drag to its last row.
    screen.handle_viewport_input(&sgr_press(0, 0));
    screen.handle_viewport_input(&sgr_motion(3, 1));
    // The screen rows are the viewport rows, not content rows.
    assert_eq!(screen.active_selection_text().as_deref(), Some("l3\nl4"));
}

// --- search highlights and the overlay -----------------------------------------------------------

#[test]
fn search_highlights_and_the_overlay_reach_the_frame() {
    let terminal = RecorderTerminal::new(60, 4);
    let mut screen = screen(
        &terminal,
        &["needle one", "two", "needle three"],
    );
    screen.start();
    screen.do_render().expect("frame");

    screen.open_search();
    assert!(screen.search_component_mut().is_some(), "overlay component");
    screen.base_mut().handle_terminal_input("needle");
    terminal.clear_writes();
    screen.do_render().expect("search frame");

    let written = terminal.written();
    // Matches are underlined; the selected match is bold + reversed.
    assert!(written.contains("\u{1b}[4m"), "match style: {written:?}");
    assert!(
        written.contains("\u{1b}[1;7m"),
        "current match style: {written:?}"
    );
    // The search overlay renders its label and result count.
    assert!(written.contains("Find transcript"), "{written:?}");
    assert!(written.contains("1/2"), "{written:?}");

    // Closing the search drops the overlay again.
    screen.close_search();
    terminal.clear_writes();
    screen.do_render().expect("closed frame");
    let written = terminal.written();
    assert!(!written.contains("Find transcript"), "{written:?}");
}

#[test]
fn a_focused_overlay_defers_viewport_keybindings() {
    let terminal = RecorderTerminal::new(60, 4);
    let mut screen = screen(&terminal, &["a", "b", "c", "d", "e", "f", "g", "h"]);
    screen.start();
    screen.do_render().expect("frame");
    let top = screen.viewport_top();

    // With an unrelated overlay focused the viewport yields.
    struct Plain;
    impl Component for Plain {
        fn render(&mut self, _width: usize) -> Vec<String> {
            vec![]
        }
    }
    let id = screen
        .base_mut()
        .show_overlay(Box::new(Plain), pillar_tui::overlay::OverlayOptions::default());
    assert!(screen.base().overlay_is_focused(id));
    assert!(screen.handle_viewport_input("\u{1b}[H").is_none());
    assert_eq!(screen.viewport_top(), top);
}
