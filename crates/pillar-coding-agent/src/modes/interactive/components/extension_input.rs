//! Port of packages/coding-agent/src/modes/interactive/components/
//! extension-input.ts (pi v0.84.3): a text dialog for extensions (and the
//! port's own custom branch-summary instructions prompt). The same component
//! covers `ctx.ui.editor` with a multi-line body (upstream mounts the full
//! editor there).
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so
//!   [`ExtensionInputComponent::handle_key`] answers what the host must do
//!   instead of invoking `onSubmit` / `onCancel` callbacks.
//! - The `timeout` option (upstream `CountdownTimer`) is not wired yet.

use pillar_tui::components::Text;
use pillar_tui::editor::{Editor, EditorInputEvent};
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

/// What the dialog edits with: one line, or the multi-line editor
/// (`ctx.ui.editor`).
enum Body {
    Single(Input),
    Multi(Box<Editor>),
}

/// Text dialog (upstream `ExtensionInputComponent`).
pub struct ExtensionInputComponent {
    title: String,
    body: Body,
    focused: bool,
}

impl ExtensionInputComponent {
    /// Upstream the constructor (the placeholder is unused there too).
    pub fn new(title: &str, _placeholder: Option<&str>) -> Self {
        Self {
            title: title.to_string(),
            body: Body::Single(Input::new()),
            focused: false,
        }
    }

    /// `ctx.ui.editor(title, initialText)`: the multi-line editor, seeded with
    /// `initial` (`tui.input.submit` submits, `tui.input.newLine` adds a line).
    pub fn new_multi_line(title: &str, initial: &str) -> Self {
        let mut editor = Editor::new();
        editor.set_text(initial);
        Self {
            title: title.to_string(),
            body: Body::Multi(Box::new(editor)),
            focused: false,
        }
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ExtensionInputOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches("tui.select.cancel") {
            return ExtensionInputOutcome::Cancel;
        }
        match &mut self.body {
            Body::Single(input) => {
                if matches("tui.select.confirm") || data == "\n" {
                    return ExtensionInputOutcome::Submit(input.get_value().to_string());
                }
                if !dispatch_input_keybinding(input, data) {
                    input.handle_input(data);
                }
            }
            Body::Multi(editor) => {
                // The editor owns submit / newline / cursor keys: Enter submits
                // (the same binding the prompt editor uses).
                editor.handle_input(data);
                let submitted =
                    editor
                        .take_input_events()
                        .into_iter()
                        .find_map(|event| match event {
                            EditorInputEvent::Submitted(text) => Some(text),
                            EditorInputEvent::Changed => None,
                        });
                if let Some(text) = submitted {
                    return ExtensionInputOutcome::Submit(text);
                }
            }
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
        match &mut self.body {
            Body::Single(input) => lines.extend(input.render(width)),
            Body::Multi(editor) => lines.extend(editor.render(width)),
        }
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
        match &mut self.body {
            Body::Single(input) => input.focused = focused,
            Body::Multi(editor) => editor.set_focused(focused),
        }
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
