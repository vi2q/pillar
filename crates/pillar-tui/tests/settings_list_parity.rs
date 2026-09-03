//! Parity tests for the settings-list component (pi v0.84.3
//! components/settings-list.ts).

use pillar_tui::settings_list::{SettingItem, SettingsList, SettingsListTheme};

fn passthrough(text: &str, _selected: bool) -> String {
    text.to_string()
}

fn passthrough1(text: &str) -> String {
    text.to_string()
}

fn theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(passthrough),
        value: Box::new(passthrough),
        description: Box::new(passthrough1),
        cursor: "→ ".to_string(),
        hint: Box::new(passthrough1),
    }
}

fn items() -> Vec<SettingItem> {
    vec![
        SettingItem {
            id: "theme".to_string(),
            label: "Theme".to_string(),
            description: Some("Color scheme".to_string()),
            current_value: "dark".to_string(),
            values: vec!["dark".to_string(), "light".to_string()],
        },
        SettingItem {
            id: "model".to_string(),
            label: "Model".to_string(),
            description: None,
            current_value: "sonnet".to_string(),
            values: vec!["sonnet".to_string(), "opus".to_string()],
        },
        SettingItem {
            id: "nosub".to_string(),
            label: "NoSub".to_string(),
            description: None,
            current_value: "fixed".to_string(),
            values: vec![],
        },
    ]
}

// --- value cycling ------------------------------------------------------------------------------

#[test]
fn activate_cycles_values_and_reports_change() {
    let mut changes = Vec::new();
    {
        let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
        list.activate_selected(&mut |id, value| changes.push((id.to_string(), value.to_string())));
    }
    assert_eq!(changes, vec![("theme".to_string(), "light".to_string())]);
}

#[test]
fn activate_cycles_wraps_around() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.activate_selected(&mut |_, _| {});
    list.activate_selected(&mut |_, _| {});
    assert_eq!(list_value(&list, "theme"), "dark");
}

fn list_value(list: &SettingsList, id: &str) -> String {
    // Access via rendering: find the row containing the id's value.
    let lines = list.render(200, &theme());
    lines
        .iter()
        .find(|l| l.contains("Theme"))
        .cloned()
        .unwrap_or_default();
    let _ = id;
    // Simpler: render and grab the Theme row's value section.
    let lines = list.render(200, &theme());
    let row = lines.iter().find(|l| l.contains("Theme")).unwrap();
    row.split_whitespace().last().unwrap().to_string()
}

#[test]
fn item_without_values_does_not_change() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.set_selected_index_by_id("nosub");
    let changed = list.activate_selected(&mut |_, _| {});
    assert_eq!(changed, None);
}

// --- selection / movement -------------------------------------------------------------------------

trait SelectById {
    fn set_selected_index_by_id(&mut self, id: &str);
}

impl SelectById for SettingsList {
    fn set_selected_index_by_id(&mut self, id: &str) {
        // Find index by rendering order: items are in insertion order.
        let ids = ["theme", "model", "nosub"];
        let index = ids.iter().position(|&i| i == id).unwrap();
        for _ in 0..index {
            self.move_down();
        }
    }
}

#[test]
fn move_up_wraps_to_bottom() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.move_up();
    assert_eq!(list.selected_index(), 2);
}

#[test]
fn move_down_wraps_to_top() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.move_down();
    list.move_down();
    assert_eq!(list.selected_index(), 2);
    list.move_down();
    assert_eq!(list.selected_index(), 0);
}

#[test]
fn select_item_moves_to_id() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.select_item("nosub");
    assert_eq!(list.selected_index(), 2);
    // Unknown id is a no-op.
    list.select_item("missing");
    assert_eq!(list.selected_index(), 2);
}

#[test]
fn update_value_changes_current_value() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    list.update_value("theme", "solarized");
    let lines = list.render(200, &theme());
    let row = lines.iter().find(|l| l.contains("Theme")).unwrap();
    assert!(row.contains("solarized"), "{row:?}");
}

// --- rendering --------------------------------------------------------------------------------------

#[test]
fn render_rows_with_padded_labels() {
    let list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    let theme_row = lines.iter().find(|l| l.contains("Theme")).unwrap();
    let model_row = lines.iter().find(|l| l.contains("Model")).unwrap();
    // Values align: both descriptions start at the same column.
    // Both values start at the same visible column (prefix + label +
    // separator). Byte offsets differ because the cursor is multibyte.
    let t = theme_row.find("dark").unwrap();
    let m = model_row.find("sonnet").unwrap();
    assert_eq!(theme_row.len() - t, 4);
    assert_eq!(m, 2 + 5 + 2);
    assert!(t > m);
}

#[test]
fn render_selected_cursor_prefix() {
    let list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    assert!(lines[0].starts_with("→ "), "{:?}", lines[0]);
}

#[test]
fn render_description_for_selected_item() {
    let list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    assert!(
        lines.iter().any(|l| l.contains("Color scheme")),
        "{lines:?}"
    );
}

#[test]
fn render_hint_line() {
    let list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Enter/Space to change · Esc to cancel")),
        "{lines:?}"
    );
}

#[test]
fn render_scroll_indicator() {
    let list = SettingsList::new(items(), 2, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    assert!(lines.iter().any(|l| l.contains("(1/3)")), "{lines:?}");
}

#[test]
fn render_search_box_and_search_hint_when_enabled() {
    let list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, true);
    let lines = list.render(80, &theme());
    assert!(lines.iter().any(|l| l.starts_with("> ")), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Type to search · Enter/Space to change")),
        "{lines:?}"
    );
}

#[test]
fn render_empty_message() {
    let list = SettingsList::new(vec![], 10, &mut |_, _| {}, &mut || {}, false);
    let lines = list.render(80, &theme());
    assert!(lines.iter().any(|l| l.contains("No settings available")));
}

// --- search filtering ---------------------------------------------------------------------------------

#[test]
fn search_filter_fuzzy_matches_labels() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, true);
    if let Some(input) = list.search_input_mut() {
        input.set_value("thm");
    }
    list.apply_filter("thm");
    // Fuzzy "thm" matches "Theme" only.
    let lines = list.render(80, &theme());
    assert!(lines.iter().any(|l| l.contains("Theme")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("Model")), "{lines:?}");
}

#[test]
fn search_filter_resets_selection() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, true);
    list.move_down();
    list.move_down();
    list.apply_filter("model");
    assert_eq!(list.selected_index(), 0);
}

#[test]
fn search_moves_selection_onto_filtered_items() {
    let mut list = SettingsList::new(items(), 10, &mut |_, _| {}, &mut || {}, true);
    list.apply_filter("model");
    // Selection is on the filtered "model" row → activates model values.
    let changed = list.activate_selected(&mut |_, _| {});
    assert_eq!(changed, Some(("model".to_string(), "opus".to_string())));
}
