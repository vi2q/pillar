//! Port of packages/tui/src/components/select-list.ts (pi v0.84.3):
//! filtered item list with two-column layout, scrolling window, and
//! selection callbacks.
//!
//! divergences: keybinding dispatch (handleInput) stays host-side since
//! the keybindings table lives in the runtime; the port exposes the
//! same movement primitives (up/down wrap, confirm, cancel) as plain
//! methods. Theme functions are plain fn pointers / closures instead of
//! an object.

use crate::text_utils::{truncate_to_width, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

fn normalize_to_single_line(text: &str) -> String {
    // Collapse runs of CR/LF into a single space (upstream
    // /[\r\n]+/g → " ").
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            in_run = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

/// A selectable item (upstream `SelectItem`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Theme hooks (upstream `SelectListTheme`).
pub struct SelectListTheme {
    pub selected_prefix: Box<dyn Fn(&str) -> String + Send>,
    pub selected_text: Box<dyn Fn(&str) -> String + Send>,
    pub description: Box<dyn Fn(&str) -> String + Send>,
    pub scroll_info: Box<dyn Fn(&str) -> String + Send>,
    pub no_match: Box<dyn Fn(&str) -> String + Send>,
}

impl Default for SelectListTheme {
    /// Unstyled defaults (port-only): every hook passes its text through.
    fn default() -> Self {
        Self {
            selected_prefix: Box::new(|text| text.to_string()),
            selected_text: Box::new(|text| text.to_string()),
            description: Box::new(|text| text.to_string()),
            scroll_info: Box::new(|text| text.to_string()),
            no_match: Box::new(|text| text.to_string()),
        }
    }
}

/// Layout overrides (upstream `SelectListLayoutOptions`).
#[derive(Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<Box<TruncatePrimaryFn>>,
}

/// Hook type for a custom `truncate_primary` (upstream the
/// `truncatePrimary` option).
pub type TruncatePrimaryFn = dyn Fn(TruncatePrimaryContext<'_>) -> String + Send;

/// Context passed to a custom `truncate_primary` hook (upstream
/// `SelectListTruncatePrimaryContext`).
pub struct TruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

/// Filtered select list (upstream `SelectList`).
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<SelectItem>,
    selected_index: usize,
    max_visible: usize,
    layout: SelectListLayoutOptions,
}

impl SelectList {
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        layout: SelectListLayoutOptions,
    ) -> Self {
        Self {
            filtered_items: items.clone(),
            items,
            selected_index: 0,
            max_visible,
            layout,
        }
    }

    /// Filter by case-insensitive value prefix; resets selection
    /// (upstream `setFilter`).
    pub fn set_filter(&mut self, filter: &str) {
        let filter = filter.to_lowercase();
        self.filtered_items = self
            .items
            .iter()
            .filter(|item| item.value.to_lowercase().starts_with(&filter))
            .cloned()
            .collect();
        self.selected_index = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index.min(self.filtered_items.len().saturating_sub(1));
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn filtered_items(&self) -> &[SelectItem] {
        &self.filtered_items
    }

    /// Currently highlighted item (upstream `getSelectedItem`).
    pub fn get_selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items.get(self.selected_index)
    }

    /// Move up with wrap (upstream the `tui.select.up` branch).
    pub fn move_up(&mut self) {
        self.selected_index = if self.selected_index == 0 {
            self.filtered_items.len().saturating_sub(1)
        } else {
            self.selected_index - 1
        };
    }

    /// Move down with wrap (upstream the `tui.select.down` branch).
    pub fn move_down(&mut self) {
        if !self.filtered_items.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.filtered_items.len();
        }
    }

    pub fn render(&self, width: usize, theme: &SelectListTheme) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.filtered_items.is_empty() {
            lines.push((theme.no_match)("  No matching commands"));
            return lines;
        }

        let primary_column_width = self.get_primary_column_width();

        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered_items.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.filtered_items.len());

        for i in start_index..end_index {
            let item = &self.filtered_items[i];
            let is_selected = i == self.selected_index;
            let description_single_line = item
                .description
                .as_ref()
                .map(|d| normalize_to_single_line(d));
            lines.push(self.render_item(
                item,
                is_selected,
                width,
                description_single_line.as_deref(),
                primary_column_width,
                theme,
            ));
        }

        if start_index > 0 || end_index < self.filtered_items.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_items.len()
            );
            lines.push((theme.scroll_info)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        lines
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
        theme: &SelectListTheme,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(description) = description_single_line
            && width > 40
        {
            let effective_primary_column_width = primary_column_width
                .min(width.saturating_sub(prefix_width + 4))
                .max(1);
            let max_primary_width = (effective_primary_column_width - 1).max(1);
            let truncated_value = self.truncate_primary(
                item,
                is_selected,
                max_primary_width,
                effective_primary_column_width,
            );
            let truncated_value_width = visible_width(&truncated_value);
            let spacing = " ".repeat(
                effective_primary_column_width
                    .saturating_sub(truncated_value_width)
                    .max(1),
            );
            let description_start = prefix_width + truncated_value_width + spacing.len();
            let remaining_width = width.saturating_sub(description_start + 2); // -2 for safety

            if remaining_width > MIN_DESCRIPTION_WIDTH {
                let truncated_desc = truncate_to_width(description, remaining_width, "", false);
                if is_selected {
                    return (theme.selected_text)(&format!(
                        "{prefix}{truncated_value}{spacing}{truncated_desc}"
                    ));
                }
                let desc_text = (theme.description)(&format!("{spacing}{truncated_desc}"));
                return format!("{prefix}{truncated_value}{desc_text}");
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated_value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            (theme.selected_text)(&format!("{prefix}{truncated_value}"))
        } else {
            format!("{prefix}{truncated_value}")
        }
    }

    fn get_primary_column_width(&self) -> usize {
        let bounds = self.get_primary_column_bounds();
        let widest_primary = self
            .filtered_items
            .iter()
            .map(|item| visible_width(&self.get_display_value(item)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        clamp(widest_primary, bounds.0, bounds.1)
    }

    fn get_primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        is_selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display_value = self.get_display_value(item);
        let truncated_value = match &self.layout.truncate_primary {
            Some(hook) => hook(TruncatePrimaryContext {
                text: &display_value,
                max_width,
                column_width,
                item,
                is_selected,
            }),
            None => truncate_to_width(&display_value, max_width, "", false),
        };
        truncate_to_width(&truncated_value, max_width, "", false)
    }

    fn get_display_value(&self, item: &SelectItem) -> String {
        if item.label.is_empty() {
            item.value.clone()
        } else {
            item.label.clone()
        }
    }
}
