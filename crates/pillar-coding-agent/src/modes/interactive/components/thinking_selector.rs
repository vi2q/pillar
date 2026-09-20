//! Port of packages/coding-agent/src/modes/interactive/components/thinking-selector.ts
//! (pi v0.84.3): searchable thinking-level picker shown in the editor slot.
//!
//! divergence: pillar-tui's `SelectList` / `Input` keep keybinding dispatch
//! host-side, so [`ThinkingSelectorComponent::handle_key`] resolves the keys
//! and answers what the host must do (confirm / cancel / set-as-default)
//! instead of invoking `onSelect` / `onCancel` / `onSelectAsDefault`
//! callbacks. Rendering composes the same lines as upstream's container.

use pillar_tui::components::Text;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::keys::matches_key;
use pillar_tui::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pillar_tui::tui::{Component, Focusable, RenderLines, render_lines};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_display_text;
use crate::modes::interactive::theme::{get_select_list_theme, theme};

/// Upstream `THINKING_SELECT_LIST_LAYOUT`.
fn thinking_select_list_layout() -> SelectListLayoutOptions {
    SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
        ..Default::default()
    }
}

/// Upstream `LEVEL_DESCRIPTIONS`.
fn level_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        "max" => "Maximum reasoning",
        _ => "",
    }
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingSelectorOutcome {
    /// Handled inside the selector (navigation or search input).
    Consumed,
    /// Enter: select `level` for this session (upstream `onSelect`).
    Select(String),
    /// Ctrl+S: select `level` and persist it as the default (upstream
    /// `onSelectAsDefault`).
    SelectAsDefault(String),
    /// Escape / select-cancel (upstream `onCancel`).
    Cancel,
}

/// Searchable thinking-level selector (upstream
/// `ThinkingSelectorComponent`).
pub struct ThinkingSelectorComponent {
    all_items: Vec<SelectItem>,
    search_input: Input,
    select_list: SelectList,
    focused: bool,
}

impl ThinkingSelectorComponent {
    /// Upstream the constructor: build the items, preselect `current_level`
    /// and mark the persisted default in the description.
    pub fn new(
        current_level: &str,
        available_levels: &[String],
        default_thinking_level: Option<&str>,
    ) -> Self {
        let all_items: Vec<SelectItem> = available_levels
            .iter()
            .map(|level| {
                let mut description = level_description(level).to_string();
                if Some(level.as_str()) == default_thinking_level {
                    if description.is_empty() {
                        description = "default".to_string();
                    } else {
                        description.push_str(" · default");
                    }
                }
                SelectItem {
                    value: level.clone(),
                    label: level.clone(),
                    description: (!description.is_empty()).then_some(description),
                }
            })
            .collect();
        let select_list = build_select_list(&all_items, Some(current_level));
        Self {
            all_items,
            search_input: Input::new(),
            select_list,
            focused: false,
        }
    }

    /// The currently highlighted level (upstream `getSelectList().getSelectedItem()`).
    pub fn selected_value(&self) -> Option<String> {
        self.select_list
            .get_selected_item()
            .map(|item| item.value.clone())
    }

    /// The search box contents.
    pub fn search_value(&self) -> String {
        self.search_input.get_value().to_string()
    }

    /// The visible (filtered) items.
    pub fn visible_items(&self) -> &[SelectItem] {
        self.select_list.filtered_items()
    }

    /// Upstream `applyFilter`: fuzzy-filter the items by the search box and
    /// keep the highlighted value selected.
    pub fn apply_filter(&mut self) {
        let query = self.search_input.get_value().to_string();
        let selected = self.selected_value();
        if query.is_empty() {
            self.select_list = build_select_list(&self.all_items, selected.as_deref());
            return;
        }
        let filtered: Vec<SelectItem> =
            pillar_tui::fuzzy::fuzzy_filter_by(&self.all_items, &query, |item: &SelectItem| {
                match &item.description {
                    Some(description) => format!("{} {description}", item.label),
                    None => item.label.clone(),
                }
            })
            .into_iter()
            .cloned()
            .collect();
        self.select_list = build_select_list(&filtered, selected.as_deref());
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ThinkingSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches_key(data, "ctrl+s") {
            return match self.selected_value() {
                Some(level) => ThinkingSelectorOutcome::SelectAsDefault(level),
                None => ThinkingSelectorOutcome::Consumed,
            };
        }

        if matches("tui.select.cancel") {
            return ThinkingSelectorOutcome::Cancel;
        }
        if matches("tui.select.confirm") || matches("tui.input.submit") {
            return match self.selected_value() {
                Some(level) => ThinkingSelectorOutcome::Select(level),
                None => ThinkingSelectorOutcome::Consumed,
            };
        }
        if matches("tui.select.up") {
            self.select_list.move_up();
            return ThinkingSelectorOutcome::Consumed;
        }
        if matches("tui.select.down") {
            self.select_list.move_down();
            return ThinkingSelectorOutcome::Consumed;
        }

        // Everything else goes to the search box (upstream `searchInput`):
        // editing keys first, then printable input.
        if dispatch_input_keybinding(&mut self.search_input, data) {
            self.apply_filter();
            return ThinkingSelectorOutcome::Consumed;
        }
        self.search_input.handle_input(data);
        self.apply_filter();
        ThinkingSelectorOutcome::Consumed
    }
}

/// Upstream `buildSelectList`.
fn build_select_list(items: &[SelectItem], preselect: Option<&str>) -> SelectList {
    let mut list = SelectList::new(
        items.to_vec(),
        items.len().max(1),
        thinking_select_list_layout(),
    );
    if let Some(preselect) = preselect
        && let Some(index) = items.iter().position(|item| item.value == preselect)
    {
        list.set_selected_index(index);
    }
    list
}

impl Component for ThinkingSelectorComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(
            DynamicBorder::new()
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            Text::new("Thinking Level", 0, 0)
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            Text::new(
                &format!(
                    "{} cycles thinking levels in-session",
                    key_display_text("app.thinking.cycle")
                ),
                0,
                0,
            )
            .render(width)
            .iter()
            .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            self.search_input
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            self.select_list
                .render(width, &get_select_list_theme())
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            Text::new(
                &theme_handle.fg(
                    "dim",
                    "  Enter to select · Ctrl+S to set as default · Esc to cancel",
                ),
                0,
                0,
            )
            .render(width)
            .iter()
            .map(|line| line.to_string()),
        );
        lines.extend(
            DynamicBorder::new()
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        render_lines(lines)
    }
}

impl Focusable for ThinkingSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
