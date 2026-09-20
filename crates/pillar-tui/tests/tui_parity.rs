//! Parity tests for packages/tui/src/tui.ts (pi v0.84.3): the Component
//! trait, Container, the overlay/TUI option types, and the TuiBase core
//! (focus, input dispatch, overlays, render scheduling, terminal queries).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::process_terminal::Terminal;
use pillar_tui::tui::{
    Component, ComponentId, Container, Focusable, InputListenerResult, OverlayAnchor,
    OverlayMargin, OverlayOptions, RenderLines, SizeValue, TuiBase, TuiMode, TuiStopOptions,
    is_focusable, is_key_release, render_lines,
};

/// The shared frame as owned lines (the parity assertions compare strings).
fn to_vec(lines: RenderLines) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

#[derive(Default)]
struct Leaf {
    renders: usize,
    invalidations: usize,
    inputs: Vec<String>,
    focused: bool,
    focusable: bool,
    wants_release: bool,
    /// Focus changes, shared with the test (the component is boxed).
    focus_log: Arc<Mutex<Vec<bool>>>,
}

impl Leaf {
    fn new() -> Self {
        Self::default()
    }

    fn focusable() -> Self {
        Self {
            focusable: true,
            ..Self::default()
        }
    }
}

impl Focusable for Leaf {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.focus_log.lock().unwrap().push(focused);
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

impl Component for Leaf {
    fn render(&mut self, width: usize) -> RenderLines {
        self.renders += 1;
        render_lines(vec![format!("leaf:{width}")])
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
    }

    fn wants_key_release(&self) -> bool {
        self.wants_release
    }

    fn invalidate(&mut self) {
        self.invalidations += 1;
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        if self.focusable { Some(self) } else { None }
    }
}

/// A terminal that records writes and replays queued input.
#[derive(Default)]
struct TestTerminal {
    writes: Arc<Mutex<Vec<String>>>,
    input: Arc<Mutex<Vec<String>>>,
    size: (usize, usize),
}

impl TestTerminal {
    fn new() -> Self {
        Self {
            size: (80, 24),
            ..Default::default()
        }
    }

    fn written(&self) -> String {
        self.writes.lock().unwrap().join("")
    }

    fn push_input(&self, data: &str) {
        self.input.lock().unwrap().push(data.to_string());
    }
}

impl Terminal for TestTerminal {
    fn start(&mut self) {}
    fn stop(&mut self) {}
    fn drain_input(&mut self, _max_ms: u64, _idle_ms: u64) {}
    fn write(&mut self, data: &str) {
        self.writes.lock().unwrap().push(data.to_string());
    }
    fn read_input(&mut self, _timeout: Duration) -> Option<String> {
        let mut input = self.input.lock().unwrap();
        if input.is_empty() {
            return None;
        }
        Some(input.remove(0))
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
        self.size.0
    }
    fn rows(&self) -> usize {
        self.size.1
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

fn make_tui(terminal: &TestTerminal) -> TuiBase {
    TuiBase::new(
        Box::new(TestTerminal {
            writes: Arc::clone(&terminal.writes),
            input: Arc::clone(&terminal.input),
            size: terminal.size,
        }),
        TuiMode::Regular,
    )
}

#[test]
fn container_renders_children_and_propagates_lifecycle() {
    let mut container = Container::new();
    container.add_child(Box::new(Leaf::new()));
    container.add_child(Box::new(Leaf::new()));
    assert_eq!(to_vec(container.render(20)), vec!["leaf:20", "leaf:20"]);
    container.invalidate();
    container.handle_input("x");

    let mut removed = container.remove_child(0).expect("child");
    assert_eq!(to_vec(removed.render(1)), vec!["leaf:1"]);
    assert!(container.remove_child(9).is_none());
    container.clear();
    assert!(container.is_empty());
}

#[test]
fn focus_is_discoverable_through_the_component_trait() {
    let mut focusable = Leaf::focusable();
    let mut plain = Leaf::new();
    assert!(is_focusable(Some(&mut focusable)));
    assert!(!is_focusable(Some(&mut plain)));
    assert!(!is_focusable(None));
}

#[test]
fn size_values_parse_and_resolve_like_upstream() {
    assert_eq!(SizeValue::parse("12"), Some(SizeValue::Absolute(12)));
    assert_eq!(SizeValue::parse("50%"), Some(SizeValue::Percent(500)));
    assert_eq!(SizeValue::parse("12.5%"), Some(SizeValue::Percent(125)));
    assert_eq!(SizeValue::parse("-5%"), None);
    assert_eq!(SizeValue::parse("x"), None);

    assert_eq!(SizeValue::Percent(500).resolve_size(101), 50);
    assert_eq!(SizeValue::Percent(335).resolve_size(100), 33);
    assert_eq!(SizeValue::Absolute(7).resolve_size(100), 7);
}

#[test]
fn overlay_options_resolve_anchor_and_visibility() {
    let options = OverlayOptions::default();
    assert_eq!(options.resolved_anchor(), OverlayAnchor::Center);
    assert!(options.is_visible(80, 24));
    assert!(!options.non_capturing);

    let anchored = OverlayOptions {
        anchor: Some(OverlayAnchor::TopRight),
        non_capturing: true,
        margin: Some(OverlayMargin {
            top: 1,
            ..Default::default()
        }),
        visible: Some(Arc::new(|width, _| width >= 100)),
        ..Default::default()
    };
    assert!(!anchored.is_visible(80, 24));
    assert!(anchored.is_visible(120, 24));
    assert_eq!(OverlayAnchor::parse("top-right"), OverlayAnchor::TopRight);
    assert_eq!(anchored.resolved_anchor(), OverlayAnchor::TopRight);
}

#[test]
fn key_release_detection_matches_upstream() {
    assert!(is_key_release("\u{1b}[97;5:3u"));
    assert!(is_key_release("\u{1b}[1;2:3A"));
    assert!(!is_key_release("\u{1b}[97;5u"));
    // Bracketed paste content is never a release.
    assert!(!is_key_release("\u{1b}[200~90:62:3F:A5\u{1b}[201~"));
    assert!(!is_key_release("a"));
}

#[test]
fn tui_base_renders_roots_in_order_and_invalidates_them() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    tui.add_child(Box::new(Leaf::new()));
    tui.add_child(Box::new(Leaf::new()));
    assert_eq!(tui.child_count(), 2);
    assert_eq!(tui.render(40).len(), 2);
    tui.invalidate();
    assert_eq!(tui.root_ids().len(), 2);
    assert!(tui.remove_child(tui.root_ids()[0]).is_some());
    assert_eq!(tui.child_count(), 1);
    tui.clear();
    assert_eq!(tui.child_count(), 0);
}

#[test]
fn tui_base_focus_wires_focusable_components_and_dispatches_input() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    let log: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let focusable = tui.add_child(Box::new(Leaf {
        focus_log: Arc::clone(&log),
        ..Leaf::focusable()
    }));
    let plain = tui.add_child(Box::new(Leaf::new()));

    tui.set_focus(Some(focusable));
    assert_eq!(tui.focused(), Some(focusable));
    tui.handle_terminal_input("hello");
    tui.set_focus(Some(plain));
    tui.handle_terminal_input("ignored");

    // Key-release events are filtered unless the component opts in.
    tui.set_focus(Some(focusable));
    tui.handle_terminal_input("\u{1b}[97;5:3u");

    // Focus was granted to the component, then released when focus moved on.
    assert_eq!(log.lock().unwrap().clone(), vec![true, false, true]);
    assert!(tui.remove_child(focusable).is_some());
}

#[test]
fn input_listeners_can_rewrite_or_consume_input() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tui.add_input_listener(Box::new(move |data| {
        log.lock().unwrap().push(data.to_string());
        Some(InputListenerResult {
            consume: false,
            data: Some(format!("{data}!")),
        })
    }));

    let focused = tui.add_child(Box::new(Leaf::focusable()));
    tui.set_focus(Some(focused));
    tui.handle_terminal_input("a");

    assert_eq!(seen.lock().unwrap().clone(), vec!["a".to_string()]);

    // A consuming listener stops the chain.
    let consumed: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&consumed);
    tui.add_input_listener(Box::new(move |_data| {
        *counter.lock().unwrap() += 1;
        Some(InputListenerResult {
            consume: true,
            data: None,
        })
    }));
    tui.handle_terminal_input("b");
    assert_eq!(*consumed.lock().unwrap(), 1);
}

#[test]
fn overlays_stack_focus_and_remove() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    let base = tui.add_child(Box::new(Leaf::focusable()));
    tui.set_focus(Some(base));

    let overlay: ComponentId =
        tui.show_overlay(Box::new(Leaf::focusable()), OverlayOptions::default());
    assert!(tui.has_overlay());
    assert_eq!(tui.focused(), Some(overlay));
    assert!(tui.overlay_is_focused(overlay));
    assert_eq!(tui.overlay_ids(), vec![overlay]);

    // A non-capturing overlay does not steal focus.
    let passive = tui.show_overlay(
        Box::new(Leaf::focusable()),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    assert_eq!(tui.focused(), Some(overlay));

    tui.focus_overlay(passive);
    assert!(tui.overlay_is_focused(passive));

    tui.remove_overlay(passive);
    tui.hide_overlay();
    assert!(!tui.has_overlay());
    assert_eq!(tui.focused(), Some(base));
}

#[test]
fn render_requests_are_host_driven_and_throttled() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    let now = Instant::now();

    assert!(!tui.render_due(now));
    tui.request_render(false);
    assert!(!tui.render_due(now), "throttled frame is not due yet");
    assert!(tui.render_due(Instant::now() + Duration::from_millis(50)));
    assert!(tui.take_render_request(Instant::now() + Duration::from_millis(50)));
    assert!(!tui.render_due(Instant::now() + Duration::from_millis(60)));

    // A forced request is due immediately.
    tui.request_render(true);
    let soon = Instant::now() + Duration::from_millis(1);
    assert!(tui.render_due(soon));
    assert!(tui.take_render_request(soon));
    assert_eq!(tui.full_redraws(), 0);
    tui.note_full_redraw();
    assert_eq!(tui.full_redraws(), 1);
}

#[test]
fn start_stop_drive_the_terminal_and_colour_scheme_notifications() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);
    tui.start();
    assert!(!tui.is_stopped());
    tui.set_terminal_color_scheme_notifications(true);
    let written = terminal.written();
    assert!(written.contains("\u{1b}[?2031h"), "{written:?}");

    tui.stop(TuiStopOptions::default());
    assert!(tui.is_stopped());
    let written = terminal.written();
    assert!(written.contains("\u{1b}[?2031l"), "{written:?}");
}

#[test]
fn background_and_colour_scheme_queries_resolve_or_time_out() {
    let terminal = TestTerminal::new();
    let mut tui = make_tui(&terminal);

    // No reply: the query times out and reports nothing.
    assert_eq!(
        tui.query_terminal_background_color(Duration::from_millis(5)),
        None
    );
    assert!(terminal.written().contains("\u{1b}]11;?\u{7}"));

    // A terminal reply resolves the query.
    terminal.push_input("\u{1b}]11;rgb:ff/00/00\u{7}");
    assert_eq!(
        tui.query_terminal_background_color(Duration::from_millis(200)),
        Some(pillar_tui::terminal_colors::RgbColor { r: 255, g: 0, b: 0 })
    );

    // The colour-scheme query parses `CSI ? 997 ; 2 n`.
    terminal.push_input("\u{1b}[?997;2n");
    assert_eq!(
        tui.query_terminal_color_scheme(Duration::from_millis(200)),
        Some(pillar_tui::terminal_colors::TerminalColorScheme::Light)
    );

    // A cell-size reply is consumed by the TUI (it never reaches components).
    let mut probe = make_tui(&terminal);
    let focused = probe.add_child(Box::new(Leaf::focusable()));
    probe.set_focus(Some(focused));
    probe.handle_terminal_input("\u{1b}[6;20;10t");
    assert_eq!(
        pillar_tui::terminal_image::get_cell_dimensions().height_px,
        20
    );
    assert_eq!(
        pillar_tui::terminal_image::get_cell_dimensions().width_px,
        10
    );
}
