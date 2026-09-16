//! Port of packages/coding-agent/src/modes/interactive/components/
//! settings-selector.ts (pi v0.84.3): the `/settings` panel — a searchable
//! list of the user settings plus the theme, warnings and per-model
//! thinking-level submenus.
//!
//! divergences:
//! - The ~35 upstream callbacks are replaced by
//!   [`SettingsSelectorOutcome`]; the host applies the change (the port's
//!   components answer outcomes instead of invoking callbacks).

use std::collections::BTreeMap;

use pillar_ai::types::Model;
use pillar_tui::components::Text;
use pillar_tui::input::dispatch_input_keybinding;
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::select_list::SelectItem;
use pillar_tui::settings_list::{SettingItem, SettingsActivation, SettingsList};
use pillar_tui::tui::{Component, Focusable};

use crate::core::http_dispatcher::{HTTP_IDLE_TIMEOUT_CHOICES, format_http_idle_timeout_ms};
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_display_text;
use crate::modes::interactive::components::settings_submenu::{
    SelectSubmenu, SelectSubmenuOutcome, StepContext, SteppedSubmenu, SteppedSubmenuOutcome,
    SteppedSubmenuStep, SubmenuLayout,
};
use crate::modes::interactive::theme::{
    TerminalTheme, get_settings_list_theme, parse_auto_theme_setting, theme,
};

fn matches(data: &str, keybinding: &str) -> bool {
    with_global_keybindings(|kb| kb.matches(data, keybinding))
}

/// Upstream `THINKING_DESCRIPTIONS`.
fn thinking_description(level: &str) -> &'static str {
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

/// Upstream's level list for a reasoning model (`getSupportedThinkingLevels`).
fn supported_thinking_levels(model: &Model) -> Vec<String> {
    if !model.reasoning {
        return vec!["off".to_string()];
    }
    pillar_ai::models::get_supported_thinking_levels(model)
        .into_iter()
        .map(|level| {
            match level {
                pillar_ai::types::ModelThinkingLevel::Off => "off",
                pillar_ai::types::ModelThinkingLevel::Minimal => "minimal",
                pillar_ai::types::ModelThinkingLevel::Low => "low",
                pillar_ai::types::ModelThinkingLevel::Medium => "medium",
                pillar_ai::types::ModelThinkingLevel::High => "high",
                pillar_ai::types::ModelThinkingLevel::Xhigh => "xhigh",
                pillar_ai::types::ModelThinkingLevel::Max => "max",
            }
            .to_string()
        })
        .collect()
}

/// Upstream `DEFAULT_PROJECT_TRUST_LABELS` (value → label).
const DEFAULT_PROJECT_TRUST_LABELS: [(&str, &str); 3] = [
    ("ask", "Ask"),
    ("always", "Always trust"),
    ("never", "Never trust"),
];

fn project_trust_label(value: &str) -> &'static str {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(key, _)| *key == value)
        .map(|(_, label)| *label)
        .unwrap_or("Ask")
}

fn project_trust_value(label: &str) -> Option<&'static str> {
    DEFAULT_PROJECT_TRUST_LABELS
        .iter()
        .find(|(_, value)| *value == label)
        .map(|(key, _)| *key)
}

const MODEL_PICKER_LAYOUT: SubmenuLayout = SubmenuLayout {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(46),
};

const CLEAR_OVERRIDE_VALUE: &str = "__clear__";
const AUTOMATIC_THEME_VALUE: &str = "/";

/// The `/settings` panel's snapshot of every displayed value (upstream
/// `SettingsConfig`).
#[derive(Debug, Clone)]
pub struct SettingsConfig {
    pub auto_compact: bool,
    pub default_model: String,
    pub current_model: Option<Model>,
    pub available_default_models: Vec<Model>,
    pub show_images: bool,
    pub image_width_cells: u64,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub enable_skill_commands: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub transport: String,
    pub http_idle_timeout_ms: u64,
    pub thinking_level: String,
    pub model_thinking_levels: BTreeMap<String, String>,
    /// The theme *setting* (a name or a `light/dark` pair).
    pub current_theme: String,
    pub terminal_theme: TerminalTheme,
    pub available_themes: Vec<String>,
    pub hide_thinking_block: bool,
    pub mermaid_rendering_mode: String,
    pub show_cache_miss_notices: bool,
    pub collapse_changelog: bool,
    pub enable_install_telemetry: bool,
    pub double_escape_action: String,
    pub tree_filter_mode: String,
    pub show_hardware_cursor: bool,
    pub editor_padding_x: i64,
    pub output_pad: u8,
    pub autocomplete_max_visible: u64,
    pub quiet_startup: bool,
    pub default_project_trust: String,
    pub clear_on_shrink: bool,
    pub show_terminal_progress: bool,
    pub tui_mode: String,
    pub fullscreen_exit_output: String,
    pub fullscreen_scrollbar: String,
    pub fullscreen_copy_on_select: bool,
    pub warnings: serde_json::Value,
    /// `getCapabilities().images`; the image rows are hidden without it.
    pub supports_images: bool,
}

/// What the host must do after a key (upstream the `SettingsCallbacks`).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsSelectorOutcome {
    /// Handled inside the selector.
    Consumed,
    /// A setting changed (upstream `onChange(id, newValue)`): the flat rows
    /// and the theme / warnings submenus. `value` is the stored form, not the
    /// displayed label.
    Change { id: String, value: String },
    /// The theme submenu's highlighted option changed (upstream
    /// `onThemePreview`): preview it without persisting.
    ThemePreview(String),
    /// A per-model thinking level was set (upstream
    /// `onModelThinkingLevelChange`).
    ModelThinkingLevelChange {
        provider: String,
        model_id: String,
        level: String,
    },
    /// A per-model thinking level was cleared (upstream
    /// `onModelThinkingLevelRemove`).
    ModelThinkingLevelRemove { provider: String, model_id: String },
    /// Escape at the top level (upstream `onCancel`).
    Close,
}

// --- model helpers ---------------------------------------------------------

fn model_setting_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn model_display_label(model: &Model) -> String {
    format!("{} [{}]", model.id, model.provider)
}

fn model_thinking_overrides_summary(overrides: &BTreeMap<String, String>) -> String {
    match overrides.len() {
        0 => "none".to_string(),
        count => format!("{count} configured"),
    }
}

fn model_item_label(model: &Model) -> String {
    format!(
        "{} {}",
        model.id,
        theme().fg("muted", &format!("[{}]", model.provider))
    )
}

// --- theme helpers ---------------------------------------------------------

fn theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    available_themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: name.clone(),
            description: None,
        })
        .collect()
}

/// Upstream `singleModeThemeItems`: the Automatic entry first.
fn single_mode_theme_items(available_themes: &[String]) -> Vec<SelectItem> {
    let mut items = vec![SelectItem {
        value: AUTOMATIC_THEME_VALUE.to_string(),
        label: "Automatic".to_string(),
        description: Some("Use separate themes for light and dark terminal appearance".to_string()),
    }];
    items.extend(theme_items(available_themes));
    items
}

/// Upstream `preferredTheme`.
fn preferred_theme(available_themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    if let Some(preferred) = preferred {
        if available_themes.iter().any(|theme| theme == preferred) {
            return preferred.to_string();
        }
    }
    if available_themes.iter().any(|theme| theme == fallback) {
        return fallback.to_string();
    }
    available_themes
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// Upstream `defaultAutomaticThemes`.
fn default_automatic_themes(
    current_theme_setting: &str,
    available_themes: &[String],
) -> (String, String) {
    if let Some(auto) = parse_auto_theme_setting(Some(current_theme_setting)) {
        return (auto.light_theme, auto.dark_theme);
    }
    let current_fixed_theme = if current_theme_setting.contains('/') {
        None
    } else {
        Some(current_theme_setting)
    };
    let theme_name = preferred_theme(available_themes, current_fixed_theme, "dark");
    (theme_name.clone(), theme_name)
}

// --- warnings submenu ------------------------------------------------------

enum WarningsOutcome {
    Consumed,
    Changed(serde_json::Value),
    Cancel,
}

/// Upstream `WarningSettingsSubmenu`: a one-item settings list.
struct WarningSettingsSubmenu {
    list: SettingsList,
    state: serde_json::Value,
}

impl WarningSettingsSubmenu {
    fn new(warnings: serde_json::Value) -> Self {
        let enabled = warnings
            .get("anthropicExtraUsage")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let items = vec![SettingItem {
            id: "anthropic-extra-usage".to_string(),
            label: "Anthropic extra usage".to_string(),
            description: Some(
                "Warn when Anthropic subscription auth may use paid extra usage".to_string(),
            ),
            current_value: if enabled { "true" } else { "false" }.to_string(),
            values: vec!["true".to_string(), "false".to_string()],
            submenu: None,
        }];
        Self {
            list: SettingsList::new(items, 10, &mut |_, _| {}, &mut || {}, false),
            state: warnings,
        }
    }

    fn handle_key(&mut self, data: &str) -> WarningsOutcome {
        if matches(data, "tui.select.cancel") {
            return WarningsOutcome::Cancel;
        }
        if matches(data, "tui.select.up") {
            self.list.move_up();
            return WarningsOutcome::Consumed;
        }
        if matches(data, "tui.select.down") {
            self.list.move_down();
            return WarningsOutcome::Consumed;
        }
        if matches(data, "tui.select.confirm") || data == " " {
            let mut changes: Vec<(String, String)> = Vec::new();
            let activation = self.list.activate_selected(&mut |id, value| {
                changes.push((id.to_string(), value.to_string()));
            });
            if let SettingsActivation::Cycled { .. } = activation {
                if let Some((_, value)) = changes.first() {
                    if let Some(object) = self.state.as_object_mut() {
                        object.insert(
                            "anthropicExtraUsage".to_string(),
                            serde_json::Value::Bool(value == "true"),
                        );
                    }
                    return WarningsOutcome::Changed(self.state.clone());
                }
            }
        }
        WarningsOutcome::Consumed
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        self.list.render(width, &get_settings_list_theme())
    }
}

// --- theme submenu ---------------------------------------------------------

enum ThemeSubmenuOutcome {
    Consumed,
    Preview(String),
    Apply(String),
    Cancel,
}

/// Which view the theme submenu shows (upstream `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeViewKind {
    Single,
    Automatic,
}

/// Upstream `ThemeSubmenu`: one theme, or an automatic light/dark pair.
/// The sub-components are kept as flat fields so transitions can rebuild them
/// without re-entrant borrows.
struct ThemeSubmenu {
    terminal_theme: TerminalTheme,
    available_themes: Vec<String>,
    original_theme_setting: String,
    kind: ThemeViewKind,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
    single: SelectSubmenu,
    automatic: SettingsList,
    /// The nested light/dark picker: `(is_light, picker)`.
    nested: Option<(bool, SelectSubmenu)>,
}

impl ThemeSubmenu {
    fn new(
        current_theme_setting: &str,
        terminal_theme: TerminalTheme,
        available_themes: Vec<String>,
    ) -> Self {
        let auto = parse_auto_theme_setting(Some(current_theme_setting));
        let (light_theme, dark_theme) =
            default_automatic_themes(current_theme_setting, &available_themes);
        let fixed_theme = if auto.is_some() || current_theme_setting.contains('/') {
            None
        } else {
            Some(current_theme_setting.to_string())
        };
        let mut submenu = Self {
            terminal_theme,
            available_themes,
            original_theme_setting: current_theme_setting.to_string(),
            kind: if auto.is_some() {
                ThemeViewKind::Automatic
            } else {
                ThemeViewKind::Single
            },
            single_theme: String::new(),
            light_theme,
            dark_theme,
            single: SelectSubmenu::new("", "", Vec::new(), "", false, SubmenuLayout::default()),
            automatic: SettingsList::new(Vec::new(), 10, &mut |_, _| {}, &mut || {}, false),
            nested: None,
        };
        let active_automatic = submenu.active_automatic_theme();
        submenu.single_theme = preferred_theme(
            &submenu.available_themes.clone(),
            fixed_theme.as_deref().or(if auto.is_some() {
                Some(active_automatic.as_str())
            } else {
                None
            }),
            "dark",
        );
        if submenu.kind == ThemeViewKind::Automatic {
            submenu.show_automatic_menu();
        } else {
            submenu.show_single_menu();
        }
        submenu
    }

    fn active_automatic_theme(&self) -> String {
        if self.terminal_theme == TerminalTheme::Light {
            self.light_theme.clone()
        } else {
            self.dark_theme.clone()
        }
    }

    fn automatic_theme_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }

    fn theme_setting(&self) -> String {
        if self.kind == ThemeViewKind::Automatic {
            self.automatic_theme_setting()
        } else {
            self.single_theme.clone()
        }
    }

    fn show_single_menu(&mut self) {
        self.kind = ThemeViewKind::Single;
        self.nested = None;
        self.single = SelectSubmenu::new(
            "Theme",
            "Select a theme, or choose Automatic to follow terminal appearance.",
            single_mode_theme_items(&self.available_themes),
            &self.single_theme,
            false,
            SubmenuLayout::submenu_default(),
        );
    }

    fn automatic_items(&self) -> Vec<SettingItem> {
        vec![
            SettingItem {
                id: "light-theme".to_string(),
                label: "Light theme".to_string(),
                description: Some(
                    "Theme to use in automatic mode when the terminal is light".to_string(),
                ),
                current_value: self.light_theme.clone(),
                values: Vec::new(),
                submenu: Some("light-theme".to_string()),
            },
            SettingItem {
                id: "dark-theme".to_string(),
                label: "Dark theme".to_string(),
                description: Some(
                    "Theme to use in automatic mode when the terminal is dark".to_string(),
                ),
                current_value: self.dark_theme.clone(),
                values: Vec::new(),
                submenu: Some("dark-theme".to_string()),
            },
            SettingItem {
                id: "apply".to_string(),
                label: "Apply".to_string(),
                description: Some("Save and go back".to_string()),
                current_value: "save and go back".to_string(),
                values: vec!["save and go back".to_string()],
                submenu: None,
            },
            SettingItem {
                id: "single-mode".to_string(),
                label: "Change mode".to_string(),
                description: Some("Switch to one theme for light and dark".to_string()),
                current_value: "switch to single theme".to_string(),
                values: vec!["switch to single theme".to_string()],
                submenu: None,
            },
        ]
    }

    fn show_automatic_menu(&mut self) {
        self.kind = ThemeViewKind::Automatic;
        self.nested = None;
        // Keep the current selection on the Apply row (upstream starts on the
        // first row; the row values carry the live light/dark names).
        self.automatic = SettingsList::new(
            self.automatic_items(),
            10,
            &mut |_, _| {},
            &mut || {},
            false,
        );
    }

    fn sync_automatic_values(&mut self) {
        self.automatic
            .update_value("light-theme", &self.light_theme);
        self.automatic.update_value("dark-theme", &self.dark_theme);
    }

    fn create_theme_select(&self, title: &str, description: &str, current: &str) -> SelectSubmenu {
        SelectSubmenu::new(
            title,
            description,
            theme_items(&self.available_themes),
            current,
            false,
            SubmenuLayout::submenu_default(),
        )
    }

    fn handle_key(&mut self, data: &str) -> ThemeSubmenuOutcome {
        // A nested picker owns the keys while open.
        if let Some((is_light, picker)) = self.nested.as_mut() {
            return match picker.handle_key(data) {
                SelectSubmenuOutcome::Consumed => ThemeSubmenuOutcome::Consumed,
                SelectSubmenuOutcome::Select(value) => {
                    if *is_light {
                        self.light_theme = value;
                    } else {
                        self.dark_theme = value;
                    }
                    self.nested = None;
                    self.sync_automatic_values();
                    ThemeSubmenuOutcome::Preview(self.theme_setting())
                }
                SelectSubmenuOutcome::Cancel => {
                    self.nested = None;
                    ThemeSubmenuOutcome::Preview(self.theme_setting())
                }
            };
        }

        if self.kind == ThemeViewKind::Single {
            return match self.single.handle_key(data) {
                SelectSubmenuOutcome::Consumed => ThemeSubmenuOutcome::Consumed,
                SelectSubmenuOutcome::Select(value) => {
                    if value == AUTOMATIC_THEME_VALUE {
                        self.show_automatic_menu();
                        return ThemeSubmenuOutcome::Preview(self.theme_setting());
                    }
                    self.single_theme = value.clone();
                    ThemeSubmenuOutcome::Apply(value)
                }
                SelectSubmenuOutcome::Cancel => ThemeSubmenuOutcome::Cancel,
            };
        }

        if matches(data, "tui.select.cancel") {
            return ThemeSubmenuOutcome::Cancel;
        }
        if matches(data, "tui.select.up") {
            self.automatic.move_up();
            return ThemeSubmenuOutcome::Consumed;
        }
        if matches(data, "tui.select.down") {
            self.automatic.move_down();
            return ThemeSubmenuOutcome::Consumed;
        }
        if matches(data, "tui.select.confirm") || data == " " {
            let mut changes: Vec<(String, String)> = Vec::new();
            let activation = self.automatic.activate_selected(&mut |id, value| {
                changes.push((id.to_string(), value.to_string()));
            });
            match activation {
                SettingsActivation::OpenSubmenu { id, .. } => {
                    let is_light = id == "light-theme";
                    let (title, description, current) = if is_light {
                        (
                            "Light Theme",
                            "Select the theme to use for light terminal appearance",
                            self.light_theme.clone(),
                        )
                    } else {
                        (
                            "Dark Theme",
                            "Select the theme to use for dark terminal appearance",
                            self.dark_theme.clone(),
                        )
                    };
                    self.nested = Some((
                        is_light,
                        self.create_theme_select(title, description, &current),
                    ));
                }
                SettingsActivation::Cycled { id, .. } => match id.as_str() {
                    "single-mode" => {
                        self.single_theme = self.active_automatic_theme();
                        let preview = self.single_theme.clone();
                        self.show_single_menu();
                        return ThemeSubmenuOutcome::Preview(preview);
                    }
                    "apply" => {
                        return ThemeSubmenuOutcome::Apply(self.automatic_theme_setting());
                    }
                    _ => {}
                },
                SettingsActivation::None => {}
            }
        }
        ThemeSubmenuOutcome::Consumed
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((_, picker)) = self.nested.as_mut() {
            return picker.render(width);
        }
        if self.kind == ThemeViewKind::Single {
            return self.single.render(width);
        }
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(
            Text::new(
                &theme_handle.bold(&theme_handle.fg("accent", "Automatic Theme")),
                0,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(
            Text::new(
                &theme_handle.fg(
                    "muted",
                    "Choose themes for terminal light and dark appearance.",
                ),
                0,
                0,
            )
            .render(width),
        );
        lines.extend(
            Text::new(
                &theme_handle.fg("muted", "Light/dark detection requires terminal support."),
                0,
                0,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(self.automatic.render(width, &get_settings_list_theme()));
        lines
    }
}

// --- main selector ---------------------------------------------------------

enum ActiveSubmenu {
    Theme(Box<ThemeSubmenu>),
    Warnings(Box<WarningSettingsSubmenu>),
    Stepped(Box<SteppedSubmenu>),
}

/// The `/settings` panel (upstream `SettingsSelectorComponent`).
pub struct SettingsSelectorComponent {
    list: SettingsList,
    config: SettingsConfig,
    active_submenu: Option<ActiveSubmenu>,
    submenu_item_id: Option<String>,
    focused: bool,
}

impl SettingsSelectorComponent {
    pub fn new(config: SettingsConfig) -> Self {
        let list = SettingsList::new(build_items(&config), 10, &mut |_, _| {}, &mut || {}, true);
        Self {
            list,
            config,
            active_submenu: None,
            submenu_item_id: None,
            focused: false,
        }
    }

    /// The underlying settings list (upstream `getSettingsList`).
    pub fn settings_list(&self) -> &SettingsList {
        &self.list
    }

    /// Update one row's value (upstream
    /// `selector.getSettingsList().updateValue(id, value)`).
    pub fn refresh_value(&mut self, id: &str, value: &str) {
        self.list.update_value(id, value);
    }

    /// Move the cursor to a setting by id (upstream
    /// `getSettingsList().selectItem`).
    pub fn select_item(&mut self, id: &str) {
        self.list.select_item(id);
    }

    /// The currently highlighted setting id (test / host convenience).
    pub fn selected_item_id(&self) -> Option<String> {
        self.list.selected_item_id()
    }

    /// Whether a submenu currently owns the keys.
    pub fn is_submenu_open(&self) -> bool {
        self.active_submenu.is_some()
    }

    /// Host-driven key handling (upstream `SettingsList.handleInput` plus the
    /// submenu delegation).
    pub fn handle_key(&mut self, data: &str) -> SettingsSelectorOutcome {
        // An open submenu owns every key; take it out so the handlers can
        // mutate `self` (upstream the submenu is a child component).
        if let Some(mut active) = self.active_submenu.take() {
            let (outcome, keep) = match &mut active {
                ActiveSubmenu::Theme(submenu) => match submenu.handle_key(data) {
                    ThemeSubmenuOutcome::Consumed => (SettingsSelectorOutcome::Consumed, true),
                    ThemeSubmenuOutcome::Preview(value) => {
                        (SettingsSelectorOutcome::ThemePreview(value), true)
                    }
                    ThemeSubmenuOutcome::Apply(value) => (
                        SettingsSelectorOutcome::Change {
                            id: "theme".to_string(),
                            value,
                        },
                        false,
                    ),
                    ThemeSubmenuOutcome::Cancel => {
                        let original = submenu.original_theme_setting.clone();
                        (SettingsSelectorOutcome::ThemePreview(original), false)
                    }
                },
                ActiveSubmenu::Warnings(submenu) => match submenu.handle_key(data) {
                    WarningsOutcome::Consumed => (SettingsSelectorOutcome::Consumed, true),
                    WarningsOutcome::Changed(warnings) => (
                        SettingsSelectorOutcome::Change {
                            id: "warnings".to_string(),
                            value: serde_json::to_string(&warnings).unwrap_or_default(),
                        },
                        true,
                    ),
                    WarningsOutcome::Cancel => (SettingsSelectorOutcome::Consumed, false),
                },
                ActiveSubmenu::Stepped(submenu) => match submenu.handle_key(data) {
                    SteppedSubmenuOutcome::Consumed => (SettingsSelectorOutcome::Consumed, true),
                    SteppedSubmenuOutcome::Complete(context) => {
                        // `loop: true`: the stepped submenu stays open.
                        (self.complete_model_thinking(&context), true)
                    }
                    SteppedSubmenuOutcome::Cancel => (SettingsSelectorOutcome::Consumed, false),
                },
            };
            if keep {
                self.active_submenu = Some(active);
            }
            if !keep {
                // Closing restores the cursor to the row that opened it.
                if let Some(id) = self.submenu_item_id.take() {
                    self.list.select_item(&id);
                }
            }
            return outcome;
        }

        if matches(data, "tui.select.cancel") {
            return SettingsSelectorOutcome::Close;
        }
        if matches(data, "tui.select.up") {
            self.list.move_up();
            return SettingsSelectorOutcome::Consumed;
        }
        if matches(data, "tui.select.down") {
            self.list.move_down();
            return SettingsSelectorOutcome::Consumed;
        }
        if matches(data, "tui.select.confirm") || data == " " {
            let mut changes: Vec<(String, String)> = Vec::new();
            let activation = self.list.activate_selected(&mut |id, value| {
                changes.push((id.to_string(), value.to_string()));
            });
            return match activation {
                SettingsActivation::None => SettingsSelectorOutcome::Consumed,
                SettingsActivation::Cycled { id, value } => SettingsSelectorOutcome::Change {
                    id: id.clone(),
                    value: self.map_cycled_value(&id, &value),
                },
                SettingsActivation::OpenSubmenu { id, submenu, .. } => {
                    self.open_submenu(&id, &submenu);
                    SettingsSelectorOutcome::Consumed
                }
            };
        }

        // Everything else goes to the search box.
        let mut query = None;
        if let Some(input) = self.list.search_input_mut() {
            if !dispatch_input_keybinding(input, data) {
                input.handle_input(data);
            }
            query = Some(input.get_value().to_string());
        }
        if let Some(query) = query {
            self.list.apply_filter(&query);
        }
        SettingsSelectorOutcome::Consumed
    }

    /// Translate a cycled *label* back to the stored value where they differ
    /// (project trust and the HTTP timeout rows show labels).
    fn map_cycled_value(&self, id: &str, label: &str) -> String {
        match id {
            "default-project-trust" => project_trust_value(label).unwrap_or(label).to_string(),
            "http-idle-timeout" => HTTP_IDLE_TIMEOUT_CHOICES
                .iter()
                .find(|(choice, _)| *choice == label)
                .map(|(_, value)| value.to_string())
                .unwrap_or_else(|| label.to_string()),
            _ => label.to_string(),
        }
    }

    fn open_submenu(&mut self, item_id: &str, submenu: &str) {
        let opened = match submenu {
            "theme" => Some(ActiveSubmenu::Theme(Box::new(ThemeSubmenu::new(
                &self.config.current_theme,
                self.config.terminal_theme,
                self.config.available_themes.clone(),
            )))),
            "warnings" => Some(ActiveSubmenu::Warnings(Box::new(
                WarningSettingsSubmenu::new(self.config.warnings.clone()),
            ))),
            "model-thinking" => Some(ActiveSubmenu::Stepped(Box::new(
                self.build_model_thinking_submenu(),
            ))),
            _ => None,
        };
        if let Some(opened) = opened {
            self.submenu_item_id = Some(item_id.to_string());
            self.active_submenu = Some(opened);
        }
    }

    /// Upstream the `model-thinking` item's `SteppedSubmenu` (model → level,
    /// looping).
    fn build_model_thinking_submenu(&self) -> SteppedSubmenu {
        let config = self.config.clone();
        let current_model_key = config.current_model.as_ref().map(model_setting_key);
        let default_model_key = config
            .available_default_models
            .iter()
            .map(model_setting_key)
            .find(|key| *key == config.default_model);

        // Step 1: pick a model (the current / default one sorts first).
        let models_step = {
            let options_config = config.clone();
            let options_current = current_model_key.clone();
            let options_default = default_model_key.clone();
            let preselect_current = current_model_key.clone();
            let preselect_default = default_model_key.clone();
            SteppedSubmenuStep {
                key: "model".to_string(),
                title: Box::new(|_| "Per-Model Thinking Level".to_string()),
                description: Box::new(|_| "Select a model to configure".to_string()),
                options: Box::new(move |_| {
                    let mut sorted = options_config.available_default_models.clone();
                    sorted.sort_by(|a, b| {
                        let a_key = model_setting_key(a);
                        let b_key = model_setting_key(b);
                        let rank = |key: &str| {
                            if Some(key) == options_current.as_deref() {
                                0
                            } else if Some(key) == options_default.as_deref() {
                                1
                            } else {
                                2
                            }
                        };
                        rank(&a_key)
                            .cmp(&rank(&b_key))
                            .then_with(|| a.provider.cmp(&b.provider))
                    });
                    let mut items: Vec<SelectItem> = sorted
                        .iter()
                        .map(|model| {
                            let key = model_setting_key(model);
                            SelectItem {
                                value: key.clone(),
                                label: model_item_label(model),
                                description: options_config
                                    .model_thinking_levels
                                    .get(&key)
                                    .cloned(),
                            }
                        })
                        .collect();
                    if items.is_empty() {
                        items.push(SelectItem {
                            value: "__none__".to_string(),
                            label: "No models available".to_string(),
                            description: Some(
                                "Log in to a provider or configure an API key first".to_string(),
                            ),
                        });
                    }
                    items
                }),
                preselect: Some(Box::new(move |_| {
                    preselect_current
                        .clone()
                        .or_else(|| preselect_default.clone())
                })),
                searchable: true,
                layout: Some(MODEL_PICKER_LAYOUT),
            }
        };

        // Step 2: pick the level for that model.
        let levels_step = {
            let title_config = config.clone();
            let options_config = config.clone();
            let preselect_config = config.clone();
            SteppedSubmenuStep {
                key: "level".to_string(),
                title: Box::new(move |context| {
                    let model = title_config
                        .available_default_models
                        .iter()
                        .find(|model| model_setting_key(model) == context["model"]);
                    match model {
                        Some(model) => format!("Thinking Level for {}", model_display_label(model)),
                        None => format!("Thinking Level for {}", context["model"]),
                    }
                }),
                description: Box::new(|_| {
                    "Select default thinking level for this model".to_string()
                }),
                options: Box::new(move |context| {
                    let model_key = context["model"].clone();
                    let Some(model) = options_config
                        .available_default_models
                        .iter()
                        .find(|model| model_setting_key(model) == model_key)
                    else {
                        return Vec::new();
                    };
                    let mut items: Vec<SelectItem> = supported_thinking_levels(model)
                        .into_iter()
                        .map(|level| SelectItem {
                            value: level.clone(),
                            label: level.clone(),
                            description: Some(thinking_description(&level).to_string()),
                        })
                        .collect();
                    if options_config
                        .model_thinking_levels
                        .contains_key(&model_key)
                    {
                        items.push(SelectItem {
                            value: CLEAR_OVERRIDE_VALUE.to_string(),
                            label: "(clear override)".to_string(),
                            description: Some(format!(
                                "Revert to global default ({})",
                                options_config.thinking_level
                            )),
                        });
                    }
                    items
                }),
                preselect: Some(Box::new(move |context| {
                    preselect_config
                        .model_thinking_levels
                        .get(&context["model"])
                        .cloned()
                })),
                searchable: false,
                layout: None,
            }
        };

        SteppedSubmenu::new(
            vec![models_step, levels_step],
            true,
            None,
            StepContext::new(),
        )
    }

    fn complete_model_thinking(&mut self, context: &StepContext) -> SettingsSelectorOutcome {
        let Some(model_key) = context.get("model") else {
            return SettingsSelectorOutcome::Consumed;
        };
        let Some(level) = context.get("level") else {
            return SettingsSelectorOutcome::Consumed;
        };
        let Some(model) = self
            .config
            .available_default_models
            .iter()
            .find(|model| model_setting_key(model) == *model_key)
            .cloned()
        else {
            return SettingsSelectorOutcome::Consumed;
        };
        let (provider, model_id) = (model.provider.clone(), model.id.clone());
        if level == CLEAR_OVERRIDE_VALUE {
            self.config.model_thinking_levels.remove(model_key);
            let summary = model_thinking_overrides_summary(&self.config.model_thinking_levels);
            self.list.update_value("model-thinking", &summary);
            return SettingsSelectorOutcome::ModelThinkingLevelRemove { provider, model_id };
        }
        self.config
            .model_thinking_levels
            .insert(model_key.clone(), level.clone());
        let summary = model_thinking_overrides_summary(&self.config.model_thinking_levels);
        self.list.update_value("model-thinking", &summary);
        SettingsSelectorOutcome::ModelThinkingLevelChange {
            provider,
            model_id,
            level: level.clone(),
        }
    }
}

impl Component for SettingsSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.extend(DynamicBorder::new().render(width));
        lines.extend(match &mut self.active_submenu {
            Some(ActiveSubmenu::Theme(submenu)) => submenu.render(width),
            Some(ActiveSubmenu::Warnings(submenu)) => submenu.render(width),
            Some(ActiveSubmenu::Stepped(submenu)) => submenu.render(width),
            None => self.list.render(width, &get_settings_list_theme()),
        });
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for SettingsSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

// --- item construction -----------------------------------------------------

fn flat(id: &str, label: &str, description: &str, current: &str, values: &[&str]) -> SettingItem {
    SettingItem {
        id: id.to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        current_value: current.to_string(),
        values: values.iter().map(|value| (*value).to_string()).collect(),
        submenu: None,
    }
}

fn insert_after(items: &mut Vec<SettingItem>, after_id: &str, item: SettingItem) {
    let index = items
        .iter()
        .position(|existing| existing.id == after_id)
        .map(|index| index + 1)
        .unwrap_or(items.len());
    items.insert(index, item);
}

/// Upstream the constructor's item list.
fn build_items(config: &SettingsConfig) -> Vec<SettingItem> {
    let follow_up_key = key_display_text("app.message.followUp");
    let cycle_thinking_key = key_display_text("app.thinking.cycle");
    let mut items: Vec<SettingItem> = vec![
        flat(
            "autocompact",
            "Auto-compact",
            "Automatically compact context when it gets too large",
            if config.auto_compact { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "steering-mode",
            "Steering mode",
            "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            &config.steering_mode,
            &["one-at-a-time", "all"],
        ),
        flat(
            "follow-up-mode",
            "Follow-up mode",
            &format!(
                "{follow_up_key} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
            ),
            &config.follow_up_mode,
            &["one-at-a-time", "all"],
        ),
        flat(
            "transport",
            "Transport",
            "Preferred transport for providers that support multiple transports",
            &config.transport,
            &["sse", "websocket", "websocket-cached", "auto"],
        ),
        SettingItem {
            id: "http-idle-timeout".to_string(),
            label: "HTTP idle timeout".to_string(),
            description: Some(
                "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.".to_string(),
            ),
            current_value: format_http_idle_timeout_ms(config.http_idle_timeout_ms),
            values: HTTP_IDLE_TIMEOUT_CHOICES
                .iter()
                .map(|(label, _)| (*label).to_string())
                .collect(),
            submenu: None,
        },
        flat(
            "hide-thinking",
            "Hide thinking",
            "Hide thinking blocks in assistant responses",
            if config.hide_thinking_block { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "mermaid-rendering",
            "Mermaid diagrams",
            "Render Mermaid code blocks as Unicode diagrams",
            &config.mermaid_rendering_mode,
            &["off", "final", "streaming"],
        ),
        flat(
            "cache-miss-notices",
            "Cache miss notices",
            "Show transcript notices for significant prompt-cache misses and compaction costs",
            if config.show_cache_miss_notices { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "collapse-changelog",
            "Collapse changelog",
            "Show condensed changelog after updates",
            if config.collapse_changelog { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "quiet-startup",
            "Quiet startup",
            "Disable verbose printing at startup",
            if config.quiet_startup { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "install-telemetry",
            "Install telemetry",
            "Send an anonymous version/update ping after changelog-detected updates",
            if config.enable_install_telemetry { "true" } else { "false" },
            &["true", "false"],
        ),
        flat(
            "default-project-trust",
            "Default project trust",
            "Fallback behavior when no extension or saved trust decision decides project trust",
            project_trust_label(&config.default_project_trust),
            &["Ask", "Always trust", "Never trust"],
        ),
        flat(
            "double-escape-action",
            "Double-escape action",
            "Action when pressing Escape twice with empty editor",
            &config.double_escape_action,
            &["tree", "fork", "none"],
        ),
        flat(
            "tree-filter-mode",
            "Tree filter mode",
            "Default filter when opening /tree",
            &config.tree_filter_mode,
            &["default", "no-tools", "user-only", "labeled-only", "all"],
        ),
        SettingItem {
            id: "warnings".to_string(),
            label: "Warnings".to_string(),
            description: Some("Enable or disable individual warnings".to_string()),
            current_value: "configure".to_string(),
            values: Vec::new(),
            submenu: Some("warnings".to_string()),
        },
        SettingItem {
            id: "model-thinking".to_string(),
            label: "Default thinking level per model".to_string(),
            description: Some(format!(
                "Override the default thinking level for specific models. {cycle_thinking_key} cycles in-session."
            )),
            current_value: model_thinking_overrides_summary(&config.model_thinking_levels),
            values: Vec::new(),
            submenu: Some("model-thinking".to_string()),
        },
        flat(
            "tui-mode",
            "TUI mode",
            "Interface layout; fullscreen mode is experimental",
            &config.tui_mode,
            &["regular", "fullscreen"],
        ),
        flat(
            "fullscreen-exit-output",
            "Fullscreen exit output",
            "Print the transcript or only a session resume hint when exiting fullscreen mode",
            &config.fullscreen_exit_output,
            &["transcript", "resume-hint"],
        ),
        flat(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
            &config.fullscreen_scrollbar,
            &["auto", "always", "hidden"],
        ),
        flat(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
            if config.fullscreen_copy_on_select { "true" } else { "false" },
            &["true", "false"],
        ),
        SettingItem {
            id: "theme".to_string(),
            label: "Theme".to_string(),
            description: Some("Color theme for the interface".to_string()),
            current_value: config.current_theme.clone(),
            values: Vec::new(),
            submenu: Some("theme".to_string()),
        },
    ];

    // Image rows only when the terminal supports images (upstream
    // `getCapabilities().images`).
    if config.supports_images {
        items.insert(
            1,
            flat(
                "show-images",
                "Show images",
                "Render images inline in terminal",
                if config.show_images { "true" } else { "false" },
                &["true", "false"],
            ),
        );
        items.insert(
            2,
            flat(
                "image-width-cells",
                "Image width",
                "Preferred inline image width in terminal cells",
                &config.image_width_cells.to_string(),
                &["60", "80", "120"],
            ),
        );
    }

    let auto_resize_index = if config.supports_images { 3 } else { 1 };
    items.insert(
        auto_resize_index,
        flat(
            "auto-resize-images",
            "Auto-resize images",
            "Resize large images to 2000x2000 max for better model compatibility",
            if config.auto_resize_images {
                "true"
            } else {
                "false"
            },
            &["true", "false"],
        ),
    );
    insert_after(
        &mut items,
        "auto-resize-images",
        flat(
            "block-images",
            "Block images",
            "Prevent images from being sent to LLM providers",
            if config.block_images { "true" } else { "false" },
            &["true", "false"],
        ),
    );
    insert_after(
        &mut items,
        "block-images",
        flat(
            "skill-commands",
            "Skill commands",
            "Register skills as /skill:name commands",
            if config.enable_skill_commands {
                "true"
            } else {
                "false"
            },
            &["true", "false"],
        ),
    );
    insert_after(
        &mut items,
        "skill-commands",
        flat(
            "show-hardware-cursor",
            "Show hardware cursor",
            "Show the terminal cursor while still positioning it for IME support",
            if config.show_hardware_cursor {
                "true"
            } else {
                "false"
            },
            &["true", "false"],
        ),
    );
    insert_after(
        &mut items,
        "show-hardware-cursor",
        flat(
            "editor-padding",
            "Editor padding",
            "Horizontal padding for input editor (0-3)",
            &config.editor_padding_x.to_string(),
            &["0", "1", "2", "3"],
        ),
    );
    insert_after(
        &mut items,
        "editor-padding",
        flat(
            "output-padding",
            "Output padding",
            "Horizontal padding for user messages, assistant messages, and thinking",
            &config.output_pad.to_string(),
            &["0", "1"],
        ),
    );
    insert_after(
        &mut items,
        "output-padding",
        flat(
            "autocomplete-max-visible",
            "Autocomplete max items",
            "Max visible items in autocomplete dropdown (3-20)",
            &config.autocomplete_max_visible.to_string(),
            &["3", "5", "7", "10", "15", "20"],
        ),
    );
    insert_after(
        &mut items,
        "autocomplete-max-visible",
        flat(
            "clear-on-shrink",
            "Clear on shrink",
            "Clear empty rows when content shrinks (may cause flicker)",
            if config.clear_on_shrink {
                "true"
            } else {
                "false"
            },
            &["true", "false"],
        ),
    );
    insert_after(
        &mut items,
        "clear-on-shrink",
        flat(
            "terminal-progress",
            "Terminal progress",
            "Show OSC 9;4 progress indicators in the terminal tab bar",
            if config.show_terminal_progress {
                "true"
            } else {
                "false"
            },
            &["true", "false"],
        ),
    );

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_tui::text_utils::strip_terminal_sequences;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::modes::interactive::components::test_support::setup()
    }

    fn model(id: &str, provider: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_string(),
            name: format!("{id} name"),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8_000,
            compat: None,
            headers: None,
            sampling_params: None,
        }
    }

    fn config() -> SettingsConfig {
        SettingsConfig {
            auto_compact: true,
            default_model: "p/a".to_string(),
            current_model: Some(model("a", "p", true)),
            available_default_models: vec![model("a", "p", true)],
            show_images: true,
            image_width_cells: 60,
            auto_resize_images: true,
            block_images: false,
            enable_skill_commands: true,
            steering_mode: "one-at-a-time".to_string(),
            follow_up_mode: "all".to_string(),
            transport: "sse".to_string(),
            http_idle_timeout_ms: 300_000,
            thinking_level: "high".to_string(),
            model_thinking_levels: BTreeMap::new(),
            current_theme: "dark".to_string(),
            terminal_theme: TerminalTheme::Dark,
            available_themes: vec!["dark".to_string(), "light".to_string()],
            hide_thinking_block: false,
            mermaid_rendering_mode: "streaming".to_string(),
            show_cache_miss_notices: false,
            collapse_changelog: false,
            enable_install_telemetry: true,
            double_escape_action: "tree".to_string(),
            tree_filter_mode: "default".to_string(),
            show_hardware_cursor: false,
            editor_padding_x: 0,
            output_pad: 1,
            autocomplete_max_visible: 5,
            quiet_startup: false,
            default_project_trust: "ask".to_string(),
            clear_on_shrink: false,
            show_terminal_progress: false,
            tui_mode: "regular".to_string(),
            fullscreen_exit_output: "transcript".to_string(),
            fullscreen_scrollbar: "auto".to_string(),
            fullscreen_copy_on_select: true,
            warnings: serde_json::json!({}),
            supports_images: true,
        }
    }

    fn plain(selector: &mut SettingsSelectorComponent) -> String {
        selector
            .render(100)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_every_row_and_the_search_box() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        let labels: Vec<String> = selector
            .settings_list()
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect();
        let body = plain(&mut selector);
        for label in [
            "Auto-compact",
            "Show images",
            "Image width",
            "Auto-resize images",
            "Block images",
            "Skill commands",
            "Show hardware cursor",
            "Editor padding",
            "Output padding",
            "Autocomplete max items",
            "Clear on shrink",
            "Terminal progress",
            "Steering mode",
            "Follow-up mode",
            "Transport",
            "HTTP idle timeout",
            "Hide thinking",
            "Mermaid diagrams",
            "Cache miss notices",
            "Collapse changelog",
            "Quiet startup",
            "Install telemetry",
            "Default project trust",
            "Double-escape action",
            "Tree filter mode",
            "Warnings",
            "Default thinking level per model",
            "TUI mode",
            "Fullscreen exit output",
            "Fullscreen scrollbar",
            "Fullscreen copy on select",
            "Theme",
        ] {
            assert!(
                labels.iter().any(|l| l == label),
                "{label:?} missing in {labels:?}"
            );
        }
        // The image rows are inserted after auto-compact (upstream order).
        let auto_pos = body.find("Auto-compact").unwrap();
        let images_pos = body.find("Show images").unwrap();
        let width_pos = body.find("Image width").unwrap();
        let resize_pos = body.find("Auto-resize images").unwrap();
        assert!(auto_pos < images_pos && images_pos < width_pos && width_pos < resize_pos);
        // Labels for the derived values (rows below the first page render
        // from the item list).
        let values: Vec<String> = selector
            .settings_list()
            .items()
            .iter()
            .map(|item| item.current_value.clone())
            .collect();
        for value in ["Ask", "5 min", "none", "true", "false"] {
            assert!(
                values.iter().any(|v| v == value),
                "{value:?} missing in {values:?}"
            );
        }
    }

    #[test]
    fn hides_the_image_rows_without_terminal_support() {
        let _guard = setup();
        let mut config = config();
        config.supports_images = false;
        let mut selector = SettingsSelectorComponent::new(config);
        let body = plain(&mut selector);
        assert!(!body.contains("Show images"), "{body}");
        assert!(!body.contains("Image width"), "{body}");
        assert!(body.contains("Auto-resize images"), "{body}");
    }

    #[test]
    fn cycling_reports_the_stored_value_not_the_label() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());

        selector.select_item("transport");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "transport".to_string(),
                value: "websocket".to_string(),
            }
        );

        // The project-trust row shows a label but stores the value.
        selector.select_item("default-project-trust");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "default-project-trust".to_string(),
                value: "always".to_string(),
            }
        );

        // The HTTP timeout row shows a label but stores milliseconds.
        selector.select_item("http-idle-timeout");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "http-idle-timeout".to_string(),
                value: "600000".to_string(), // "5 min" -> "10 min"
            }
        );
    }

    #[test]
    fn search_filters_the_rows() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        for ch in "them".chars() {
            selector.handle_key(&ch.to_string());
        }
        let body = plain(&mut selector);
        assert!(body.contains("Theme"), "{body}");
        assert!(!body.contains("Auto-compact"), "{body}");
    }

    #[test]
    fn escape_closes_the_top_level() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        assert_eq!(selector.handle_key("\x1b"), SettingsSelectorOutcome::Close);
    }

    #[test]
    fn theme_submenu_applies_and_automatic_mode_previews() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        selector.select_item("theme");
        assert_eq!(selector.handle_key("\r"), SettingsSelectorOutcome::Consumed);
        assert!(selector.is_submenu_open());
        let body = plain(&mut selector);
        assert!(body.contains("Automatic"), "{body}");

        // "dark" is pre-selected; down picks "light" and Enter applies it.
        selector.handle_key("\x1b[B");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "theme".to_string(),
                value: "light".to_string(),
            }
        );
        assert!(!selector.is_submenu_open());

        // Re-open and choose Automatic: the automatic menu is previewed.
        selector.select_item("theme");
        selector.handle_key("\r");
        selector.handle_key("\x1b[A"); // up from "dark" wraps? no: up goes to Automatic
        let outcome = selector.handle_key("\r");
        assert_eq!(
            outcome,
            SettingsSelectorOutcome::ThemePreview("dark/dark".to_string())
        );
        let body = plain(&mut selector);
        assert!(body.contains("Automatic Theme"), "{body}");
        assert!(body.contains("Light theme"), "{body}");
        assert!(body.contains("Dark theme"), "{body}");

        // Picking the Dark theme row opens the nested picker; choosing "light"
        // updates the preview.
        selector.handle_key("\x1b[B"); // -> Dark theme
        assert_eq!(selector.handle_key("\r"), SettingsSelectorOutcome::Consumed);
        let body = plain(&mut selector);
        assert!(body.contains("Dark Theme"), "{body}");
        selector.handle_key("\x1b[B"); // dark -> light... preselect is dark
        let outcome = selector.handle_key("\r");
        assert_eq!(
            outcome,
            SettingsSelectorOutcome::ThemePreview("dark/light".to_string())
        );

        // Apply commits the automatic setting (the automatic list restored the
        // "Dark theme" row after the nested picker closed).
        selector.handle_key("\x1b[B");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "theme".to_string(),
                value: "dark/light".to_string(),
            }
        );
    }

    #[test]
    fn warnings_submenu_reports_the_updated_object() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        selector.select_item("warnings");
        assert_eq!(selector.handle_key("\r"), SettingsSelectorOutcome::Consumed);
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::Change {
                id: "warnings".to_string(),
                value: "{\"anthropicExtraUsage\":false}".to_string(),
            }
        );
    }

    #[test]
    fn model_thinking_submenu_sets_and_clears_the_override() {
        let _guard = setup();
        let mut selector = SettingsSelectorComponent::new(config());
        selector.select_item("model-thinking");
        assert_eq!(selector.handle_key("\r"), SettingsSelectorOutcome::Consumed);
        // Step 1: the only model is pre-selected.
        assert_eq!(selector.handle_key("\r"), SettingsSelectorOutcome::Consumed);
        // Step 2: "off" is pre-selected (no override yet); down -> minimal.
        selector.handle_key("\x1b[B");
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::ModelThinkingLevelChange {
                provider: "p".to_string(),
                model_id: "a".to_string(),
                level: "minimal".to_string(),
            }
        );
        // The submenu loops back to step 1 (still open); Esc at step 1 goes
        // back, Esc at step 0 closes it, and the summary row was updated.
        selector.handle_key("\x1b");
        selector.handle_key("\x1b");
        assert!(!selector.is_submenu_open());
        let body = plain(&mut selector);
        assert!(body.contains("1 configured"), "{body}");

        // Re-open: the model row now shows the override, and the level step
        // offers "(clear override)".
        selector.select_item("model-thinking");
        selector.handle_key("\r");
        selector.handle_key("\r");
        // The override preselects "minimal" (index 1 of off/minimal/low/
        // medium/high + clear); four downs reach "(clear override)".
        for _ in 0..4 {
            selector.handle_key("\x1b[B");
        }
        assert_eq!(
            selector.handle_key("\r"),
            SettingsSelectorOutcome::ModelThinkingLevelRemove {
                provider: "p".to_string(),
                model_id: "a".to_string(),
            }
        );
        // The submenu loops back to step 1; leave it before reading the list.
        selector.handle_key("\x1b");
        selector.handle_key("\x1b");
        assert!(!selector.is_submenu_open());
        let body = plain(&mut selector);
        assert!(body.contains("none"), "{body}");
    }
}
