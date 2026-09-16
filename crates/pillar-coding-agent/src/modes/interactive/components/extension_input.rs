//! Port of packages/coding-agent/src/modes/interactive/components/
//! extension-input.ts (pi v0.84.3): a single-line text dialog for extensions
//! (and the port's own custom branch-summary instructions prompt).
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so
//!   [`ExtensionInputComponent::handle_key`] answers what the host must do
//!   instead of invoking `onSubmit` / `onCancel` callbacks.
//! - The `timeout` option (upstream `CountdownTimer`) is not wired yet.

use pillar_tui::components::Text;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::tui::{Component, Focusable};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::theme::theme;

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionInputOutcome {
    /// Handled inside the input.
    Consumed,
    /// Enter: the submitted value (upstream `onSubmit`).
    Submit(String),
    /// Escape / select-cancel (upstream `onCancel`).
    Cancel,
}

/// Single-line text dialog (upstream `ExtensionInputComponent`).
pub struct ExtensionInputComponent {
    title: String,
    input: Input,
    focused: bool,
}

impl ExtensionInputComponent {
    /// Upstream the constructor (the placeholder is unused there too).
    pub fn new(title: &str, _placeholder: Option<&str>) -> Self {
        Self {
            title: title.to_string(),
            input: Input::new(),
            focused: false,
        }
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ExtensionInputOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches("tui.select.confirm") || data == "\n" {
            return ExtensionInputOutcome::Submit(self.input.get_value().to_string());
        }
        if matches("tui.select.cancel") {
            return ExtensionInputOutcome::Cancel;
        }
        if !dispatch_input_keybinding(&mut self.input, data) {
            self.input.handle_input(data);
        }
        ExtensionInputOutcome::Consumed
    }
}

impl Component for ExtensionInputComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());
        lines.extend(Text::new(&theme_handle.fg("accent", &self.title), 1, 0).render(width));
        lines.push(String::new());
        lines.extend(self.input.render(width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                &format!(
                    "{}  {}",
                    key_hint("tui.select.confirm", "submit"),
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

impl Focusable for ExtensionInputComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_then_submit_reports_the_value() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut input = ExtensionInputComponent::new("Custom summarization instructions", None);
        for ch in "focus on tests".chars() {
            assert_eq!(
                input.handle_key(&ch.to_string()),
                ExtensionInputOutcome::Consumed
            );
        }
        assert_eq!(
            input.handle_key("\r"),
            ExtensionInputOutcome::Submit("focus on tests".to_string())
        );
    }

    #[test]
    fn cancel_is_reported() {
        let _guard = crate::modes::interactive::components::test_support::setup();
        let mut input = ExtensionInputComponent::new("Custom summarization instructions", None);
        assert_eq!(input.handle_key("\x1b"), ExtensionInputOutcome::Cancel);
    }
}
