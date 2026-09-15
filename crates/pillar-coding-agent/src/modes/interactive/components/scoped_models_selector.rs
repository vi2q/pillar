//! Port of packages/coding-agent/src/modes/interactive/components/
//! scoped-models-selector.ts (pi v0.84.3): searchable model list for
//! enabling/disabling the models Ctrl+P cycles through (`/scoped-models`).
//!
//! The enabled set is session-only until Ctrl+S persists it to settings.
//! `EnabledIds` semantics follow upstream: `None` = all enabled (no filter),
//! `Some(ids)` = the explicit ordered list.
//!
//! divergence: pillar-tui's `Input` keeps keybinding dispatch host-side, so
//! [`ScopedModelsSelectorComponent::handle_key`] answers what the host must
//! do (apply the new enabled set / persist / cancel) instead of invoking the
//! `onChange` / `onPersist` / `onCancel` callbacks. Rendering composes the
//! same lines as upstream's container children.

use std::collections::HashMap;

use pillar_ai::types::Model;
use pillar_tui::components::Text;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::keys::matches_key;
use pillar_tui::tui::{Component, Focusable};

use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_text;
use crate::modes::interactive::model_search::{ModelSearchItem, model_search_text};
use crate::modes::interactive::theme::theme;

/// Upstream `EnabledIds`: `None` = all enabled (no filter).
type EnabledIds = Option<Vec<String>>;

/// Upstream `isEnabled`.
fn is_enabled(enabled_ids: &EnabledIds, id: &str) -> bool {
    match enabled_ids {
        None => true,
        Some(ids) => ids.iter().any(|enabled| enabled == id),
    }
}

/// Upstream `toggle`.
fn toggle(enabled_ids: EnabledIds, id: &str) -> EnabledIds {
    match enabled_ids {
        // First toggle: start with only this one.
        None => Some(vec![id.to_string()]),
        Some(mut ids) => {
            if let Some(index) = ids.iter().position(|enabled| enabled == id) {
                ids.remove(index);
            } else {
                ids.push(id.to_string());
            }
            Some(ids)
        }
    }
}

/// Upstream `enableAll`.
fn enable_all(
    enabled_ids: EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    match enabled_ids {
        // Already all enabled.
        None => None,
        Some(ids) => {
            let targets: Vec<String> = match target_ids {
                Some(targets) => targets.to_vec(),
                None => all_ids.to_vec(),
            };
            let mut result = ids;
            for id in targets {
                if !result.contains(&id) {
                    result.push(id);
                }
            }
            let all_enabled = all_ids
                .iter()
                .all(|id| result.iter().any(|enabled| enabled == id));
            if all_enabled { None } else { Some(result) }
        }
    }
}

/// Upstream `clearAll`.
fn clear_all(
    enabled_ids: EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    match enabled_ids {
        None => match target_ids {
            Some(targets) => Some(
                all_ids
                    .iter()
                    .filter(|id| !targets.contains(*id))
                    .cloned()
                    .collect(),
            ),
            None => Some(Vec::new()),
        },
        Some(ids) => {
            let targets: std::collections::HashSet<String> = match target_ids {
                Some(targets) => targets.iter().cloned().collect(),
                None => ids.iter().cloned().collect(),
            };
            Some(ids.into_iter().filter(|id| !targets.contains(id)).collect())
        }
    }
}

/// Upstream `move`: swap `id` with its neighbour `delta` away in the enabled
/// list.
fn move_enabled(enabled_ids: EnabledIds, id: &str, delta: i64) -> EnabledIds {
    let mut ids = enabled_ids?;
    let Some(index) = ids.iter().position(|enabled| enabled == id) else {
        return Some(ids);
    };
    let new_index = index as i64 + delta;
    if new_index < 0 || new_index as usize >= ids.len() {
        return Some(ids);
    }
    ids.swap(index, new_index as usize);
    Some(ids)
}

/// Upstream `getSortedIds`: enabled ids first (their order is the cycle
/// order), then the rest of the available ids.
fn get_sorted_ids(enabled_ids: &EnabledIds, all_ids: &[String]) -> Vec<String> {
    match enabled_ids {
        None => all_ids.to_vec(),
        Some(ids) => {
            let mut sorted = ids.clone();
            for id in all_ids {
                if !ids.contains(id) {
                    sorted.push(id.clone());
                }
            }
            sorted
        }
    }
}

/// One filterable row (upstream `ModelItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelItem {
    pub full_id: String,
    /// `None` for configured patterns that match no available model
    /// (upstream `model: undefined`, rendered as `unavailable`).
    pub model: Option<Model>,
    pub enabled: bool,
}

/// What the host must do after a key (upstream `ModelsCallbacks`).
#[derive(Debug, Clone, PartialEq)]
pub enum ScopedModelsOutcome {
    /// Handled inside the selector (navigation or search input).
    Consumed,
    /// The enabled set or its order changed (upstream `onChange`; the host
    /// updates the session's cycle scope — session-only, no persist).
    Change(EnabledIds),
    /// Ctrl+S: persist the current selection to settings (upstream
    /// `onPersist`).
    Persist(EnabledIds),
    /// Escape / Ctrl+C with an empty search (upstream `onCancel`).
    Cancel,
}

/// Searchable model enable/reorder selector (upstream
/// `ScopedModelsSelectorComponent`).
pub struct ScopedModelsSelectorComponent {
    models_by_id: HashMap<String, Model>,
    all_ids: Vec<String>,
    enabled_ids: EnabledIds,
    filtered_items: Vec<ModelItem>,
    selected_index: usize,
    search_input: Input,
    is_dirty: bool,
    focused: bool,
    /// Upstream `maxVisible`.
    max_visible: usize,
}

impl ScopedModelsSelectorComponent {
    /// Upstream the constructor: index the models and seed the enabled set
    /// (`enabled_model_ids` already resolved by the mode).
    pub fn new(all_models: Vec<Model>, enabled_model_ids: EnabledIds) -> Self {
        let mut models_by_id = HashMap::new();
        let mut all_ids = Vec::new();
        for model in all_models {
            let full_id = format!("{}/{}", model.provider, model.id);
            all_ids.push(full_id.clone());
            models_by_id.insert(full_id, model);
        }
        let mut selector = Self {
            models_by_id,
            all_ids,
            enabled_ids: enabled_model_ids,
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            is_dirty: false,
            focused: false,
            max_visible: 8,
        };
        selector.filtered_items = selector.build_items();
        selector
    }

    /// The enabled set (upstream `enabledIds`), for mode-level assertions.
    pub fn enabled_ids(&self) -> &EnabledIds {
        &self.enabled_ids
    }

    /// The highlighted row (upstream `filteredItems[selectedIndex]`).
    pub fn selected_item(&self) -> Option<&ModelItem> {
        self.filtered_items.get(self.selected_index)
    }

    /// Whether a change has not been persisted yet (upstream `isDirty`).
    pub fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    /// The search box contents.
    pub fn search_value(&self) -> String {
        self.search_input.get_value().to_string()
    }

    fn build_items(&self) -> Vec<ModelItem> {
        get_sorted_ids(&self.enabled_ids, &self.all_ids)
            .into_iter()
            .map(|id| {
                let model = self.models_by_id.get(&id).cloned();
                ModelItem {
                    enabled: is_enabled(&self.enabled_ids, &id),
                    model,
                    full_id: id,
                }
            })
            .collect()
    }

    /// Upstream `getFooterText`: the key hints plus the enabled count
    /// (unavailable configured ids count separately).
    fn footer_text(&self) -> String {
        let enabled_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| self.models_by_id.contains_key(*id))
                .count(),
            None => self.all_ids.len(),
        };
        let unavailable_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| !self.models_by_id.contains_key(*id))
                .count(),
            None => 0,
        };
        let all_enabled = self.enabled_ids.is_none();
        let count_text = if all_enabled {
            "all enabled".to_string()
        } else {
            let unavailable = if unavailable_count > 0 {
                format!(" · {unavailable_count} unavailable")
            } else {
                String::new()
            };
            format!(
                "{enabled_count}/{} enabled{unavailable}",
                self.all_ids.len()
            )
        };
        let parts = [
            format!("{} toggle", key_text("tui.select.confirm")),
            format!("{} all", key_text("app.models.enableAll")),
            format!("{} clear", key_text("app.models.clearAll")),
            format!("{} provider", key_text("app.models.toggleProvider")),
            format!(
                "{}/{} reorder",
                key_text("app.models.reorderUp"),
                key_text("app.models.reorderDown")
            ),
            format!("{} save", key_text("app.models.save")),
            count_text,
        ];
        let theme_handle = theme();
        let body = format!("  {} ", parts.join(" · "));
        if self.is_dirty {
            format!(
                "{}{}",
                theme_handle.fg("dim", &body),
                theme_handle.fg("warning", "(unsaved)")
            )
        } else {
            theme_handle.fg("dim", &body)
        }
    }

    /// Upstream `refresh`: re-filter and clamp the selection.
    fn refresh(&mut self) {
        let query = self.search_input.get_value().to_string();
        let items = self.build_items();
        self.filtered_items = if query.is_empty() {
            items
        } else {
            pillar_tui::fuzzy::fuzzy_filter_by(&items, &query, |item: &ModelItem| {
                match &item.model {
                    Some(model) => model_search_text(&ModelSearchItem {
                        id: &model.id,
                        provider: &model.provider,
                        name: Some(&model.name),
                    }),
                    None => item.full_id.clone(),
                }
            })
            .into_iter()
            .map(|item| (*item).clone())
            .collect()
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_items.len().saturating_sub(1));
    }

    /// Upstream the scroll window in `updateList`.
    fn window_start(&self) -> usize {
        if self.filtered_items.is_empty() {
            return 0;
        }
        let total = self.filtered_items.len();
        let max_visible = self.max_visible;
        (self.selected_index.saturating_sub(max_visible / 2)).min(total.saturating_sub(max_visible))
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> ScopedModelsOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        // Navigation
        if matches("tui.select.up") {
            if self.filtered_items.is_empty() {
                return ScopedModelsOutcome::Consumed;
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered_items.len() - 1
            } else {
                self.selected_index - 1
            };
            return ScopedModelsOutcome::Consumed;
        }
        if matches("tui.select.down") {
            if self.filtered_items.is_empty() {
                return ScopedModelsOutcome::Consumed;
            }
            self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            return ScopedModelsOutcome::Consumed;
        }

        // Reorder enabled models (Alt+Up / Alt+Down).
        let reorder_up = matches("app.models.reorderUp");
        let reorder_down = matches("app.models.reorderDown");
        if reorder_up || reorder_down {
            let Some(ids) = &self.enabled_ids else {
                return ScopedModelsOutcome::Consumed;
            };
            let Some(item) = self.selected_item() else {
                return ScopedModelsOutcome::Consumed;
            };
            if is_enabled(&self.enabled_ids, &item.full_id) {
                let delta: i64 = if reorder_up { -1 } else { 1 };
                let current = ids.iter().position(|id| id == &item.full_id);
                if let Some(current) = current {
                    let new_index = current as i64 + delta;
                    if new_index >= 0 && (new_index as usize) < ids.len() {
                        self.enabled_ids =
                            move_enabled(self.enabled_ids.clone(), &item.full_id, delta);
                        self.is_dirty = true;
                        self.selected_index = (self.selected_index as i64 + delta) as usize;
                        self.refresh();
                        return ScopedModelsOutcome::Change(self.enabled_ids.clone());
                    }
                }
            }
            return ScopedModelsOutcome::Consumed;
        }

        // Toggle on Enter.
        if matches("tui.select.confirm") {
            if let Some(item) = self.selected_item() {
                self.enabled_ids = toggle(self.enabled_ids.clone(), &item.full_id);
                self.is_dirty = true;
                self.refresh();
                return ScopedModelsOutcome::Change(self.enabled_ids.clone());
            }
            return ScopedModelsOutcome::Consumed;
        }

        // Enable all (filtered if search active, otherwise all).
        if matches("app.models.enableAll") {
            let target_ids = self.filtered_target_ids();
            self.enabled_ids = enable_all(
                self.enabled_ids.clone(),
                &self.all_ids,
                target_ids.as_deref(),
            );
            self.is_dirty = true;
            self.refresh();
            return ScopedModelsOutcome::Change(self.enabled_ids.clone());
        }

        // Clear all (filtered if search active, otherwise all).
        if matches("app.models.clearAll") {
            let target_ids = self.filtered_target_ids();
            self.enabled_ids = clear_all(
                self.enabled_ids.clone(),
                &self.all_ids,
                target_ids.as_deref(),
            );
            self.is_dirty = true;
            self.refresh();
            return ScopedModelsOutcome::Change(self.enabled_ids.clone());
        }

        // Toggle provider of current item.
        if matches("app.models.toggleProvider") {
            if let Some(provider) = self
                .selected_item()
                .and_then(|item| item.model.as_ref())
                .map(|model| model.provider.clone())
            {
                let provider_ids: Vec<String> = self
                    .all_ids
                    .iter()
                    .filter(|id| {
                        self.models_by_id
                            .get(*id)
                            .is_some_and(|model| model.provider == provider)
                    })
                    .cloned()
                    .collect();
                let all_enabled = provider_ids
                    .iter()
                    .all(|id| is_enabled(&self.enabled_ids, id));
                self.enabled_ids = if all_enabled {
                    clear_all(self.enabled_ids.clone(), &self.all_ids, Some(&provider_ids))
                } else {
                    enable_all(self.enabled_ids.clone(), &self.all_ids, Some(&provider_ids))
                };
                self.is_dirty = true;
                self.refresh();
                return ScopedModelsOutcome::Change(self.enabled_ids.clone());
            }
            return ScopedModelsOutcome::Consumed;
        }

        // Save/persist to settings. Upstream calls `onPersist` and then
        // clears the unsaved marker.
        if matches("app.models.save") {
            self.is_dirty = false;
            return ScopedModelsOutcome::Persist(self.enabled_ids.clone());
        }

        // Ctrl+C: clear search or cancel if empty.
        if matches_key(data, "ctrl+c") {
            if !self.search_input.get_value().is_empty() {
                self.search_input.set_value("");
                self.refresh();
                return ScopedModelsOutcome::Consumed;
            }
            return ScopedModelsOutcome::Cancel;
        }

        // Escape: cancel.
        if matches("tui.select.cancel") {
            return ScopedModelsOutcome::Cancel;
        }

        // Pass everything else to the search input.
        if dispatch_input_keybinding(&mut self.search_input, data) {
            self.refresh();
            return ScopedModelsOutcome::Consumed;
        }
        self.search_input.handle_input(data);
        self.refresh();
        ScopedModelsOutcome::Consumed
    }

    /// Upstream the `enableAll` / `clearAll` targets: the filtered ids when a
    /// search is active, else `undefined` (everything).
    fn filtered_target_ids(&self) -> Option<Vec<String>> {
        if self.search_input.get_value().is_empty() {
            None
        } else {
            Some(
                self.filtered_items
                    .iter()
                    .map(|item| item.full_id.clone())
                    .collect(),
            )
        }
    }
}

impl Component for ScopedModelsSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());
        lines.extend(
            Text::new(
                &theme_handle.fg("accent", &theme_handle.bold("Model Configuration")),
                0,
                0,
            )
            .render(width),
        );
        lines.extend(
            Text::new(
                &theme_handle.fg(
                    "muted",
                    &format!(
                        "Session-only. {} to save to settings.",
                        key_text("app.models.save")
                    ),
                ),
                0,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        // Upstream `updateList` into `listContainer`.
        if self.filtered_items.is_empty() {
            lines.extend(
                Text::new(&theme_handle.fg("muted", "  No matching models"), 0, 0).render(width),
            );
        } else {
            let start = self.window_start();
            let end = (start + self.max_visible).min(self.filtered_items.len());
            let all_enabled = self.enabled_ids.is_none();
            for (offset, item) in self.filtered_items[start..end].iter().enumerate() {
                let index = start + offset;
                let is_selected = index == self.selected_index;
                let prefix = if is_selected { "→ " } else { "  " };
                let id = item
                    .model
                    .as_ref()
                    .map(|model| model.id.clone())
                    .unwrap_or_else(|| item.full_id.clone());
                let model_text = if is_selected {
                    theme_handle.fg("accent", &id)
                } else {
                    id
                };
                let provider_label = match &item.model {
                    Some(model) => format!(" [{}]", model.provider),
                    None => " [unavailable]".to_string(),
                };
                let provider_badge = theme_handle.fg("muted", &provider_label);
                let status = match &item.model {
                    Some(_) => {
                        if all_enabled {
                            String::new()
                        } else if item.enabled {
                            theme_handle.fg("success", " ✓")
                        } else {
                            theme_handle.fg("dim", " ✗")
                        }
                    }
                    None => theme_handle.fg("dim", " ✗"),
                };
                let text = format!("{prefix}{model_text}{provider_badge}{status}");
                lines.extend(Text::new(&text, 0, 0).render(width));
            }

            if start > 0 || end < self.filtered_items.len() {
                lines.extend(
                    Text::new(
                        &theme_handle.fg(
                            "muted",
                            &format!(
                                "  ({}/{})",
                                self.selected_index + 1,
                                self.filtered_items.len()
                            ),
                        ),
                        0,
                        0,
                    )
                    .render(width),
                );
            }

            lines.push(String::new());
            lines.extend(
                Text::new(
                    &theme_handle.fg(
                        "muted",
                        &format!(
                            "  {}",
                            match self.selected_item().and_then(|item| item.model.as_ref()) {
                                Some(model) => format!("Model Name: {}", model.name),
                                None => "Model unavailable".to_string(),
                            }
                        ),
                    ),
                    0,
                    0,
                )
                .render(width),
            );
        }

        lines.push(String::new());
        lines.extend(Text::new(&self.footer_text(), 0, 0).render(width));
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for ScopedModelsSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        // Upstream propagates to `searchInput` for IME cursor positioning.
        self.search_input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Component tests render (footer / list) and match the app-level
    /// `app.models.*` keybindings, so the merged keybinding table and a theme
    /// must be installed. The merged table is a superset of the TUI table the
    /// global registry lazily installs, so the parallel tests that only use
    /// `tui.*` keys are unaffected.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::modes::interactive::theme::init_theme(Some("dark"));
        let definitions = crate::core::keybindings::keybindings("darwin", &Default::default());
        pillar_tui::keybindings::set_keybindings(pillar_tui::keybindings::KeybindingsManager::new(
            definitions,
            Default::default(),
        ));
        guard
    }

    fn model(id: &str, provider: &str) -> Model {
        named_model(id, provider, &format!("{id} name"))
    }

    fn named_model(id: &str, provider: &str, name: &str) -> Model {
        Model {
            id: id.to_string(),
            name: name.to_string(),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 10_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn models() -> Vec<Model> {
        vec![model("m1", "p1"), model("m2", "p1"), model("m3", "p2")]
    }

    #[test]
    fn starts_all_enabled_and_orders_enabled_ids_first() {
        let _guard = setup();
        let selector = ScopedModelsSelectorComponent::new(models(), None);
        assert_eq!(selector.enabled_ids(), &None, "null = all enabled");
        let ids: Vec<&str> = selector
            .filtered_items
            .iter()
            .map(|item| item.full_id.as_str())
            .collect();
        assert_eq!(ids, vec!["p1/m1", "p1/m2", "p2/m3"]);

        // Explicit enabled ids come first (in their own order), the rest
        // follow.
        let selector = ScopedModelsSelectorComponent::new(
            models(),
            Some(vec!["p2/m3".to_string(), "p1/m1".to_string()]),
        );
        let ids: Vec<&str> = selector
            .filtered_items
            .iter()
            .map(|item| item.full_id.as_str())
            .collect();
        assert_eq!(ids, vec!["p2/m3", "p1/m1", "p1/m2"]);
    }

    #[test]
    fn enter_toggles_and_the_footer_counts() {
        let _guard = setup();
        let mut selector = ScopedModelsSelectorComponent::new(models(), None);
        // Enter on an all-enabled list starts an explicit list with only the
        // selected one (upstream `toggle(null, id)`).
        assert_eq!(
            selector.handle_key("\r"),
            ScopedModelsOutcome::Change(Some(vec!["p1/m1".to_string()])),
        );
        assert!(selector.is_dirty());
        assert!(selector.footer_text().contains("1/3 enabled"));
        assert!(selector.footer_text().contains("(unsaved)"));

        // Toggle the next one on.
        selector.handle_key("\u{1b}[B"); // down
        assert_eq!(
            selector.handle_key("\r"),
            ScopedModelsOutcome::Change(Some(vec!["p1/m1".to_string(), "p1/m2".to_string()])),
        );
        // Toggle it off again.
        selector.handle_key("\r");
        assert_eq!(selector.enabled_ids(), &Some(vec!["p1/m1".to_string()]));

        // Enabling everything again (via Ctrl+A over no query) returns to
        // null — plain toggles only ever produce explicit lists. Ctrl+A is
        // itself a change, so the unsaved marker stays.
        selector.handle_key("\u{1}"); // Ctrl+A
        assert_eq!(selector.enabled_ids(), &None);
        assert!(selector.footer_text().contains("all enabled"));
        assert!(selector.footer_text().contains("(unsaved)"));
        assert!(selector.is_dirty());
    }

    #[test]
    fn search_filters_and_enable_all_scopes_to_the_filtered_ids() {
        let _guard = setup();
        // Distinct names so a query can isolate one model.
        let models = vec![
            named_model("m1", "p1", "alpha"),
            named_model("m2", "p1", "beta"),
            named_model("m3", "p2", "gamma"),
        ];
        let mut selector =
            ScopedModelsSelectorComponent::new(models, Some(vec!["p1/m1".to_string()]));

        // Typing filters the list to the fuzzy match.
        for ch in "bet".chars() {
            selector.handle_key(&ch.to_string());
        }
        assert_eq!(selector.filtered_items.len(), 1);
        assert_eq!(selector.search_value(), "bet");

        // Ctrl+A enables just the filtered id.
        assert_eq!(
            selector.handle_key("\u{1}"), // Ctrl+A
            ScopedModelsOutcome::Change(Some(vec!["p1/m1".to_string(), "p1/m2".to_string()])),
        );

        // Ctrl+X clears the filtered target only.
        assert_eq!(
            selector.handle_key("\u{18}"), // Ctrl+X
            ScopedModelsOutcome::Change(Some(vec!["p1/m1".to_string()])),
        );

        // Without a query the targets are everything.
        selector.search_input.set_value("");
        selector.refresh();
        assert_eq!(
            selector.handle_key("\u{18}"), // Ctrl+X
            ScopedModelsOutcome::Change(Some(Vec::new())),
        );
        // And Ctrl+A from the empty list re-enables everything (→ null).
        assert_eq!(
            selector.handle_key("\u{1}"),
            ScopedModelsOutcome::Change(None)
        );
    }

    #[test]
    fn ctrl_p_toggles_the_whole_provider_of_the_selected_model() {
        let _guard = setup();
        let mut selector = ScopedModelsSelectorComponent::new(models(), None);
        // Down to p1/m2, then Ctrl+P disables all of p1.
        selector.handle_key("\u{1b}[B");
        assert_eq!(
            selector.handle_key("\u{10}"), // Ctrl+P
            ScopedModelsOutcome::Change(Some(vec!["p2/m3".to_string()])),
        );
        // Ctrl+P again re-enables the provider (→ null = all).
        assert_eq!(
            selector.handle_key("\u{10}"),
            ScopedModelsOutcome::Change(None)
        );
    }

    #[test]
    fn alt_up_and_alt_down_reorder_enabled_models() {
        let _guard = setup();
        let mut selector = ScopedModelsSelectorComponent::new(
            models(),
            Some(vec!["p1/m1".to_string(), "p2/m3".to_string()]),
        );
        // p1/m1 is selected first; it is already first, so Alt+Up stays.
        assert_eq!(
            selector.handle_key("\u{1b}[1;3A"), // Alt+Up (kitty press form)
            ScopedModelsOutcome::Consumed,
        );
        // Alt+Down swaps it with p2/m3.
        assert_eq!(
            selector.handle_key("\u{1b}[1;3B"), // Alt+Down
            ScopedModelsOutcome::Change(Some(vec!["p2/m3".to_string(), "p1/m1".to_string()])),
        );
        // The selection follows the moved item.
        assert_eq!(
            selector.selected_item().map(|item| item.full_id.as_str()),
            Some("p1/m1")
        );

        // Reordering is a no-op when everything is enabled (null list).
        let mut all = ScopedModelsSelectorComponent::new(models(), None);
        assert_eq!(
            all.handle_key("\u{1b}[1;3B"),
            ScopedModelsOutcome::Consumed,
            "null = no order to change"
        );
    }

    #[test]
    fn ctrl_s_persists_and_escape_and_ctrl_c_cancel() {
        let _guard = setup();
        let mut selector =
            ScopedModelsSelectorComponent::new(models(), Some(vec!["p1/m1".to_string()]));

        assert_eq!(
            selector.handle_key("\u{13}"), // Ctrl+S
            ScopedModelsOutcome::Persist(Some(vec!["p1/m1".to_string()])),
        );
        assert!(!selector.is_dirty(), "persist clears the unsaved marker");

        // Ctrl+C with a query clears the search first.
        selector.handle_key("m");
        assert_eq!(selector.handle_key("\u{3}"), ScopedModelsOutcome::Consumed);
        assert_eq!(selector.search_value(), "");
        assert_eq!(selector.handle_key("\u{3}"), ScopedModelsOutcome::Cancel);

        let mut selector = ScopedModelsSelectorComponent::new(models(), None);
        assert_eq!(selector.handle_key("\u{1b}"), ScopedModelsOutcome::Cancel);
    }

    #[test]
    fn unavailable_enabled_ids_render_and_count() {
        let _guard = setup();
        // A configured pattern that matches nothing (upstream the no-match
        // diagnostic ids) stays in the list as `unavailable`.
        let mut selector = ScopedModelsSelectorComponent::new(
            models(),
            Some(vec!["p1/m1".to_string(), "ghost/none".to_string()]),
        );
        let ids: Vec<&str> = selector
            .filtered_items
            .iter()
            .map(|item| item.full_id.as_str())
            .collect();
        assert_eq!(ids, vec!["p1/m1", "ghost/none", "p1/m2", "p2/m3"]);
        let body = strip_ansi_vec(selector.render(60)).join("\n");
        assert!(body.contains("[unavailable]"), "{body:?}");
        assert!(
            selector
                .footer_text()
                .contains("1/3 enabled · 1 unavailable")
        );

        // The highlighted unavailable row renders the fallback info line.
        selector.handle_key("\u{1b}[B"); // ghost/none
        let body = strip_ansi_vec(selector.render(60)).join("\n");
        assert!(body.contains("Model unavailable"), "{body:?}");

        // Toggling the highlighted unavailable id still tracks it (the
        // selection is still on it).
        assert_eq!(
            selector.handle_key("\r"),
            ScopedModelsOutcome::Change(Some(vec!["p1/m1".to_string()])),
        );
    }

    fn strip_ansi_vec(lines: Vec<String>) -> Vec<String> {
        lines
            .iter()
            .map(|line| pillar_tui::text_utils::strip_terminal_sequences(line))
            .collect()
    }
}
