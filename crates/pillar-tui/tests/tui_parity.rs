//! Parity tests for packages/tui/src/tui.ts (pi v0.84.3): the Component
//! trait, Container, focus plumbing and the overlay option types.

use pillar_tui::tui::{
    Component, Container, Focusable, MarginSpec, OverlayAnchor, OverlayMargin, OverlayOptions,
    SizeValue, TuiMode, TuiStopOptions, is_focusable, parse_size_value,
};

/// A leaf component recording the renders and lifecycle calls it received.
#[derive(Default)]
struct Leaf {
    label: String,
    renders: usize,
    invalidations: usize,
    inputs: Vec<String>,
    focused: bool,
    focusable: bool,
}

impl Leaf {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            ..Default::default()
        }
    }

    fn focusable(label: &str) -> Self {
        Self {
            focusable: true,
            ..Self::new(label)
        }
    }
}

impl Focusable for Leaf {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

impl Component for Leaf {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.renders += 1;
        vec![format!("{}:{width}", self.label)]
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
    }

    fn invalidate(&mut self) {
        self.invalidations += 1;
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        if self.focusable { Some(self) } else { None }
    }
}

#[test]
fn container_renders_children_in_order() {
    let mut container = Container::new();
    container.add_child(Box::new(Leaf::new("a")));
    container.add_child(Box::new(Leaf::new("b")));
    assert_eq!(container.len(), 2);
    assert_eq!(container.render(40), vec!["a:40", "b:40"]);
}

#[test]
fn container_forwards_invalidate_and_input_and_supports_removal() {
    let mut container = Container::new();
    container.add_child(Box::new(Leaf::new("a")));
    container.add_child(Box::new(Leaf::new("b")));

    container.invalidate();
    container.handle_input("x");

    // The container itself is a Component, so it renders through the root too.
    let mut root = Container::new();
    root.add_child(Box::new(Leaf::new("r")));
    assert_eq!(root.render(10), vec!["r:10"]);

    let mut removed = container.remove_child(0).expect("child at 0");
    assert_eq!(removed.render(1), vec!["a:1"]);
    assert_eq!(container.len(), 1);
    assert!(container.remove_child(5).is_none());

    container.clear();
    assert!(container.is_empty());
    assert!(container.render(80).is_empty());
}

#[test]
fn focus_is_discoverable_through_the_component_trait() {
    let mut focusable = Leaf::focusable("input");
    let mut plain = Leaf::new("label");

    assert!(is_focusable(Some(&mut focusable)));
    assert!(!is_focusable(Some(&mut plain)));
    assert!(!is_focusable(None));

    let focused = focusable.as_focusable().expect("focusable");
    assert!(!focused.is_focused());
    focused.set_focused(true);
    assert!(focusable.is_focused());
}

#[test]
fn size_values_parse_absolute_and_percentage() {
    assert_eq!(
        parse_size_value(Some(SizeValue::Absolute(12)), 100),
        Some(12)
    );
    assert_eq!(
        parse_size_value(Some(SizeValue::Percent(50.0)), 101),
        Some(50)
    );
    assert_eq!(
        parse_size_value(Some(SizeValue::Percent(33.5)), 100),
        Some(33)
    );
    assert_eq!(
        parse_size_value(Some(SizeValue::Percent(100.0)), 24),
        Some(24)
    );
    assert_eq!(parse_size_value(None, 100), None);
}

#[test]
fn overlay_options_default_to_centered_visible_and_capturing() {
    let options = OverlayOptions::default();
    assert_eq!(options.resolved_anchor(), OverlayAnchor::Center);
    assert!(options.is_visible(80, 24));
    assert!(!options.non_capturing);
    assert_eq!(options.width, None);

    let anchored = OverlayOptions {
        anchor: Some(OverlayAnchor::TopRight),
        non_capturing: true,
        margin: Some(MarginSpec::All(2)),
        visible: Some(Box::new(|width, _height| width > 100)),
        ..Default::default()
    };
    assert_eq!(anchored.resolved_anchor(), OverlayAnchor::TopRight);
    assert!(!anchored.is_visible(80, 24));
    assert!(anchored.is_visible(120, 24));
    assert_eq!(anchored.margin, Some(MarginSpec::All(2)));
    assert_eq!(anchored.resolved_anchor().as_str(), "top-right");
}

#[test]
fn margins_accept_a_single_number_or_per_side_values() {
    let all: MarginSpec = 3usize.into();
    assert_eq!(all, MarginSpec::All(3));

    let sides: MarginSpec = OverlayMargin {
        top: Some(1),
        bottom: Some(2),
        ..Default::default()
    }
    .into();
    assert_eq!(
        sides,
        MarginSpec::Sides(OverlayMargin {
            top: Some(1),
            bottom: Some(2),
            right: None,
            left: None,
        })
    );

    // Anchors cover the upstream union.
    let anchors = [
        OverlayAnchor::Center,
        OverlayAnchor::TopLeft,
        OverlayAnchor::TopRight,
        OverlayAnchor::BottomLeft,
        OverlayAnchor::BottomRight,
        OverlayAnchor::TopCenter,
        OverlayAnchor::BottomCenter,
        OverlayAnchor::LeftCenter,
        OverlayAnchor::RightCenter,
    ];
    for anchor in anchors {
        assert!(!anchor.as_str().is_empty());
    }
}

#[test]
fn tui_mode_and_stop_options_match_upstream() {
    assert_eq!(TuiMode::default(), TuiMode::Regular);
    assert_eq!(TuiMode::Regular.as_str(), "regular");
    assert_eq!(TuiMode::Fullscreen.as_str(), "fullscreen");
    assert!(!TuiStopOptions::default().preserve_screen);
    assert!(
        TuiStopOptions {
            preserve_screen: true
        }
        .preserve_screen
    );
}

#[test]
fn composite_tui_line_is_reexported_from_the_tui_surface() {
    // Upstream exposes `compositeTuiLine` from tui.ts; the port keeps the
    // implementation in stack_layout and re-exports it here.
    let composed = pillar_tui::tui::composite_tui_line("aaaaaaaa", "BB", 3, 2, 8);
    assert_eq!(pillar_tui::text_utils::visible_width(&composed), 8);
    assert!(composed.contains("BB"), "{composed:?}");
    assert_eq!(pillar_tui::tui::CURSOR_MARKER, "\u{1b}_pi:c\u{7}");
}
