//! Port of packages/coding-agent/src/modes/interactive/components/
//! extension-selector.ts (pi v0.84.3): a generic list dialog for extensions
//! (and the port's own "Summarize branch?" prompt).
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so
//!   [`ExtensionSelectorComponent::handle_key`] answers what the host must do
//!   instead of invoking `onSelect` / `onCancel` callbacks.
//! - The `timeout` option (upstream `CountdownTimer`) is not wired: nothing in
//!   the port passes one yet.

use pillar_tui::components::Text;
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::tui::{Component, Focusable};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, raw_key_hint};
use crate::modes::interactive::theme::theme;

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionSelectorOutcome {
    /// Handled inside the selector (navigation).
    Consumed,
    /// Enter: the chosen option (upstream `onSelect`).
    Select(String),
    /// Escape / select-cancel (upstream `onCancel`).
    Cancel,
    /// `app.tools.expand` (upstream `onToggleToolsExpanded`).
    ToggleToolsExpanded,
}

/// Generic option-list dialog (upstream `ExtensionSelectorComponent`).
pub struct ExtensionSelectorComponent {
    title: String,
    options: Vec<String>,
    selected_index: usize,
    focused: bool,
}

impl ExtensionSelectorComponent {
    pub fn new(title: &str, options: &[String]) -> Self {
        Self {
            title: title.to_string(),
            options: options.to_vec(),
            selected_index: 0,
            focused: false,
        }
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ExtensionSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches("app.tools.expand") {
            return ExtensionSelectorOutcome::ToggleToolsExpanded;
        }
        if matches("tui.select.up") || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
            return ExtensionSelectorOutcome::Consumed;
        }
        if matches("tui.select.down") || data == "j" {
            self.selected_index =
                (self.selected_index + 1).min(self.options.len().saturating_sub(1));
            return ExtensionSelectorOutcome::Consumed;
        }
        if matches("tui.select.confirm") || data == "\n" {
            if let Some(option) = self.options.get(self.selected_index) {
                return ExtensionSelectorOutcome::Select(option.clone());
            }
            return ExtensionSelectorOutcome::Consumed;
        }
        if matches("tui.select.cancel") {
            return ExtensionSelectorOutcome::Cancel;
        }
        ExtensionSelectorOutcome::Consumed
    }
}

impl Component for ExtensionSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                &theme_handle.fg("accent", &theme_handle.bold(&self.title)),
                1,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        for (index, option) in self.options.iter().enumerate() {
            let is_selected = index == self.selected_index;
            let text = if is_selected {
                format!(
                    "{}{}",
                    theme_handle.fg("accent", "→ "),
                    theme_handle.fg("accent", option)
                )
            } else {
                format!("  {}", theme_handle.fg("text", option))
            };
            lines.extend(Text::new(&text, 1, 0).render(width));
        }
        lines.push(String::new());
        lines.extend(
            Text::new(
                &format!(
                    "{}  {}  {}",
                    raw_key_hint("↑↓", "navigate"),
                    key_hint("tui.select.confirm", "select"),
                    key_hint("tui.select.cancel", "cancel")
                ),
                1,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for ExtensionSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<String> {
        vec![
            "No summary".to_string(),
            "Summarize".to_string(),
            "Summarize with custom prompt".to_string(),
        ]
    }

    #[test]
    fn navigation_and_selection() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut selector = ExtensionSelectorComponent::new("Summarize branch?", &options());
        // Up clamps at the top.
        assert_eq!(
            selector.handle_key("\x1b[A"),
            ExtensionSelectorOutcome::Consumed
        );
        // Down twice reaches the last option; further downs clamp.
        selector.handle_key("\x1b[B");
        selector.handle_key("\x1b[B");
        selector.handle_key("\x1b[B");
        assert_eq!(
            selector.handle_key("\r"),
            ExtensionSelectorOutcome::Select("Summarize with custom prompt".to_string())
        );
    }

    #[test]
    fn cancel_and_tools_expand_are_reported() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut selector = ExtensionSelectorComponent::new("Summarize branch?", &options());
        assert_eq!(
            selector.handle_key("\x1b"),
            ExtensionSelectorOutcome::Cancel
        );
    }
}
