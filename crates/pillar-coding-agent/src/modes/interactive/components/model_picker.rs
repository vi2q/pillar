//! Two-column model picker (the `/m` selector).
//!
//! Not an upstream component: this ports the picker of the user's
//! `pi-model-picker` extension (<https://github.com/vi2q/pi-model-picker>)
//! natively, because the port has no extension UI bridge (`ExtensionUIContext`)
//! yet. Layout and behaviour follow the extension:
//!
//! ```text
//!  commandcode/gpt-5.6-luna
//!  ←→ category   ↑↓ model   Enter select   Esc cancel
//!
//!  › RECENT              commandcode/gpt-5.6-luna
//!    PROVIDERS           openai-codex/gpt-5.3-codex
//!    CommandCode         commandcode/claude-opus-5
//!
//!  commandcode/gpt-5.6-luna · ctx 1,100,000
//! ```
//!
//! The left column holds the categories (`RECENT` when there is history, then
//! one per provider behind a `PROVIDERS` label); the right column lists the
//! highlighted category's models. Both columns scroll independently through one
//! window sized to the terminal height.
//!
//! divergence: upstream reads `tui.terminal.rows` on every render; the port's
//! components have no TUI handle, so the host passes the terminal height in as
//! a shared cell the pump keeps up to date on resize.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pillar_ai::types::Model;
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::keys::matches_key;
use pillar_tui::text_utils::{truncate_to_width, visible_width};
use pillar_tui::tui::Focusable;

use crate::modes::interactive::theme::theme;

/// What a left-column row stands for (upstream `Category.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryKind {
    /// The recent-model history (spans providers).
    Recent,
    /// One provider's models.
    Provider,
}

/// One selectable unit of the left column (upstream `Category`).
#[derive(Debug, Clone, PartialEq)]
pub struct PickerCategory {
    pub kind: CategoryKind,
    /// The category identity: the provider id, or the recent sentinel.
    pub id: String,
    pub label: String,
    pub models: Vec<Model>,
}

/// Upstream `RECENT_CATEGORY_ID`.
pub const RECENT_CATEGORY_ID: &str = "__recent__";

/// Upstream `listRows`: the picker never takes the whole screen.
fn list_rows(terminal_rows: usize) -> usize {
    (terminal_rows.saturating_sub(8)).clamp(3, 16)
}

/// Upstream `windowStart`: the scroll window that keeps the selected row
/// centered.
fn window_start(selected: usize, total: usize, visible: usize) -> usize {
    if total <= visible {
        return 0;
    }
    selected.saturating_sub(visible / 2).min(total - visible)
}

/// Upstream `toLocaleString()` for the context window in the footer.
fn format_context_window(context_window: u64) -> String {
    let digits = context_window.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// What the host must do after a key (the extension's `onSelect` / `onCancel`).
#[derive(Debug, Clone, PartialEq)]
pub enum ModelPickerOutcome {
    /// Handled inside the picker (navigation).
    Consumed,
    /// Enter: switch to `model` (upstream `onSelect`).
    Select(Box<Model>),
    /// Escape / Ctrl+C (upstream `onCancel`).
    Cancel,
}

/// The two-column picker (upstream `ProviderModelPicker`).
pub struct ModelPickerComponent {
    categories: Vec<PickerCategory>,
    category_index: usize,
    model_index: usize,
    active_model: Option<Model>,
    terminal_rows: Arc<AtomicUsize>,
    focused: bool,
}

impl ModelPickerComponent {
    /// Open on the recent category when there is history (upstream the
    /// `/m` handler's `startCategory`), else on the first category.
    pub fn new(
        categories: Vec<PickerCategory>,
        active_model: Option<Model>,
        terminal_rows: Arc<AtomicUsize>,
    ) -> Self {
        let category_index = categories
            .iter()
            .position(|category| category.kind == CategoryKind::Recent)
            .unwrap_or(0);
        Self {
            categories,
            category_index,
            model_index: 0,
            active_model,
            terminal_rows,
            focused: false,
        }
    }

    /// The categories, left column order.
    pub fn categories(&self) -> &[PickerCategory] {
        &self.categories
    }

    pub fn category_index(&self) -> usize {
        self.category_index
    }

    pub fn model_index(&self) -> usize {
        self.model_index
    }

    pub fn selected_model(&self) -> Option<&Model> {
        self.categories
            .get(self.category_index)
            .and_then(|category| category.models.get(self.model_index))
    }

    /// Upstream `moveCategory`: wraps and resets the model position.
    fn move_category(&mut self, delta: i64) {
        let count = self.categories.len();
        if count == 0 {
            return;
        }
        self.category_index = ((self.category_index as i64 + delta).rem_euclid(count as i64)) as usize;
        self.model_index = 0;
    }

    /// Upstream `moveModel`: wraps within the highlighted category.
    fn move_model(&mut self, delta: i64) {
        let Some(count) = self
            .categories
            .get(self.category_index)
            .map(|category| category.models.len())
        else {
            return;
        };
        if count == 0 {
            return;
        }
        self.model_index = ((self.model_index as i64 + delta).rem_euclid(count as i64)) as usize;
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ModelPickerOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches_key(data, "left") {
            self.move_category(-1);
        } else if matches_key(data, "right") {
            self.move_category(1);
        } else if matches("tui.select.up") {
            self.move_model(-1);
        } else if matches("tui.select.down") {
            self.move_model(1);
        } else if matches("tui.select.pageUp") {
            self.move_model(-10);
        } else if matches("tui.select.pageDown") {
            self.move_model(10);
        } else if matches("tui.select.confirm") {
            return match self.selected_model() {
                Some(model) => ModelPickerOutcome::Select(Box::new(model.clone())),
                None => ModelPickerOutcome::Consumed,
            };
        } else if matches("tui.select.cancel") {
            return ModelPickerOutcome::Cancel;
        } else {
            return ModelPickerOutcome::Consumed;
        }
        ModelPickerOutcome::Consumed
    }

    /// Upstream `buildLeftRows`: the `PROVIDERS` label, then one row per
    /// category.
    fn left_rows(&self) -> Vec<LeftRow> {
        let mut rows = Vec::new();
        for (index, category) in self.categories.iter().enumerate() {
            let first_provider = category.kind == CategoryKind::Provider
                && (index == 0 || self.categories[index - 1].kind == CategoryKind::Recent);
            if first_provider {
                rows.push(LeftRow {
                    text: "PROVIDERS".to_string(),
                    is_category: false,
                    active: false,
                });
            }
            let active = index == self.category_index;
            rows.push(LeftRow {
                text: format!("{}{}", if active { "› " } else { "  " }, category.label),
                is_category: true,
                active,
            });
        }
        rows
    }

    /// The fixed left-column width (upstream `providerColWidth`).
    fn provider_col_width(&self, width: usize) -> usize {
        let widest = self
            .categories
            .iter()
            .map(|category| visible_width(&category.label) + 4)
            .max()
            .unwrap_or(10)
            .max(10);
        widest.min(12.max(width / 2).saturating_sub(2))
    }
}

/// One left-column row (upstream the inline row objects).
struct LeftRow {
    text: String,
    is_category: bool,
    active: bool,
}

impl pillar_tui::tui::Component for ModelPickerComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();

        // Header: the model under the cursor (recent entries span providers,
        // so they are labelled with the full reference).
        let selected = self.selected_model();
        let is_recent = self
            .categories
            .get(self.category_index)
            .is_some_and(|category| category.kind == CategoryKind::Recent);
        let header_text = match selected {
            Some(model) if is_recent => format!("{}/{}", model.provider, model.id),
            Some(model) => model.name.clone(),
            None => "(no model)".to_string(),
        };
        lines.push(truncate_to_width(
            &format!(" {}", theme_handle.fg("accent", &theme_handle.bold(&header_text))),
            width,
            "",
            false,
        ));
        lines.push(truncate_to_width(
            &format!(
                " {}",
                theme_handle.fg(
                    "dim",
                    "←→ category   ↑↓ model   Enter select   Esc cancel"
                )
            ),
            width,
            "",
            false,
        ));

        let rows_visible = list_rows(self.terminal_rows.load(Ordering::SeqCst));
        let left_rows = self.left_rows();
        let model_count = self
            .categories
            .get(self.category_index)
            .map(|category| category.models.len())
            .unwrap_or(0);

        let selected_left_row = left_rows
            .iter()
            .position(|row| row.active)
            .unwrap_or(0);
        let left_start = window_start(selected_left_row, left_rows.len(), rows_visible);
        let model_start = window_start(self.model_index, model_count, rows_visible);
        let rows = rows_visible.min(
            left_rows
                .len()
                .saturating_sub(left_start)
                .max(model_count.saturating_sub(model_start)),
        );

        let provider_col_width = self.provider_col_width(width);
        for row in 0..rows {
            let left = match left_rows.get(left_start + row) {
                Some(left_row) => {
                    let name = truncate_to_width(&left_row.text, provider_col_width, "", false);
                    let fill = " ".repeat(provider_col_width.saturating_sub(visible_width(&name)));
                    if left_row.active {
                        // The highlight pads too, so its background forms a
                        // full band while the column width stays strict.
                        theme_handle.bg(
                            "selectedBg",
                            &theme_handle.fg("text", &theme_handle.bold(&format!("{name}{fill}"))),
                        )
                    } else if left_row.is_category {
                        format!("{}{fill}", theme_handle.fg("muted", &name))
                    } else {
                        // Label rows such as PROVIDERS.
                        format!("{}{fill}", theme_handle.fg("dim", &name))
                    }
                }
                // No category on this row: keep the column position fixed.
                None => " ".repeat(provider_col_width),
            };

            let right = match self
                .categories
                .get(self.category_index)
                .and_then(|category| category.models.get(model_start + row))
            {
                Some(model) => {
                    let is_selected = model_start + row == self.model_index;
                    let is_active = self.active_model.as_ref().is_some_and(|active| {
                        active.provider == model.provider && active.id == model.id
                    });
                    let label = if is_recent {
                        format!("{}/{}", model.provider, model.id)
                    } else {
                        model.name.clone()
                    };
                    let text = if is_active {
                        format!("{label} ✓")
                    } else {
                        label
                    };
                    if is_selected {
                        theme_handle.bg(
                            "selectedBg",
                            &theme_handle.fg("text", &theme_handle.bold(&text)),
                        )
                    } else if is_active {
                        theme_handle.fg("success", &text)
                    } else {
                        theme_handle.fg("muted", &text)
                    }
                }
                None => String::new(),
            };
            lines.push(truncate_to_width(
                &format!(" {left}{right}"),
                width,
                "",
                false,
            ));
        }

        // Scroll indicators, one per column.
        let category_count = self.categories.len();
        let category_scroll = if left_rows.len() > rows_visible {
            theme_handle.fg(
                "dim",
                &format!("  [{}/{}]", self.category_index + 1, category_count),
            )
        } else {
            String::new()
        };
        let model_scroll = if model_count > rows_visible {
            theme_handle.fg(
                "dim",
                &format!("  [{}/{}]", self.model_index + 1, model_count),
            )
        } else {
            String::new()
        };
        if !category_scroll.is_empty() || !model_scroll.is_empty() {
            lines.push(truncate_to_width(
                &format!(" {category_scroll}{model_scroll}"),
                width,
                "",
                false,
            ));
        }

        // Footer: the highlighted model's reference and context window.
        if let Some(model) = selected {
            lines.push(String::new());
            lines.push(truncate_to_width(
                &format!(
                    " {}",
                    theme_handle.fg(
                        "dim",
                        &format!(
                            "{}/{} · ctx {}",
                            model.provider,
                            model.id,
                            format_context_window(model.context_window)
                        )
                    )
                ),
                width,
                "",
                false,
            ));
        }
        lines
    }
}

impl Focusable for ModelPickerComponent {
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

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_string(),
            name: format!("{id} name"),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Default::default(),
            context_window: 1_100_000,
            max_tokens: 10_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn categories() -> Vec<PickerCategory> {
        vec![
            PickerCategory {
                kind: CategoryKind::Recent,
                id: RECENT_CATEGORY_ID.to_string(),
                label: "RECENT".to_string(),
                models: vec![model("m1", "p2"), model("m2", "p1")],
            },
            PickerCategory {
                kind: CategoryKind::Provider,
                id: "p1".to_string(),
                label: "Provider One".to_string(),
                models: vec![model("m2", "p1"), model("m3", "p1"), model("m4", "p1")],
            },
            PickerCategory {
                kind: CategoryKind::Provider,
                id: "p2".to_string(),
                label: "Provider Two".to_string(),
                models: vec![model("m1", "p2")],
            },
        ]
    }

    fn picker() -> ModelPickerComponent {
        ModelPickerComponent::new(
            categories(),
            Some(model("m2", "p1")),
            Arc::new(AtomicUsize::new(24)),
        )
    }

    #[test]
    fn opens_on_recent_and_falls_back_to_the_first_category() {
        let picker = picker();
        assert_eq!(picker.category_index(), 0);
        assert_eq!(picker.selected_model().map(|m| m.id.as_str()), Some("m1"));

        let mut without_recent = categories();
        without_recent.remove(0);
        let picker = ModelPickerComponent::new(
            without_recent,
            None,
            Arc::new(AtomicUsize::new(24)),
        );
        assert_eq!(picker.category_index(), 0);
        assert_eq!(picker.categories()[0].id, "p1");
    }

    #[test]
    fn categories_wrap_and_reset_the_model_position() {
        let mut picker = picker();
        picker.handle_key("\u{1b}[B"); // down
        assert_eq!(picker.model_index(), 1);

        assert_eq!(picker.handle_key("\u{1b}[C"), ModelPickerOutcome::Consumed); // right
        assert_eq!(picker.category_index(), 1);
        assert_eq!(picker.model_index(), 0, "category change resets the model");

        picker.handle_key("\u{1b}[C"); // right
        picker.handle_key("\u{1b}[C"); // right wraps to recent
        assert_eq!(picker.category_index(), 0);
        picker.handle_key("\u{1b}[D"); // left wraps to the last category
        assert_eq!(picker.category_index(), 2);
    }

    #[test]
    fn models_wrap_and_page_by_ten() {
        let mut picker = picker();
        picker.handle_key("\u{1b}[C"); // provider one (3 models)
        assert_eq!(picker.category_index(), 1);

        picker.handle_key("\u{1b}[B");
        picker.handle_key("\u{1b}[B");
        assert_eq!(picker.model_index(), 2);
        picker.handle_key("\u{1b}[B");
        assert_eq!(picker.model_index(), 0, "down wraps");
        picker.handle_key("\u{1b}[A");
        assert_eq!(picker.model_index(), 2, "up wraps");

        // PgDn/PgUp move by ten, wrapping around the short list. (The port
        // uses a true modulo: the extension's `(index + delta) % count` goes
        // negative for lists shorter than the step.)
        assert_eq!(picker.model_index(), 2);
        assert_eq!(
            picker.handle_key("\u{1b}[6~"),
            ModelPickerOutcome::Consumed,
            "page down"
        );
        assert_eq!(picker.model_index(), 0, "2 + 10 wraps to 0 of 3");
        assert_eq!(picker.handle_key("\u{1b}[5~"), ModelPickerOutcome::Consumed);
        assert_eq!(picker.model_index(), 2, "0 - 10 wraps back to 2");
    }

    #[test]
    fn enter_selects_and_escape_cancels() {
        let mut picker = picker();
        match picker.handle_key("\r") {
            ModelPickerOutcome::Select(model) => {
                assert_eq!((model.provider.as_str(), model.id.as_str()), ("p2", "m1"));
            }
            other => panic!("expected Select, got {other:?}"),
        }
        assert_eq!(picker.handle_key("\u{1b}"), ModelPickerOutcome::Cancel);
        assert_eq!(picker.handle_key("\u{3}"), ModelPickerOutcome::Cancel);
        // Unrelated keys are consumed without moving.
        assert_eq!(picker.handle_key("x"), ModelPickerOutcome::Consumed);
    }

    #[test]
    fn window_and_column_geometry_match_the_extension() {
        assert_eq!(window_start(0, 20, 10), 0);
        assert_eq!(window_start(10, 20, 10), 5, "centered");
        assert_eq!(window_start(19, 20, 10), 10, "clamped to the end");
        assert_eq!(window_start(3, 5, 10), 0, "everything fits");

        assert_eq!(list_rows(24), 16, "capped");
        assert_eq!(list_rows(12), 4);
        assert_eq!(list_rows(5), 3, "at least three rows");
        assert_eq!(format_context_window(1_100_000), "1,100,000");
        assert_eq!(format_context_window(200_000), "200,000");
        assert_eq!(format_context_window(999), "999");
    }

    #[test]
    fn empty_categories_are_safe() {
        let mut picker =
            ModelPickerComponent::new(Vec::new(), None, Arc::new(AtomicUsize::new(24)));
        assert_eq!(picker.handle_key("\r"), ModelPickerOutcome::Consumed);
        assert_eq!(picker.handle_key("\u{1b}[B"), ModelPickerOutcome::Consumed);
        assert!(picker.selected_model().is_none());

        // An empty category (no available models) has nothing to move to.
        let mut picker = ModelPickerComponent::new(
            vec![PickerCategory {
                kind: CategoryKind::Provider,
                id: "p".to_string(),
                label: "p".to_string(),
                models: Vec::new(),
            }],
            None,
            Arc::new(AtomicUsize::new(24)),
        );
        assert_eq!(picker.handle_key("\u{1b}[B"), ModelPickerOutcome::Consumed);
        assert_eq!(picker.model_index(), 0);
    }
}

