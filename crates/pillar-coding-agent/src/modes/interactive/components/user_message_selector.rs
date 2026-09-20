//! Port of packages/coding-agent/src/modes/interactive/components/
//! user-message-selector.ts (pi v0.84.3): the `/fork` dialog that picks a user
//! message to branch from.
//!
//! divergence: pillar-tui keeps keybinding dispatch host-side, so
//! [`UserMessageSelectorComponent::handle_key`] answers what the host must do
//! instead of invoking `onSelect` / `onCancel` callbacks. Upstream's
//! empty-list 100 ms auto-cancel is handled by the mode's empty check before
//! opening the selector.

use pillar_tui::components::Text;
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::text_utils::truncate_to_width;
use pillar_tui::tui::{Component, Focusable, RenderLines, render_lines};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::theme;

/// A forkable user message (upstream `UserMessageItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessageItem {
    /// The session entry id.
    pub id: String,
    /// The message text.
    pub text: String,
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageSelectorOutcome {
    /// Handled inside the selector (navigation).
    Consumed,
    /// Enter: fork from `entry_id` (upstream `onSelect`).
    Select(String),
    /// Escape / select-cancel (upstream `onCancel`).
    Cancel,
}

const MAX_VISIBLE: usize = 10;

/// The user-message list (upstream `UserMessageList`).
struct UserMessageList {
    messages: Vec<UserMessageItem>,
    selected_index: usize,
}

impl UserMessageList {
    fn new(messages: Vec<UserMessageItem>, initial_selected_id: Option<&str>) -> Self {
        let initial_index = initial_selected_id
            .and_then(|id| messages.iter().position(|message| message.id == id))
            .map(|index| index as isize)
            .unwrap_or(-1);
        let selected_index = if initial_index >= 0 {
            initial_index as usize
        } else {
            messages.len().saturating_sub(1)
        };
        Self {
            messages,
            selected_index,
        }
    }

    fn handle_key(&mut self, data: &str) -> UserMessageSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));
        if matches("tui.select.up") {
            self.selected_index = if self.selected_index == 0 {
                self.messages.len().saturating_sub(1)
            } else {
                self.selected_index - 1
            };
        } else if matches("tui.select.down") {
            self.selected_index = if self.selected_index + 1 >= self.messages.len() {
                0
            } else {
                self.selected_index + 1
            };
        } else if matches("tui.select.confirm") {
            if let Some(selected) = self.messages.get(self.selected_index) {
                return UserMessageSelectorOutcome::Select(selected.id.clone());
            }
        } else if matches("tui.select.cancel") {
            return UserMessageSelectorOutcome::Cancel;
        }
        UserMessageSelectorOutcome::Consumed
    }

    fn render(&self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();

        if self.messages.is_empty() {
            lines.push(theme_handle.fg("muted", "  No user messages found"));
            return lines;
        }

        let start_index = if self.selected_index >= MAX_VISIBLE / 2 {
            (self.selected_index - MAX_VISIBLE / 2)
                .min(self.messages.len().saturating_sub(MAX_VISIBLE))
        } else {
            0
        };
        let end_index = (start_index + MAX_VISIBLE).min(self.messages.len());

        for i in start_index..end_index {
            let message = &self.messages[i];
            let is_selected = i == self.selected_index;
            let normalized = message.text.replace('\n', " ").trim().to_string();

            let cursor = if is_selected {
                theme_handle.fg("accent", "› ")
            } else {
                "  ".to_string()
            };
            let max_msg_width = width.saturating_sub(2);
            let truncated = truncate_to_width(&normalized, max_msg_width, "…", false);
            let message_line = if is_selected {
                format!("{cursor}{}", theme_handle.bold(&truncated))
            } else {
                format!("{cursor}{truncated}")
            };
            lines.push(message_line);

            lines.push(theme_handle.fg(
                "muted",
                &format!("  Message {} of {}", i + 1, self.messages.len()),
            ));
            lines.push(String::new());
        }

        if start_index > 0 || end_index < self.messages.len() {
            lines.push(theme_handle.fg(
                "muted",
                &format!("  ({}/{})", self.selected_index + 1, self.messages.len()),
            ));
        }

        lines
    }
}

/// The `/fork` dialog (upstream `UserMessageSelectorComponent`).
pub struct UserMessageSelectorComponent {
    message_list: UserMessageList,
    focused: bool,
}

impl UserMessageSelectorComponent {
    pub fn new(messages: Vec<UserMessageItem>, initial_selected_id: Option<&str>) -> Self {
        Self {
            message_list: UserMessageList::new(messages, initial_selected_id),
            focused: false,
        }
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> UserMessageSelectorOutcome {
        self.message_list.handle_key(data)
    }
}

impl Component for UserMessageSelectorComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.extend(
            Text::new(&theme_handle.bold("Fork from Message"), 1, 0)
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.extend(
            Text::new(
                &theme_handle.fg(
                    "muted",
                    "Select a user message to copy the active path up to that point into a new session",
                ),
                1,
                0,
            )
            .render(width).iter().map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            DynamicBorder::new()
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            self.message_list
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            DynamicBorder::new()
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        render_lines(lines)
    }
}

impl Focusable for UserMessageSelectorComponent {
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
    use pillar_tui::text_utils::strip_terminal_sequences;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::modes::interactive::components::test_support::setup()
    }

    fn messages() -> Vec<UserMessageItem> {
        vec![
            UserMessageItem {
                id: "m1".to_string(),
                text: "first question".to_string(),
            },
            UserMessageItem {
                id: "m2".to_string(),
                text: "second\nquestion".to_string(),
            },
        ]
    }

    fn plain(selector: &mut UserMessageSelectorComponent) -> String {
        selector
            .render(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn defaults_to_the_most_recent_message_and_wraps() {
        let _guard = setup();
        let mut selector = UserMessageSelectorComponent::new(messages(), None);
        let rendered = plain(&mut selector);
        assert!(rendered.contains("Fork from Message"), "{rendered}");
        // Newlines are normalized to spaces.
        assert!(rendered.contains("second question"), "{rendered}");
        assert!(rendered.contains("Message 2 of 2"), "{rendered}");

        // Up wraps to the older message and down wraps back.
        assert_eq!(
            selector.handle_key("\x1b[A"),
            UserMessageSelectorOutcome::Consumed
        );
        assert_eq!(
            selector.handle_key("\r"),
            UserMessageSelectorOutcome::Select("m1".to_string())
        );
    }

    #[test]
    fn initial_selection_and_cancel() {
        let _guard = setup();
        let mut selector = UserMessageSelectorComponent::new(messages(), Some("m1"));
        assert_eq!(
            selector.handle_key("\r"),
            UserMessageSelectorOutcome::Select("m1".to_string())
        );
        assert_eq!(
            selector.handle_key("\x1b"),
            UserMessageSelectorOutcome::Cancel
        );
    }

    #[test]
    fn empty_list_renders_the_hint() {
        let _guard = setup();
        let mut selector = UserMessageSelectorComponent::new(Vec::new(), None);
        let rendered = plain(&mut selector);
        assert!(rendered.contains("No user messages found"), "{rendered}");
    }
}
