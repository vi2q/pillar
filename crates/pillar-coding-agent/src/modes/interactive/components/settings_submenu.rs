//! Port of packages/coding-agent/src/modes/interactive/components/
//! settings-submenu.ts (pi v0.84.3): the reusable one-step and N-step
//! submenus the settings selector is built from.
//!
//! divergence: pillar-tui keeps keybinding dispatch host-side, so both
//! submenus answer outcomes ([`SelectSubmenuOutcome`] /
//! [`SteppedSubmenuOutcome`]) instead of invoking `onSelect` / `onCancel` /
//! `onSelectionChange` callbacks, and the steps / options are built with
//! owned closures rather than re-entering the component.

use std::collections::BTreeMap;

use pillar_tui::components::Text;
use pillar_tui::fuzzy::fuzzy_filter_by;
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pillar_tui::tui::Component;

use crate::modes::interactive::theme::{get_select_list_theme, theme};

/// Upstream `SUBMENU_SELECT_LIST_LAYOUT` (the widths only; the port's
/// `SelectListLayoutOptions` also carries an uncloneable `truncate_primary`
/// hook, so steps store the plain numbers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmenuLayout {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
}

impl SubmenuLayout {
    pub const fn submenu_default() -> Self {
        Self {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(32),
        }
    }

    pub const fn model_picker() -> Self {
        Self {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(46),
        }
    }

    fn to_select_list_layout(self) -> SelectListLayoutOptions {
        SelectListLayoutOptions {
            min_primary_column_width: self.min_primary_column_width,
            max_primary_column_width: self.max_primary_column_width,
            truncate_primary: None,
        }
    }
}

fn matches(data: &str, keybinding: &str) -> bool {
    with_global_keybindings(|kb| kb.matches(data, keybinding))
}

/// What the host must do after a key in a single-step submenu (upstream the
/// `onSelect` / `onCancel` callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectSubmenuOutcome {
    /// Handled inside the submenu (navigation, filtering).
    Consumed,
    /// Enter: the chosen value.
    Select(String),
    /// Escape / select-cancel (go back one step, or close the submenu).
    Cancel,
}

/// Single-step submenu that shows a titled select list (upstream
/// `SelectSubmenu`).
pub struct SelectSubmenu {
    title: String,
    description: String,
    all_options: Vec<SelectItem>,
    list: SelectList,
    layout: SubmenuLayout,
    search_input: Option<Input>,
    searchable: bool,
}

impl SelectSubmenu {
    /// Upstream the constructor. `current_value` pre-selects the matching
    /// option.
    pub fn new(
        title: &str,
        description: &str,
        options: Vec<SelectItem>,
        current_value: &str,
        searchable: bool,
        layout: SubmenuLayout,
    ) -> Self {
        let list = build_select_list(&options, Some(current_value), layout);
        Self {
            title: title.to_string(),
            description: description.to_string(),
            all_options: options,
            list,
            layout,
            search_input: searchable.then(Input::new),
            searchable,
        }
    }

    /// The highlighted value (upstream `getSelectedItem().value`), also used
    /// for the theme preview each time the selection changes.
    pub fn selected_value(&self) -> Option<String> {
        self.list.get_selected_item().map(|item| item.value.clone())
    }

    /// The options currently shown (after fuzzy filtering).
    pub fn visible_items(&self) -> &[SelectItem] {
        self.list.filtered_items()
    }

    /// Upstream `applyFilter`: fuzzy-filter on `label + description` and
    /// rebuild the list without a preselection.
    pub fn apply_filter(&mut self, query: &str) {
        let filtered = if query.is_empty() {
            self.all_options.clone()
        } else {
            fuzzy_filter_by(&self.all_options, query, |item| match &item.description {
                Some(description) => format!("{} {description}", item.label),
                None => item.label.clone(),
            })
            .into_iter()
            .cloned()
            .collect()
        };
        self.list = build_select_list(&filtered, None, self.layout);
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> SelectSubmenuOutcome {
        if self.searchable {
            let is_nav = matches(data, "tui.select.up")
                || matches(data, "tui.select.down")
                || matches(data, "tui.select.confirm")
                || matches(data, "tui.select.cancel");
            if !is_nav {
                let Some(input) = self.search_input.as_mut() else {
                    return SelectSubmenuOutcome::Consumed;
                };
                if !dispatch_input_keybinding(input, data) {
                    input.handle_input(data);
                }
                let query = input.get_value().to_string();
                self.apply_filter(&query);
                return SelectSubmenuOutcome::Consumed;
            }
        }
        self.handle_list_key(data)
    }

    fn handle_list_key(&mut self, data: &str) -> SelectSubmenuOutcome {
        if matches(data, "tui.select.up") {
            self.list.move_up();
        } else if matches(data, "tui.select.down") {
            self.list.move_down();
        } else if matches(data, "tui.select.confirm") {
            if let Some(value) = self.selected_value() {
                return SelectSubmenuOutcome::Select(value);
            }
        } else if matches(data, "tui.select.cancel") {
            return SelectSubmenuOutcome::Cancel;
        }
        SelectSubmenuOutcome::Consumed
    }
}

/// Upstream `buildSelectList`.
fn build_select_list(
    options: &[SelectItem],
    preselect: Option<&str>,
    layout: SubmenuLayout,
) -> SelectList {
    let mut list = SelectList::new(
        options.to_vec(),
        options.len().min(10),
        layout.to_select_list_layout(),
    );
    if let Some(preselect) = preselect {
        if let Some(index) = options.iter().position(|option| option.value == preselect) {
            list.set_selected_index(index);
        }
    }
    list
}

impl Component for SelectSubmenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(
            Text::new(&theme_handle.bold(&theme_handle.fg("accent", &self.title)), 0, 0)
                .render(width),
        );
        if !self.description.is_empty() {
            lines.push(String::new());
            lines.extend(Text::new(&theme_handle.fg("muted", &self.description), 0, 0).render(width));
        }
        if let Some(input) = self.search_input.as_mut() {
            lines.push(String::new());
            lines.extend(input.render(width));
        }
        lines.push(String::new());
        lines.extend(self.list.render(width, &get_select_list_theme()));
        lines.push(String::new());
        let hint = if self.searchable {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        lines.extend(Text::new(&theme_handle.fg("dim", hint), 0, 0).render(width));
        lines
    }
}

// --- stepped submenu -------------------------------------------------------

/// The prior selections handed to a step's builders (upstream the shared
/// `context` object).
pub type StepContext = BTreeMap<String, String>;
/// Builds a step's title / description / options / pre-selection.
pub type StepTitleFn = Box<dyn Fn(&StepContext) -> String + Send>;
pub type StepDescriptionFn = Box<dyn Fn(&StepContext) -> String + Send>;
pub type StepOptionsFn = Box<dyn Fn(&StepContext) -> Vec<SelectItem> + Send>;
pub type StepPreselectFn = Box<dyn Fn(&StepContext) -> Option<String> + Send>;

/// One step in a [`SteppedSubmenu`] (upstream `SteppedSubmenuStep`).
pub struct SteppedSubmenuStep {
    /// Unique key; the selected value is stored under it.
    pub key: String,
    /// Title shown at the step top, given the prior selections.
    pub title: StepTitleFn,
    /// Description shown under the title, given the prior selections.
    pub description: StepDescriptionFn,
    /// Options built fresh each time the step is shown.
    pub options: StepOptionsFn,
    /// Optional pre-selection for the step.
    pub preselect: Option<StepPreselectFn>,
    pub searchable: bool,
    pub layout: Option<SubmenuLayout>,
}

/// What the host must do after a key in a stepped submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteppedSubmenuOutcome {
    /// Handled inside the submenu (navigation / filtering).
    Consumed,
    /// The final step completed: apply the collected context (upstream
    /// `onComplete`). With `loop`, the submenu returned to step 0 and stays
    /// open.
    Complete(BTreeMap<String, String>),
    /// Escape at step 0 (upstream `onCancel`).
    Cancel,
}

/// N-step submenu built on top of [`SelectSubmenu`] (upstream
/// `SteppedSubmenu`). Each step's options can depend on prior selections;
/// Esc goes back one step (Esc at step 0 cancels). With `loop`, completing
/// the last step reports `Complete` and returns to step 0.
pub struct SteppedSubmenu {
    steps: Vec<SteppedSubmenuStep>,
    context: StepContext,
    active: SelectSubmenu,
    step_index: usize,
    looping: bool,
}

impl SteppedSubmenu {
    pub fn new(
        steps: Vec<SteppedSubmenuStep>,
        looping: bool,
        start_at_step: Option<usize>,
        initial_context: StepContext,
    ) -> Self {
        let step_index = start_at_step.unwrap_or(0).min(steps.len().saturating_sub(1));
        let mut submenu = Self {
            steps,
            context: initial_context,
            active: SelectSubmenu::new("", "", Vec::new(), "", false, SubmenuLayout::default()),
            step_index,
            looping,
        };
        submenu.active = submenu.build_step(step_index);
        submenu
    }

    fn build_step(&self, step_index: usize) -> SelectSubmenu {
        let step = &self.steps[step_index];
        let total = self.steps.len();
        let step_label = if total > 1 {
            format!("Step {}/{total} · ", step_index + 1)
        } else {
            String::new()
        };
        let title = (step.title)(&self.context);
        let description = format!("{step_label}{}", (step.description)(&self.context));
        let options = (step.options)(&self.context);
        let preselect = step
            .preselect
            .as_ref()
            .and_then(|preselect| preselect(&self.context))
            .unwrap_or_default();
        SelectSubmenu::new(
            &title,
            &description,
            options,
            &preselect,
            step.searchable,
            step.layout.unwrap_or_default(),
        )
    }

    /// The collected selections so far (upstream the shared context).
    pub fn context(&self) -> &StepContext {
        &self.context
    }

    /// The active step's index.
    pub fn step_index(&self) -> usize {
        self.step_index
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> SteppedSubmenuOutcome {
        let value = match self.active.handle_key(data) {
            SelectSubmenuOutcome::Consumed => return SteppedSubmenuOutcome::Consumed,
            SelectSubmenuOutcome::Cancel => {
                if self.step_index > 0 {
                    self.context.remove(&self.steps[self.step_index].key);
                    self.step_index -= 1;
                    self.active = self.build_step(self.step_index);
                    return SteppedSubmenuOutcome::Consumed;
                }
                return SteppedSubmenuOutcome::Cancel;
            }
            SelectSubmenuOutcome::Select(value) => value,
        };

        let step_key = self.steps[self.step_index].key.clone();
        self.context.insert(step_key, value);

        if self.step_index + 1 < self.steps.len() {
            self.step_index += 1;
            self.active = self.build_step(self.step_index);
            return SteppedSubmenuOutcome::Consumed;
        }

        let completed = self.context.clone();
        if self.looping {
            self.context = BTreeMap::new();
            self.step_index = 0;
            self.active = self.build_step(0);
        }
        SteppedSubmenuOutcome::Complete(completed)
    }
}

impl Component for SteppedSubmenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.active.render(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_tui::text_utils::strip_terminal_sequences;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::modes::interactive::components::test_support::setup()
    }

    fn item(value: &str, description: &str) -> SelectItem {
        SelectItem {
            value: value.to_string(),
            label: value.to_string(),
            description: Some(description.to_string()),
        }
    }

    fn plain(submenu: &mut SelectSubmenu) -> String {
        submenu
            .render(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn select_submenu_preselects_navigates_and_selects() {
        let _guard = setup();
        let mut submenu = SelectSubmenu::new(
            "Theme",
            "Pick one",
            vec![item("dark", "Dark"), item("light", "Light")],
            "light",
            false,
            SubmenuLayout::submenu_default(),
        );
        let rendered = plain(&mut submenu);
        assert!(rendered.contains("Theme"), "{rendered}");
        assert!(rendered.contains("Pick one"), "{rendered}");
        assert!(rendered.contains("Enter to select · Esc to go back"), "{rendered}");
        // "light" is pre-selected.
        assert_eq!(submenu.selected_value().as_deref(), Some("light"));

        // Up wraps to the last option, Enter selects it.
        assert_eq!(
            submenu.handle_key("\x1b[A"),
            SelectSubmenuOutcome::Consumed
        );
        assert_eq!(
            submenu.handle_key("\r"),
            SelectSubmenuOutcome::Select("dark".to_string())
        );
        assert_eq!(submenu.handle_key("\x1b"), SelectSubmenuOutcome::Cancel);
    }

    #[test]
    fn select_submenu_searchable_filters_fuzzily() {
        let _guard = setup();
        let mut submenu = SelectSubmenu::new(
            "Model",
            "",
            vec![item("claude-opus-5", "Opus"), item("claude-sonnet-4-5", "Sonnet")],
            "",
            true,
            SubmenuLayout::submenu_default(),
        );
        for ch in "opus".chars() {
            submenu.handle_key(&ch.to_string());
        }
        assert_eq!(submenu.visible_items().len(), 1, "{:?}", submenu.visible_items());
        assert_eq!(
            submenu.handle_key("\r"),
            SelectSubmenuOutcome::Select("claude-opus-5".to_string())
        );
        let rendered = plain(&mut submenu);
        assert!(rendered.contains("Type to filter"), "{rendered}");
    }

    fn two_step_submenu() -> SteppedSubmenu {
        let steps = vec![
            SteppedSubmenuStep {
                key: "model".to_string(),
                title: Box::new(|_| "Per-Model Thinking Level".to_string()),
                description: Box::new(|_| "Select a model to configure".to_string()),
                options: Box::new(|_| {
                    vec![item("m/one", "one"), item("m/two", "two")]
                }),
                preselect: None,
                searchable: false,
                layout: None,
            },
            SteppedSubmenuStep {
                key: "level".to_string(),
                title: Box::new(|context| format!("Level for {}", context["model"])),
                description: Box::new(|_| "Select a level".to_string()),
                options: Box::new(|_| vec![item("high", "High"), item("low", "Low")]),
                preselect: None,
                searchable: false,
                layout: None,
            },
        ];
        SteppedSubmenu::new(steps, false, None, BTreeMap::new())
    }

    #[test]
    fn stepped_submenu_advances_and_completes() {
        let _guard = setup();
        let mut submenu = two_step_submenu();
        assert_eq!(submenu.step_index(), 0);
        let rendered = submenu
            .render(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Step 1/2 · Select a model to configure"), "{rendered}");

        // Pick the first model -> advance to step 2.
        assert_eq!(
            submenu.handle_key("\r"),
            SteppedSubmenuOutcome::Consumed
        );
        assert_eq!(submenu.step_index(), 1);
        assert_eq!(submenu.context()["model"], "m/one");
        let rendered = submenu
            .render(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Level for m/one"), "{rendered}");

        // Complete on the final step.
        assert_eq!(
            submenu.handle_key("\r"),
            SteppedSubmenuOutcome::Complete(BTreeMap::from([
                ("model".to_string(), "m/one".to_string()),
                ("level".to_string(), "high".to_string()),
            ]))
        );
    }

    #[test]
    fn stepped_submenu_escape_goes_back_one_step() {
        let _guard = setup();
        let mut submenu = two_step_submenu();
        submenu.handle_key("\r");
        assert_eq!(submenu.step_index(), 1);
        assert_eq!(
            submenu.handle_key("\x1b"),
            SteppedSubmenuOutcome::Consumed
        );
        assert_eq!(submenu.step_index(), 0);
        // Esc drops the *current* step's answer (upstream deletes
        // `steps[stepIndex].key`) and keeps the earlier ones.
        assert!(submenu.context().contains_key("model"));
        assert!(!submenu.context().contains_key("level"));
        // Esc at step 0 cancels.
        assert_eq!(submenu.handle_key("\x1b"), SteppedSubmenuOutcome::Cancel);
    }

    #[test]
    fn stepped_submenu_loop_returns_to_step_zero() {
        let _guard = setup();
        let steps = vec![SteppedSubmenuStep {
            key: "level".to_string(),
            title: Box::new(|_| "Level".to_string()),
            description: Box::new(|_| String::new()),
            options: Box::new(|_| vec![item("high", "High")]),
            preselect: None,
            searchable: false,
            layout: None,
        }];
        let mut submenu = SteppedSubmenu::new(steps, true, None, BTreeMap::new());
        assert_eq!(
            submenu.handle_key("\r"),
            SteppedSubmenuOutcome::Complete(BTreeMap::from([(
                "level".to_string(),
                "high".to_string()
            )]))
        );
        assert_eq!(submenu.step_index(), 0);
        assert!(submenu.context().is_empty());
    }
}
