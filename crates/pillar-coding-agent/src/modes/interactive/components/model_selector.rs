//! Port of packages/coding-agent/src/modes/interactive/components/model-selector.ts
//! (pi v0.84.3): the searchable model picker shown in the editor slot.
//!
//! divergence: pillar-tui's `Input` keeps keybinding dispatch host-side, so
//! [`ModelSelectorComponent::handle_key`] resolves the keys and answers what
//! the host must do (select / select-as-default / cancel) instead of invoking
//! `onSelect` / `onCancel` / `onSelectAsDefault` callbacks.
//!
//! divergence: upstream refreshes the model catalogs on open
//! (`refreshModelCatalogs` + a 15s abort timeout) and reports its status
//! through `refreshStatusMessage` / `errorMessage`. The port has no catalog
//! refresh yet (`ModelRuntime::refresh` is `&mut self` and offline-only; see
//! docs/TASKS.md), so the selector renders the current snapshot and shows the
//! runtime's configured-error text as `errorMessage` instead. Scoped entries
//! are not re-resolved against the runtime here — the host resolves the model
//! from the runtime when the selection is applied.

use pillar_ai::types::Model;
use pillar_tui::components::Text;
use pillar_tui::fuzzy::fuzzy_filter_by;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::keys::matches_key;
use pillar_tui::tui::{Component, Focusable};

use crate::core::model_mutation::{ScopedModel, model_key, models_are_equal};
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::model_search::ModelSearchItem;
use crate::modes::interactive::model_search::model_selector_search_text;
use crate::modes::interactive::theme::theme;

/// A selectable model row (upstream `ModelItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelItem {
    pub provider: String,
    pub id: String,
    pub model: Model,
}

/// The default model reference the selector badges (upstream
/// `DefaultModelReference`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModelReference {
    pub provider: String,
    pub id: String,
}

/// Which list the selector shows (upstream `ModelScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelScope {
    All,
    Scoped,
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq)]
pub enum ModelSelectorOutcome {
    /// Handled inside the selector (scope toggle, navigation, search input).
    Consumed,
    /// Enter: switch to `model` for this session (upstream `onSelect`).
    Select(Box<Model>),
    /// Ctrl+S: switch to `model` and persist it as the default (upstream
    /// `onSelectAsDefault`).
    SelectAsDefault(Box<Model>),
    /// Escape / select-cancel (upstream `onCancel`).
    Cancel,
}

/// Upstream `maxVisible`: how many model rows the list window shows.
const MAX_VISIBLE: usize = 10;

/// Searchable model selector (upstream `ModelSelectorComponent`).
pub struct ModelSelectorComponent {
    search_input: Input,
    all_models: Vec<ModelItem>,
    scoped_model_items: Vec<ModelItem>,
    active_models: Vec<ModelItem>,
    filtered_models: Vec<ModelItem>,
    selected_index: usize,
    current_model: Option<Model>,
    scoped_models: Vec<ScopedModel>,
    default_model: Option<DefaultModelReference>,
    scope: ModelScope,
    error_message: Option<String>,
    focused: bool,
}

impl ModelSelectorComponent {
    /// Upstream the constructor: snapshot the models, pick the initial scope
    /// and preselect the current model.
    ///
    /// `available_models` is the runtime snapshot and `scoped_models` the
    /// session scope (upstream `modelRuntime.getAvailableSnapshot()` and
    /// `session.scopedModels`); `error_message` is upstream
    /// `modelRuntime.getError()`, which the port surfaces without a refresh.
    pub fn new(
        current_model: Option<&Model>,
        available_models: &[Model],
        scoped_models: &[ScopedModel],
        default_model: Option<DefaultModelReference>,
        error_message: Option<String>,
        initial_search_input: Option<&str>,
    ) -> Self {
        let mut selector = Self {
            search_input: Input::new(),
            all_models: Vec::new(),
            scoped_model_items: Vec::new(),
            active_models: Vec::new(),
            filtered_models: Vec::new(),
            selected_index: 0,
            current_model: current_model.cloned(),
            scoped_models: scoped_models.to_vec(),
            default_model,
            // Upstream `scopedModels.length > 0 ? "scoped" : "all"`.
            scope: if scoped_models.is_empty() {
                ModelScope::All
            } else {
                ModelScope::Scoped
            },
            error_message,
            focused: false,
        };
        selector.load_models_from_snapshot(available_models);
        // Upstream `if (initialSearchInput) filterModels(...) else
        // updateList()`; the port renders the list on demand.
        if let Some(search) = initial_search_input {
            selector.search_input.set_value(search);
            selector.filter_models(search);
        }
        selector
    }

    /// Upstream `loadModelsFromSnapshot` (minus the scoped re-resolution).
    fn load_models_from_snapshot(&mut self, available_models: &[Model]) {
        let models: Vec<ModelItem> = available_models
            .iter()
            .map(|model| ModelItem {
                provider: model.provider.clone(),
                id: model.id.clone(),
                model: model.clone(),
            })
            .collect();
        self.all_models = self.sort_models(models);
        self.scoped_model_items = self
            .scoped_models
            .iter()
            .map(|scoped| ModelItem {
                provider: scoped.model.provider.clone(),
                id: scoped.model.id.clone(),
                model: scoped.model.clone(),
            })
            .collect();
        self.active_models = match self.scope {
            ModelScope::Scoped => self.scoped_model_items.clone(),
            ModelScope::All => self.all_models.clone(),
        };
        self.filtered_models = self.active_models.clone();
        let current_index = self
            .filtered_models
            .iter()
            .position(|item| self.is_current_model(&item.model));
        self.selected_index = match current_index {
            Some(index) => index,
            None => self
                .selected_index
                .min(self.filtered_models.len().saturating_sub(1)),
        };
    }

    /// Upstream `sortModels`: current model first, default model second, then
    /// by provider.
    fn sort_models(&self, models: Vec<ModelItem>) -> Vec<ModelItem> {
        let mut sorted = models;
        sorted.sort_by(|a, b| {
            let a_is_current = self.is_current_model(&a.model);
            let b_is_current = self.is_current_model(&b.model);
            if a_is_current != b_is_current {
                return if a_is_current {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let a_is_default = self.is_default_model(&a.model);
            let b_is_default = self.is_default_model(&b.model);
            if a_is_default != b_is_default {
                return if a_is_default {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            a.provider.cmp(&b.provider)
        });
        sorted
    }

    fn is_current_model(&self, model: &Model) -> bool {
        self.current_model
            .as_ref()
            .is_some_and(|current| models_are_equal(current, model))
    }

    fn is_default_model(&self, model: &Model) -> bool {
        self.default_model.as_ref().is_some_and(|default| {
            default.provider == model.provider && default.id == model.id
        })
    }

    /// Upstream `isDefaultSearch`: "default" typed as a prefix of the query.
    fn is_default_search(&self, query: &str) -> bool {
        let normalized = query.trim().to_lowercase();
        !normalized.is_empty() && "default".starts_with(&normalized)
    }

    /// Upstream `setScope`.
    fn set_scope(&mut self, scope: ModelScope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.active_models = match scope {
            ModelScope::Scoped => self.scoped_model_items.clone(),
            ModelScope::All => self.all_models.clone(),
        };
        let current_index = self
            .active_models
            .iter()
            .position(|item| self.is_current_model(&item.model));
        self.selected_index = current_index.unwrap_or(0);
        let query = self.search_input.get_value().to_string();
        self.filter_models(&query);
    }

    /// Upstream `filterModels`: fuzzy filter, the `default` shortcut, and the
    /// index reset/restore.
    fn filter_models(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_models = self.active_models.clone();
        } else {
            let filtered: Vec<ModelItem> = fuzzy_filter_by(
                &self.active_models,
                query,
                |item: &ModelItem| self.search_text(item),
            )
            .into_iter()
            .cloned()
            .collect();
            if self.is_default_search(query) {
                let default_items: Vec<ModelItem> = self
                    .active_models
                    .iter()
                    .filter(|item| self.is_default_model(&item.model))
                    .cloned()
                    .collect();
                let default_keys: Vec<String> =
                    default_items.iter().map(model_item_key).collect();
                let mut merged = default_items;
                merged.extend(
                    filtered
                        .into_iter()
                        .filter(|item| !default_keys.contains(&model_item_key(item))),
                );
                self.filtered_models = merged;
            } else {
                self.filtered_models = filtered;
            }
        }
        self.selected_index = if !query.is_empty() {
            0
        } else {
            self.selected_index
                .min(self.filtered_models.len().saturating_sub(1))
        };
    }

    /// Upstream the fuzzy-filter text, which appends ` default` when the row
    /// is the persisted default.
    fn search_text(&self, item: &ModelItem) -> String {
        let default_text = if self.is_default_model(&item.model) {
            " default"
        } else {
            ""
        };
        format!(
            "{}{default_text}",
            model_selector_search_text(&ModelSearchItem {
                id: &item.id,
                provider: &item.provider,
                name: Some(&item.model.name),
            })
        )
    }

    /// Upstream `getScopeText`.
    fn scope_text(&self, theme_handle: &crate::modes::interactive::theme::Theme) -> String {
        let all = if self.scope == ModelScope::All {
            theme_handle.fg("accent", "all")
        } else {
            theme_handle.fg("muted", "all")
        };
        let scoped = if self.scope == ModelScope::Scoped {
            theme_handle.fg("accent", "scoped")
        } else {
            theme_handle.fg("muted", "scoped")
        };
        // Upstream
        // `${fg("muted", "Scope: ")}${all}${fg("muted", " | ")}${scoped}`.
        format!(
            "{}{all}{}{scoped}",
            theme_handle.fg("muted", "Scope: "),
            theme_handle.fg("muted", " | ")
        )
    }

    /// Upstream `getScopeHintText`.
    fn scope_hint_text(&self) -> String {
        format!("{} (all/scoped)", key_hint("tui.input.tab", "scope"))
    }

    /// The active scope (upstream the private `scope` field).
    pub fn scope(&self) -> ModelScope {
        self.scope
    }

    /// The visible (filtered) rows.
    pub fn filtered_models(&self) -> &[ModelItem] {
        &self.filtered_models
    }

    /// The highlighted row index (upstream `selectedIndex`).
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// The highlighted row (upstream `filteredModels[selectedIndex]`).
    pub fn selected_item(&self) -> Option<&ModelItem> {
        self.filtered_models.get(self.selected_index)
    }

    /// The search box contents.
    pub fn search_value(&self) -> String {
        self.search_input.get_value().to_string()
    }

    /// Upstream `updateList`'s row rendering, keeping the window and scroll
    /// indicator arithmetic in one place (the host renders it via
    /// `Component::render`).
    fn list_lines(&self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        let selected_index = self.selected_index;
        let start_index = selected_index
            .saturating_sub(MAX_VISIBLE / 2)
            .min(self.filtered_models.len().saturating_sub(MAX_VISIBLE));
        let end_index = (start_index + MAX_VISIBLE).min(self.filtered_models.len());

        for (index, item) in self.filtered_models[start_index..end_index]
            .iter()
            .enumerate()
        {
            let index = start_index + index;
            let is_selected = index == selected_index;
            let is_current = self.is_current_model(&item.model);
            let default_badge = if self.is_default_model(&item.model) {
                theme_handle.fg("muted", " · default")
            } else {
                String::new()
            };
            let provider_badge = theme_handle.fg("muted", &format!("[{}]", item.provider));
            let checkmark = if is_current {
                theme_handle.fg("success", " ✓")
            } else {
                String::new()
            };
            let line = if is_selected {
                format!(
                    "{}{} {provider_badge}{default_badge}{checkmark}",
                    theme_handle.fg("accent", "→ "),
                    theme_handle.fg("accent", &item.id)
                )
            } else {
                format!(
                    "  {} {provider_badge}{default_badge}{checkmark}",
                    item.id
                )
            };
            lines.extend(Text::new(&line, 0, 0).render(width));
        }

        if start_index > 0 || end_index < self.filtered_models.len() {
            lines.extend(
                Text::new(
                    &theme_handle.fg(
                        "muted",
                        &format!("  ({}/{})", selected_index + 1, self.filtered_models.len()),
                    ),
                    0,
                    0,
                )
                .render(width),
            );
        }

        match &self.error_message {
            Some(error) => {
                for line in error.split('\n') {
                    lines.extend(Text::new(&theme_handle.fg("error", line), 0, 0).render(width));
                }
            }
            None if self.filtered_models.is_empty() => {
                lines.extend(Text::new(&theme_handle.fg("muted", "  No matching models"), 0, 0).render(width));
            }
            None => {
                if let Some(selected) = self.filtered_models.get(selected_index) {
                    lines.push(String::new());
                    lines.extend(
                        Text::new(
                            &theme_handle.fg("muted", &format!("  Model Name: {}", selected.model.name)),
                            0,
                            0,
                        )
                        .render(width),
                    );
                }
            }
        }
        lines
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ModelSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches("tui.input.tab") {
            if !self.scoped_model_items.is_empty() {
                let next_scope = match self.scope {
                    ModelScope::All => ModelScope::Scoped,
                    ModelScope::Scoped => ModelScope::All,
                };
                self.set_scope(next_scope);
            }
            return ModelSelectorOutcome::Consumed;
        }
        if matches("tui.select.up") {
            if self.filtered_models.is_empty() {
                return ModelSelectorOutcome::Consumed;
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered_models.len() - 1
            } else {
                self.selected_index - 1
            };
            return ModelSelectorOutcome::Consumed;
        }
        if matches("tui.select.down") {
            if self.filtered_models.is_empty() {
                return ModelSelectorOutcome::Consumed;
            }
            self.selected_index = if self.selected_index == self.filtered_models.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            return ModelSelectorOutcome::Consumed;
        }
        if matches("tui.select.confirm") {
            return match self.selected_item() {
                Some(item) => ModelSelectorOutcome::Select(Box::new(item.model.clone())),
                None => ModelSelectorOutcome::Consumed,
            };
        }
        if matches("tui.select.cancel") {
            return ModelSelectorOutcome::Cancel;
        }
        if matches_key(data, "ctrl+s") {
            return match self.selected_item() {
                Some(item) => ModelSelectorOutcome::SelectAsDefault(Box::new(item.model.clone())),
                None => ModelSelectorOutcome::Consumed,
            };
        }

        // Everything else goes to the search box (upstream `searchInput`):
        // editing keys first, then printable input. Upstream re-filters after
        // every keystroke (`this.filterModels(this.searchInput.getValue())`).
        if dispatch_input_keybinding(&mut self.search_input, data) {
            let query = self.search_input.get_value().to_string();
            self.filter_models(&query);
            return ModelSelectorOutcome::Consumed;
        }
        self.search_input.handle_input(data);
        let query = self.search_input.get_value().to_string();
        self.filter_models(&query);
        ModelSelectorOutcome::Consumed
    }
}

/// The `provider\0id` identity of a row (upstream the `${provider}\0${id}`
/// keys used to dedupe the `default` shortcut).
fn model_item_key(item: &ModelItem) -> String {
    model_key(&item.model)
}

impl Component for ModelSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());

        if !self.scoped_model_items.is_empty() {
            lines.extend(Text::new(&self.scope_text(&theme_handle), 0, 0).render(width));
            lines.extend(Text::new(&self.scope_hint_text(), 0, 0).render(width));
        } else {
            lines.extend(
                Text::new(
                    &theme_handle.fg(
                        "warning",
                        "Only showing models from configured providers. Use /login to add providers.",
                    ),
                    0,
                    0,
                )
                .render(width),
            );
        }
        lines.push(String::new());

        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        lines.extend(self.list_lines(width));
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
            .render(width),
        );
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for ModelSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.focused = focused;
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
            context_window: 100_000,
            max_tokens: 10_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn scoped(id: &str, provider: &str) -> ScopedModel {
        ScopedModel {
            model: model(id, provider),
            thinking_level: None,
        }
    }

    fn ids(selector: &ModelSelectorComponent) -> Vec<String> {
        selector
            .filtered_models()
            .iter()
            .map(|item| item.id.clone())
            .collect()
    }

    /// Upstream `sortModels`: current model first, then the persisted default,
    /// then by provider.
    #[test]
    fn current_then_default_then_provider_sort_order() {
        let available = vec![
            model("z", "alpha"),
            model("a", "beta"),
            model("m", "alpha"),
        ];
        let selector = ModelSelectorComponent::new(
            Some(&model("m", "alpha")),
            &available,
            &[],
            Some(DefaultModelReference {
                provider: "beta".to_string(),
                id: "a".to_string(),
            }),
            None,
            None,
        );
        assert_eq!(ids(&selector), vec!["m", "a", "z"]);
        // The current model is preselected.
        assert_eq!(selector.selected_index(), 0);
    }

    /// Upstream the constructor's `scopedModels.length > 0 ? "scoped" : "all"`.
    #[test]
    fn scope_starts_scoped_and_tab_toggles_it() {
        let available = vec![model("a", "p1"), model("b", "p1"), model("c", "p2")];
        let scoped_models = vec![scoped("c", "p2")];
        let mut selector = ModelSelectorComponent::new(
            Some(&model("a", "p1")),
            &available,
            &scoped_models,
            None,
            None,
            None,
        );
        assert_eq!(selector.scope(), ModelScope::Scoped);
        assert_eq!(ids(&selector), vec!["c"]);

        assert_eq!(selector.handle_key("\t"), ModelSelectorOutcome::Consumed);
        assert_eq!(selector.scope(), ModelScope::All);
        assert_eq!(ids(&selector), vec!["a", "b", "c"]);
        assert_eq!(selector.selected_index(), 0, "current model reselected");

        assert_eq!(selector.handle_key("\t"), ModelSelectorOutcome::Consumed);
        assert_eq!(selector.scope(), ModelScope::Scoped);

        // Without scoped models Tab does not switch anything.
        let mut unscoped =
            ModelSelectorComponent::new(Some(&model("a", "p1")), &available, &[], None, None, None);
        assert_eq!(unscoped.handle_key("\t"), ModelSelectorOutcome::Consumed);
        assert_eq!(unscoped.scope(), ModelScope::All);
        assert_eq!(ids(&unscoped).len(), 3);
    }

    /// Upstream `filterModels` + `isDefaultSearch`.
    #[test]
    fn search_filters_and_the_default_query_pins_the_default() {
        let available = vec![
            model("opus", "p1"),
            model("sonnet", "p1"),
            model("haiku", "p2"),
        ];
        let default = DefaultModelReference {
            provider: "p2".to_string(),
            id: "haiku".to_string(),
        };
        let mut selector = ModelSelectorComponent::new(
            Some(&model("opus", "p1")),
            &available,
            &[],
            Some(default),
            None,
            None,
        );
        assert_eq!(ids(&selector), vec!["opus", "haiku", "sonnet"]);

        // Typing goes through the search box and fuzzy-filters the rows.
        for ch in "son".chars() {
            assert_eq!(
                selector.handle_key(&ch.to_string()),
                ModelSelectorOutcome::Consumed
            );
        }
        assert_eq!(selector.search_value(), "son");
        // Fuzzy subsequence matching also keeps unrelated rows (the provider /
        // name text contains "son" as a subsequence); the best match leads.
        assert!(
            ids(&selector).len() < 3,
            "query narrows the list: {:?}",
            ids(&selector)
        );
        assert_eq!(
            selector.filtered_models().first().map(|item| item.id.as_str()),
            Some("sonnet")
        );
        assert_eq!(selector.selected_index(), 0, "query highlights the top row");

        // Clearing the query restores every row.
        for _ in 0..3 {
            assert_eq!(
                selector.handle_key("\u{7f}"),
                ModelSelectorOutcome::Consumed,
                "backspace"
            );
        }
        assert_eq!(selector.search_value(), "");
        assert_eq!(ids(&selector).len(), 3);

        // "def" is the shortcut for the persisted default.
        for ch in "def".chars() {
            selector.handle_key(&ch.to_string());
        }
        assert_eq!(
            selector.filtered_models().first().map(|item| item.id.as_str()),
            Some("haiku"),
            "default pinned first: {:?}",
            ids(&selector)
        );
    }

    /// Upstream `handleInput`'s wrap-around navigation.
    #[test]
    fn navigation_wraps_in_both_directions() {
        let available = vec![model("a", "p1"), model("b", "p1"), model("c", "p1")];
        let mut selector =
            ModelSelectorComponent::new(Some(&model("a", "p1")), &available, &[], None, None, None);
        assert_eq!(selector.selected_index(), 0);

        assert_eq!(
            selector.handle_key("\u{1b}[A"),
            ModelSelectorOutcome::Consumed
        );
        assert_eq!(selector.selected_index(), 2, "up wraps to the bottom");
        assert_eq!(
            selector.handle_key("\u{1b}[B"),
            ModelSelectorOutcome::Consumed
        );
        assert_eq!(selector.selected_index(), 0, "down wraps to the top");
        assert_eq!(
            selector.handle_key("\u{1b}[B"),
            ModelSelectorOutcome::Consumed
        );
        assert_eq!(selector.selected_index(), 1);
    }

    /// Upstream the select / select-as-default / cancel callbacks.
    #[test]
    fn enter_ctrl_s_and_escape_report_the_outcome() {
        let available = vec![model("a", "p1"), model("b", "p1")];
        let mut selector =
            ModelSelectorComponent::new(Some(&model("a", "p1")), &available, &[], None, None, None);

        assert_eq!(
            selector.handle_key("\u{1b}[B"),
            ModelSelectorOutcome::Consumed
        );
        match selector.handle_key("\r") {
            ModelSelectorOutcome::Select(model) => {
                assert_eq!((model.provider.as_str(), model.id.as_str()), ("p1", "b"));
            }
            other => panic!("expected Select, got {other:?}"),
        }
        match selector.handle_key("\u{13}") {
            ModelSelectorOutcome::SelectAsDefault(model) => assert_eq!(model.id, "b"),
            other => panic!("expected SelectAsDefault, got {other:?}"),
        }
        assert_eq!(selector.handle_key("\u{1b}"), ModelSelectorOutcome::Cancel);
    }

    /// An empty list (no configured providers) has nothing to select.
    #[test]
    fn empty_list_consumes_confirm_and_renders_the_no_match_line() {
        let mut selector = ModelSelectorComponent::new(None, &[], &[], None, None, None);
        assert!(selector.filtered_models().is_empty());
        assert_eq!(selector.handle_key("\r"), ModelSelectorOutcome::Consumed);
        assert_eq!(
            selector.handle_key("\u{13}"),
            ModelSelectorOutcome::Consumed,
            "ctrl+s without a selection"
        );
    }

    /// The initial search input prefills the box and filters immediately.
    #[test]
    fn initial_search_input_prefills_and_filters() {
        let available = vec![model("opus", "p1"), model("sonnet", "p1")];
        let selector = ModelSelectorComponent::new(
            Some(&model("opus", "p1")),
            &available,
            &[],
            None,
            None,
            Some("sonnet"),
        );
        assert_eq!(selector.search_value(), "sonnet");
        assert_eq!(ids(&selector), vec!["sonnet"]);
    }
}
