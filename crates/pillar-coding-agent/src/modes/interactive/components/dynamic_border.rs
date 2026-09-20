//! Port of components/dynamic-border.ts: a horizontal rule that adjusts to the
//! viewport width.

use crate::modes::interactive::theme::theme;
use pillar_tui::tui::{Component, RenderLines, render_lines};

/// A full-width `─` rule (upstream `DynamicBorder`).
///
/// upstream note: extensions loaded through a separate module cache may see an
/// undefined global theme, so callers should pass an explicit colour function
/// when exporting components to extensions.
pub struct DynamicBorder {
    color: Box<dyn Fn(&str) -> String + Send>,
}

impl DynamicBorder {
    /// A border using the theme's `border` colour (upstream's default).
    pub fn new() -> Self {
        Self::with_color(Box::new(|text| theme().fg("border", text)))
    }

    /// A border with an explicit colour function.
    pub fn with_color(color: Box<dyn Fn(&str) -> String + Send>) -> Self {
        Self { color }
    }
}

impl Default for DynamicBorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for DynamicBorder {
    fn render(&mut self, width: usize) -> RenderLines {
        render_lines(vec![(self.color)(&"─".repeat(width.max(1)))])
    }
}

impl DynamicBorder {
    /// No cached state to invalidate (upstream `invalidate`).
    pub fn invalidate(&mut self) {}
}
