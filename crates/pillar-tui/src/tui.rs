//! Port of packages/tui/src/tui.ts (pi v0.84.3), first slice: the
//! [`Component`] trait, [`Container`], focus plumbing, and the overlay option
//! types. The `TUI` trait / `TuiBase` (render scheduling, input dispatch,
//! terminal queries) land next.
//!
//! Already ported elsewhere and re-exported here for parity with upstream's
//! module surface: `compositeTuiLine` ([`composite_tui_line`], in
//! [`crate::stack_layout`]) and `CURSOR_MARKER` ([`CURSOR_MARKER`], in
//! [`crate::input`]).
//!
//! divergences:
//! - upstream components extend a JS class with an optional `handleInput` and
//!   a structural `isFocusable` check; the port uses object-safe traits with
//!   [`Component::as_focusable`] as the type guard.
//! - `Container.removeChild(component)` matches by identity; the port removes
//!   by index.

pub use crate::input::CURSOR_MARKER;
pub use crate::stack_layout::composite_tui_line;

/// A renderable UI component (upstream `Component`).
pub trait Component: Send {
    /// Render the component to lines for the given viewport width.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Handle keyboard input while focused (upstream `handleInput`).
    fn handle_input(&mut self, _data: &str) {}

    /// Whether key-release events should be delivered (upstream
    /// `wantsKeyRelease`).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Drop cached rendering state (upstream `invalidate`).
    fn invalidate(&mut self) {}

    /// The focus interface when the component can take focus (upstream the
    /// `isFocusable` type guard).
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        None
    }
}

/// A component that can receive focus and show the hardware cursor (upstream
/// `Focusable`).
pub trait Focusable {
    /// Set by the TUI when focus changes.
    fn set_focused(&mut self, focused: bool);
    /// Whether this component currently has focus (upstream reads the public
    /// `focused` field).
    fn is_focused(&self) -> bool;
}

/// Whether a component can take focus (upstream `isFocusable`).
pub fn is_focusable(component: Option<&mut dyn Component>) -> bool {
    component.is_some_and(|component| component.as_focusable().is_some())
}

/// A container of components (upstream `Container`).
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    /// Remove the child at `index` (upstream `removeChild` by identity).
    pub fn remove_child(&mut self, index: usize) -> Option<Box<dyn Component>> {
        if index >= self.children.len() {
            return None;
        }
        Some(self.children.remove(index))
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn children(&self) -> &[Box<dyn Component>] {
        &self.children
    }
}

impl Component for Container {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &mut self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        for child in &mut self.children {
            child.handle_input(data);
        }
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

/// A value that can be absolute or a percentage (upstream `SizeValue`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Absolute(usize),
    /// Percentage of the reference size, e.g. `50` for `"50%"`.
    Percent(f64),
}

/// Parse a `SizeValue` against a reference size (upstream `parseSizeValue`).
pub fn parse_size_value(value: Option<SizeValue>, reference_size: usize) -> Option<usize> {
    match value? {
        SizeValue::Absolute(value) => Some(value),
        SizeValue::Percent(percent) => {
            Some(((reference_size as f64 * percent) / 100.0).floor() as usize)
        }
    }
}

/// Where an overlay is anchored (upstream `OverlayAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

impl OverlayAnchor {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayAnchor::Center => "center",
            OverlayAnchor::TopLeft => "top-left",
            OverlayAnchor::TopRight => "top-right",
            OverlayAnchor::BottomLeft => "bottom-left",
            OverlayAnchor::BottomRight => "bottom-right",
            OverlayAnchor::TopCenter => "top-center",
            OverlayAnchor::BottomCenter => "bottom-center",
            OverlayAnchor::LeftCenter => "left-center",
            OverlayAnchor::RightCenter => "right-center",
        }
    }
}

/// Per-side overlay margin (upstream `OverlayMargin`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: Option<usize>,
    pub right: Option<usize>,
    pub bottom: Option<usize>,
    pub left: Option<usize>,
}

impl From<usize> for OverlayMargin {
    fn from(value: usize) -> Self {
        Self {
            top: Some(value),
            right: Some(value),
            bottom: Some(value),
            left: Some(value),
        }
    }
}

/// Eager `Cow`-style margin spec: upstream accepts `number | OverlayMargin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginSpec {
    All(usize),
    Sides(OverlayMargin),
}

impl From<usize> for MarginSpec {
    fn from(value: usize) -> Self {
        MarginSpec::All(value)
    }
}

impl From<OverlayMargin> for MarginSpec {
    fn from(value: OverlayMargin) -> Self {
        MarginSpec::Sides(value)
    }
}

/// Overlay visibility predicate (upstream `options.visible`).
pub type OverlayVisibility = Box<dyn Fn(usize, usize) -> bool + Send + Sync>;

/// Positioning and sizing for an overlay (upstream `OverlayOptions`).
#[derive(Default)]
pub struct OverlayOptions {
    /// Width in columns or a percentage of the terminal width.
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    /// Maximum height in rows or a percentage of the terminal height.
    pub max_height: Option<SizeValue>,
    pub anchor: Option<OverlayAnchor>,
    pub offset_x: Option<i64>,
    pub offset_y: Option<i64>,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: Option<MarginSpec>,
    /// Only render when this returns true for the terminal size.
    pub visible: Option<OverlayVisibility>,
    /// Do not capture keyboard focus when shown.
    pub non_capturing: bool,
}

impl OverlayOptions {
    /// The effective anchor (upstream defaults to `center`).
    pub fn resolved_anchor(&self) -> OverlayAnchor {
        self.anchor.unwrap_or_default()
    }

    /// Whether the overlay is visible at this terminal size.
    pub fn is_visible(&self, term_width: usize, term_height: usize) -> bool {
        match &self.visible {
            Some(visible) => visible(term_width, term_height),
            None => true,
        }
    }
}

/// Options for releasing overlay focus (upstream `OverlayUnfocusOptions`).
#[derive(Default)]
pub struct OverlayUnfocusOptions {
    /// Explicit focus target after releasing this overlay (`None` = previous
    /// target).
    pub has_target: bool,
}

/// How the TUI drives the terminal (upstream `TuiMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TuiMode {
    #[default]
    Regular,
    Fullscreen,
}

impl TuiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TuiMode::Regular => "regular",
            TuiMode::Fullscreen => "fullscreen",
        }
    }
}

/// Options for stopping the TUI (upstream `TuiStopOptions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over the same
    /// terminal.
    pub preserve_screen: bool,
}
