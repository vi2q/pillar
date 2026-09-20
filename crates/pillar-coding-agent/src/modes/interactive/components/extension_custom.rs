//! Upstream the `ctx.ui.custom` component: the factory's `Component` mounted
//! in the editor slot.
//!
//! divergence: upstream renders the extension's component on the main thread,
//! so the factory's `render` / `handleInput` run inline. The port runs the
//! extension's render loop on the extension's own thread (it holds the Luau
//! runtime lock while the UI is open), so the pump only shows the last frame
//! the loop painted and forwards input and width changes back to it.
//! `tui.requestRender()` is therefore unnecessary (every paint repaints) and
//! is a no-op on the `tui` table the factory receives.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pillar_tui::tui::{Component, RenderLines, render_lines};

use crate::core::extensions_types::{
    ExtensionCustomEvent, ExtensionCustomEvents, ExtensionCustomSurface,
};

/// The editor-slot component showing one `ctx.ui.custom` frame.
pub struct ExtensionCustomComponent {
    lines: Arc<Mutex<Vec<String>>>,
    revision: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    events: ExtensionCustomEvents,
    last_width: Option<usize>,
    last_revision: u64,
}

impl ExtensionCustomComponent {
    pub fn new(surface: ExtensionCustomSurface) -> Self {
        Self {
            lines: surface.lines,
            revision: surface.revision,
            closed: surface.closed,
            events: surface.events,
            last_width: None,
            last_revision: 0,
        }
    }

    /// Whether the render loop painted or gave up since the last poll.
    pub fn poll(&mut self) -> bool {
        let revision = self.revision.load(Ordering::SeqCst);
        if revision == self.last_revision {
            return false;
        }
        self.last_revision = revision;
        true
    }

    /// Whether the render loop finished; the selector must then close.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Component for ExtensionCustomComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        if self.last_width != Some(width) {
            self.last_width = Some(width);
            self.events.send(ExtensionCustomEvent::Resize(width));
        }
        self.last_revision = self.revision.load(Ordering::SeqCst);
        let lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        render_lines(lines)
    }

    fn handle_input(&mut self, data: &str) {
        self.events
            .send(ExtensionCustomEvent::Input(data.to_string()));
    }
}
