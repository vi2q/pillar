//! Port of packages/tui/src/components/settings-list.ts (pi v0.84.3):
//! settings list with value cycling, submenus, fuzzy search, scrolling,
//! and a description/hint footer.
//!
//! divergences: keybinding dispatch (handleInput) stays host-side; the
//! port exposes movement/activation primitives as plain methods. The
//! Input search box is embedded directly. Submenu components are render
//! closures over a done callback, matching the closure-based component
//! convention used elsewhere in the port.

use crate::fuzzy::fuzzy_filter_by;
use crate::input::Input;
use crate::text_utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

/// A settings entry (upstream `SettingItem`).
#[derive(Clone)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    /// Enter/Space cycles through these when present.
    pub values: Vec<String>,
    /// When present, Enter opens a submenu. The value is an opaque key: the
    /// owner (the coding agent) builds the component, so this crate keeps no
    /// dependency on the submenu widgets.
    ///
    /// divergence: upstream stores a `submenu(currentValue, done) => Component`
    /// closure on the item; the port keeps the component assembly in the
    /// caller (the port's components answer outcomes instead of invoking
    /// callbacks).
    pub submenu: Option<String>,
}

/// What activating a setting did (upstream `activateItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsActivation {
    /// Nothing selected.
    None,
    /// A value was cycled: `(id, new value)`.
    Cycled { id: String, value: String },
    /// The item opens a submenu: the item id, the opaque submenu key and the
    /// item's current value (upstream passes it for pre-selection).
    OpenSubmenu {
        id: String,
        submenu: String,
        current_value: String,
    },
}

/// Hook type for label/value theming: receives the text and whether the
/// row is selected.
pub type SelectedTextFn = dyn Fn(&str, bool) -> String + Send;

/// Theme hooks (upstream `SettingsListTheme`).
pub struct SettingsListTheme {
    pub label: Box<SelectedTextFn>,
    pub value: Box<SelectedTextFn>,
    pub description: Box<dyn Fn(&str) -> String + Send>,
    pub cursor: String,
    pub hint: Box<dyn Fn(&str) -> String + Send>,
}

/// Done callback type for submenus (upstream `done`): receives an
/// optional selected value and an optional id to navigate to after close.
pub type SubmenuDone<'a> = &'a mut dyn FnMut(Option<String>, Option<String>);

/// Settings list (upstream `SettingsList`).
pub struct SettingsList {
    items: Vec<SettingItem>,
    filtered_items: Vec<usize>,
    max_visible: usize,
    selected_index: usize,
    search_enabled: bool,
    search_input: Option<Input>,
    /// When Some, a submenu is open over this item (by id).
    submenu_open: Option<String>,
    navigate_after_close: Option<String>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        on_change: &mut dyn FnMut(&str, &str),
        on_cancel: &mut dyn FnMut(),
        enable_search: bool,
    ) -> Self {
        let _ = (on_change, on_cancel);
        Self {
            filtered_items: (0..items.len()).collect(),
            items,
            max_visible,
            selected_index: 0,
            search_enabled: enable_search,
            search_input: enable_search.then(Input::new),
            submenu_open: None,
            navigate_after_close: None,
        }
    }

    /// Update an item's `current_value` (upstream `updateValue`).
    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    /// Move selection to the item with the given id; no-op if not found
    /// (upstream `selectItem`).
    pub fn select_item(&mut self, id: &str) {
        let indices = self.display_indices();
        if let Some(index) = indices.iter().position(|&i| self.items[i].id == id) {
            self.selected_index = index;
        }
    }

    fn display_indices(&self) -> Vec<usize> {
        if self.search_enabled {
            self.filtered_items.clone()
        } else {
            (0..self.items.len()).collect()
        }
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// The setting rows in order (upstream `items`).
    pub fn items(&self) -> &[SettingItem] {
        &self.items
    }

    /// The id of the highlighted item (test / host convenience).
    pub fn selected_item_id(&self) -> Option<String> {
        let display: Vec<usize> = if self.search_enabled {
            self.filtered_items.clone()
        } else {
            (0..self.items.len()).collect()
        };
        display
            .get(self.selected_index)
            .map(|&index| self.items[index].id.clone())
    }

    pub fn search_input_mut(&mut self) -> Option<&mut Input> {
        self.search_input.as_mut()
    }

    /// Fuzzy-filter by label; resets selection (upstream `applyFilter`).
    pub fn apply_filter(&mut self, query: &str) {
        self.filtered_items = fuzzy_filter_by(&self.items, query, |item| item.label.clone())
            .into_iter()
            .map(|item| {
                self.items
                    .iter()
                    .position(|i| i.id == item.id)
                    .unwrap_or_default()
            })
            .collect();
        self.selected_index = 0;
    }

    pub fn move_up(&mut self) {
        let len = self.display_len();
        if len == 0 {
            return;
        }
        self.selected_index = if self.selected_index == 0 {
            len - 1
        } else {
            self.selected_index - 1
        };
    }

    pub fn move_down(&mut self) {
        let len = self.display_len();
        if len == 0 {
            return;
        }
        self.selected_index = if self.selected_index == len - 1 {
            0
        } else {
            self.selected_index + 1
        };
    }

    fn display_len(&self) -> usize {
        if self.search_enabled {
            self.filtered_items.len()
        } else {
            self.items.len()
        }
    }

    /// Activate the selected item: report a submenu to open or cycle its
    /// values (upstream `activateItem`).
    pub fn activate_selected(
        &mut self,
        on_change: &mut dyn FnMut(&str, &str),
    ) -> SettingsActivation {
        let selected = if self.search_enabled {
            self.filtered_items.get(self.selected_index).copied()
        } else if self.selected_index < self.items.len() {
            Some(self.selected_index)
        } else {
            None
        };
        let Some(index) = selected else {
            return SettingsActivation::None;
        };

        // A submenu opens before any value cycling (upstream checks `submenu`
        // first).
        if let Some(submenu) = self.items[index].submenu.clone() {
            return SettingsActivation::OpenSubmenu {
                id: self.items[index].id.clone(),
                submenu,
                current_value: self.items[index].current_value.clone(),
            };
        }

        let values_len = self.items[index].values.len();
        if values_len > 0 {
            let current = self.items[index].current_value.clone();
            let current_index = self.items[index]
                .values
                .iter()
                .position(|v| *v == current)
                .unwrap_or(0);
            let next = self.items[index].values[(current_index + 1) % values_len].clone();
            self.items[index].current_value = next.clone();
            on_change(&self.items[index].id, &next);
            SettingsActivation::Cycled {
                id: self.items[index].id.clone(),
                value: next,
            }
        } else {
            SettingsActivation::None
        }
    }

    /// Close the active submenu (upstream `closeSubmenu`): restores the
    /// selection or navigates to a requested id.
    pub fn close_submenu(&mut self) {
        self.submenu_open = None;
        if let Some(id) = self.navigate_after_close.take() {
            self.select_item(&id);
        }
    }

    pub fn is_submenu_open(&self) -> bool {
        self.submenu_open.is_some()
    }

    pub fn render(&self, width: usize, theme: &SettingsListTheme) -> Vec<String> {
        self.render_main_list(width, theme)
    }

    fn render_main_list(&self, width: usize, theme: &SettingsListTheme) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.search_enabled {
            if let Some(input) = &self.search_input {
                lines.extend(input.render(width));
                lines.push(String::new());
            }
        }

        if self.items.is_empty() {
            lines.push((theme.hint)("  No settings available"));
            self.add_hint_line(&mut lines, width, theme);
            return lines;
        }

        let display: Vec<usize> = if self.search_enabled {
            self.filtered_items.clone()
        } else {
            (0..self.items.len()).collect()
        };
        if display.is_empty() {
            lines.push(truncate_to_width(
                &(theme.hint)("  No matching settings"),
                width,
                "",
                false,
            ));
            self.add_hint_line(&mut lines, width, theme);
            return lines;
        }

        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(display.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(display.len());

        let max_label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(36);

        for (i, &item_index) in display.iter().enumerate().take(end_index).skip(start_index) {
            let item = &self.items[item_index];
            let is_selected = i == self.selected_index;
            let prefix = if is_selected {
                theme.cursor.as_str()
            } else {
                "  "
            };
            let prefix_width = visible_width(prefix);

            let label_padded = format!(
                "{}{}",
                item.label,
                " ".repeat(max_label_width.saturating_sub(visible_width(&item.label)))
            );
            let label_text = (theme.label)(&label_padded, is_selected);

            let separator = "  ";
            let used_width = prefix_width + max_label_width + visible_width(separator);
            let value_max_width = width.saturating_sub(used_width + 2);
            let value_text = (theme.value)(
                &truncate_to_width(&item.current_value, value_max_width, "", false),
                is_selected,
            );

            lines.push(truncate_to_width(
                &format!("{prefix}{label_text}{separator}{value_text}"),
                width,
                "",
                false,
            ));
        }

        if start_index > 0 || end_index < display.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, display.len());
            lines.push((theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        if let Some(selected) = display.get(self.selected_index) {
            if let Some(description) = &self.items[*selected].description {
                lines.push(String::new());
                for line in wrap_text_with_ansi(description, width.saturating_sub(4)) {
                    lines.push((theme.description)(&format!("  {line}")));
                }
            }
        }

        self.add_hint_line(&mut lines, width, theme);
        lines
    }

    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize, theme: &SettingsListTheme) {
        lines.push(String::new());
        let hint = if self.search_enabled {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(truncate_to_width(&(theme.hint)(hint), width, "", false));
    }
}
