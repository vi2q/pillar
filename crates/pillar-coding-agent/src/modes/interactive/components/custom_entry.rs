//! Port of components/custom-entry.ts: render an extension's custom session
//! entry through its registered renderer.

use crate::core::extensions_types::{EntryRenderOptions, EntryRenderer};
use crate::core::session_entries::CustomEntry;
use crate::modes::interactive::theme::theme;
use pillar_tui::components::{BoxComponent, Spacer, Text};
use pillar_tui::tui::{Component, Container};

/// A custom entry plus its renderer (upstream `CustomEntryComponent`).
///
/// The host owns transcript spacing; the renderer output is only the entry's
/// content (the port prepends upstream's one-line spacer).
pub struct CustomEntryComponent {
    entry: CustomEntry,
    renderer: EntryRenderer,
    container: Container,
    has_content: bool,
    expanded: bool,
}

impl CustomEntryComponent {
    pub fn new(entry: CustomEntry, renderer: EntryRenderer) -> Self {
        let mut component = Self {
            entry,
            renderer,
            container: Container::new(),
            has_content: false,
            expanded: false,
        };
        component.rebuild();
        component
    }

    /// Whether the renderer produced a component (upstream `hasContent`).
    pub fn has_content(&self) -> bool {
        self.has_content
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Toggle the expanded state, rebuilding on change (upstream
    /// `setExpanded`).
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    /// Re-run the renderer (upstream `rebuild`), showing upstream's error box
    /// when the renderer reports nothing usable.
    pub fn rebuild(&mut self) {
        self.container.clear();
        self.has_content = false;

        let options = EntryRenderOptions {
            expanded: self.expanded,
        };
        let rendered = (self.renderer)(&self.entry, &options, &theme());

        let Some(component) = rendered else {
            return;
        };
        self.has_content = true;
        self.container.add_child(Box::new(Spacer::new(1)));
        self.container.add_child(component);
    }

    /// Render the renderer failure notice upstream shows when the renderer
    /// throws (the port cannot catch panics, so hosts call this with the
    /// message).
    pub fn error_component(&self, message: &str) -> BoxComponent {
        let theme = theme();
        let mut box_component = BoxComponent::new(1, 1);
        let bg_theme = theme.clone();
        box_component.set_bg_fn(Some(Box::new(move |text: &str| {
            bg_theme.bg("customMessageBg", text)
        })));
        box_component.add_child(Box::new(Text::new(
            &theme.fg(
                "error",
                &format!("[{}] renderer failed: {message}", self.entry.custom_type),
            ),
            0,
            0,
        )));
        box_component
    }
}

impl Component for CustomEntryComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.rebuild();
    }
}
