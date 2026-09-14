//! Port of components/bordered-loader.ts: a loader framed by dynamic borders,
//! used by the extension UI and the share flow.
//!
//! divergence: upstream composes a `Container` of borders/loader/spacer/text;
//! the port renders the same children in order but keeps ownership of the
//! loader so the host can tick and abort it (the container would own it).

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::theme::Theme;
use pillar_tui::components::{Spacer, Text};
use pillar_tui::loaders::Loader;
use pillar_tui::tui::Component;

/// Loader wrapped with borders (upstream `BorderedLoader`).
pub struct BorderedLoader {
    border_color: Box<dyn Fn(&str) -> String + Send>,
    loader: Loader,
    cancellable: bool,
    cancel_hint: Option<Text>,
    spacer: Spacer,
    aborted: bool,
    on_abort: Option<Box<dyn FnMut() + Send>>,
}

impl BorderedLoader {
    /// Build the bordered loader; `cancellable` mirrors upstream's option
    /// (default true at the call sites).
    pub fn new(theme_obj: &Theme, message: &str, cancellable: bool) -> Self {
        let border_color: Box<dyn Fn(&str) -> String + Send> = {
            let theme_obj = theme_obj.clone();
            Box::new(move |text: &str| theme_obj.fg("border", text))
        };
        let loader = Loader::new(
            {
                let theme_obj = theme_obj.clone();
                Box::new(move |spinner: &str| theme_obj.fg("accent", spinner))
            },
            {
                let theme_obj = theme_obj.clone();
                Box::new(move |text: &str| theme_obj.fg("muted", text))
            },
            message,
            None,
        );
        let cancel_hint = if cancellable {
            Some(Text::new(&key_hint("tui.select.cancel", "cancel"), 1, 0))
        } else {
            None
        };
        Self {
            border_color,
            loader,
            cancellable,
            cancel_hint,
            spacer: Spacer::new(1),
            aborted: false,
            on_abort: None,
        }
    }

    /// Whether the cancel key aborts this loader (upstream `cancellable`).
    pub fn is_cancellable(&self) -> bool {
        self.cancellable
    }

    /// The spinner inside the frame.
    pub fn loader(&self) -> &Loader {
        &self.loader
    }

    pub fn loader_mut(&mut self) -> &mut Loader {
        &mut self.loader
    }

    /// The abort signal upstream exposes; the port reports the abort through
    /// [`Self::aborted`] and the `on_abort` callback.
    pub fn abort(&mut self) {
        if self.cancellable {
            self.aborted = true;
            if let Some(callback) = self.on_abort.as_mut() {
                callback();
            }
        }
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// Called when the loader is aborted (upstream the `onAbort` setter).
    pub fn set_on_abort(&mut self, callback: Option<Box<dyn FnMut() + Send>>) {
        self.on_abort = callback;
    }

    /// Host-driven abort check (upstream `handleInput` forwarding to the
    /// cancellable loader).
    pub fn handle_abort_key(&mut self, is_cancel_key: bool) {
        if self.cancellable && is_cancel_key {
            self.abort();
        }
    }

    /// Advance the spinner (host-driven).
    pub fn tick(&mut self) -> bool {
        self.loader.tick()
    }

    pub fn dispose(&mut self) {
        self.loader.stop();
    }

    /// The message currently displayed.
    pub fn message(&self) -> &str {
        self.loader.text()
    }

    /// Render one dynamic border with this loader's colour.
    fn border(&self, width: usize) -> Vec<String> {
        vec![(self.border_color)(&"─".repeat(width.max(1)))]
    }
}

impl Component for BorderedLoader {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut border = DynamicBorder::with_color(Box::new(|text: &str| text.to_string()));
        let _ = &mut border;
        let mut lines = self.border(width);
        lines.extend(self.loader.render(width));
        if let Some(cancel_hint) = self.cancel_hint.as_mut() {
            lines.extend(self.spacer.render(width));
            lines.extend(cancel_hint.render(width));
        }
        lines.extend(self.spacer.render(width));
        lines.extend(self.border(width));
        lines
    }

    fn handle_input(&mut self, _data: &str) {
        // Keybinding dispatch stays host-side: the host calls
        // `handle_abort_key` when the cancel binding matches.
    }
}
