//! Parity tests for the select-list component (pi v0.84.3
//! components/select-list.ts).

use pillar_tui::select_list::{SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme};
use pillar_tui::text_utils::visible_width;

fn passthrough(text: &str) -> String {
    text.to_string()
}

fn theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(passthrough),
        selected_text: Box::new(passthrough),
        description: Box::new(passthrough),
        scroll_info: Box::new(passthrough),
        no_match: Box::new(passthrough),
    }
}

fn items(values: &[&str]) -> Vec<SelectItem> {
    values
        .iter()
        .map(|v| SelectItem {
            value: v.to_string(),
            label: v.to_string(),
            description: None,
        })
        .collect()
}

// --- filtering ------------------------------------------------------------------------------

#[test]
fn set_filter_matches_case_insensitive_prefix() {
    let mut list = SelectList::new(
        items(&["Deploy", "build", "debug"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.set_filter("DE");
    // Only "Deploy" and "debug" start with "de" (case-insensitive).
    let values: Vec<&str> = list
        .filtered_items()
        .iter()
        .map(|i| i.value.as_str())
        .collect();
    assert_eq!(values, vec!["Deploy", "debug"]);
}

#[test]
fn set_filter_resets_selection() {
    let mut list = SelectList::new(
        items(&["alpha", "beta", "gamma"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.set_selected_index(2);
    assert_eq!(list.selected_index(), 2);
    list.set_filter("b");
    assert_eq!(list.selected_index(), 0);
}

#[test]
fn set_selected_index_clamps() {
    let mut list = SelectList::new(items(&["a", "b"]), 5, SelectListLayoutOptions::default());
    list.set_selected_index(99);
    assert_eq!(list.selected_index(), 1);
}

// --- movement ---------------------------------------------------------------------------------

#[test]
fn move_up_wraps_to_bottom() {
    let mut list = SelectList::new(
        items(&["a", "b", "c"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.move_up();
    assert_eq!(list.selected_index(), 2);
}

#[test]
fn move_down_wraps_to_top() {
    let mut list = SelectList::new(
        items(&["a", "b", "c"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.set_selected_index(2);
    list.move_down();
    assert_eq!(list.selected_index(), 0);
}

// --- rendering ---------------------------------------------------------------------------------

#[test]
fn render_no_match_message_when_empty() {
    let mut list = SelectList::new(items(&["a"]), 5, SelectListLayoutOptions::default());
    list.set_filter("zzz");
    assert_eq!(list.render(40, &theme()), vec!["  No matching commands"]);
}

#[test]
fn render_simple_list() {
    let list = SelectList::new(
        items(&["alpha", "beta"]),
        5,
        SelectListLayoutOptions::default(),
    );
    // Selected index defaults to 0 → first item uses the selected prefix.
    let lines = list.render(40, &theme());
    assert_eq!(lines, vec!["→ alpha", "  beta"]);
}

#[test]
fn render_selected_prefix() {
    let mut list = SelectList::new(
        items(&["alpha", "beta"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.set_selected_index(1);
    let lines = list.render(40, &theme());
    assert_eq!(lines, vec!["  alpha", "→ beta"]);
}

#[test]
fn render_scrolling_window_centers_selection() {
    let list = SelectList::new(
        items(&["a", "b", "c", "d", "e", "f", "g", "h"]),
        3,
        SelectListLayoutOptions::default(),
    );
    let mut inner = SelectList::new(
        items(&["a", "b", "c", "d", "e", "f", "g", "h"]),
        3,
        SelectListLayoutOptions::default(),
    );
    let _ = &mut inner;
    let lines = list.render(40, &theme());
    // Selected 0 → shows first 3 + scroll indicator.
    assert_eq!(lines.len(), 4);
    assert!(lines[3].contains("(1/8)"), "{lines:?}");
}

#[test]
fn render_scrolled_window_shows_selection_centered() {
    let mut list = SelectList::new(
        items(&["a", "b", "c", "d", "e", "f", "g", "h"]),
        3,
        SelectListLayoutOptions::default(),
    );
    list.set_selected_index(4);
    let lines = list.render(40, &theme());
    // start = 4 - floor(3/2) = 3, window d/e/f, indicator (5/8).
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains("d"), "{lines:?}");
    assert!(lines[1].contains("→ e"), "{lines:?}");
    assert!(lines[3].contains("(5/8)"), "{lines:?}");
}

#[test]
fn render_two_column_layout_with_description() {
    let list = SelectList::new(
        vec![SelectItem {
            value: "cmd".to_string(),
            label: "cmd".to_string(),
            description: Some("does things".to_string()),
        }],
        5,
        SelectListLayoutOptions::default(),
    );
    let lines = list.render(80, &theme());
    assert_eq!(lines.len(), 1);
    // Two columns: label padded to primary column width, then description.
    // Selected index defaults to 0 → selected prefix.
    let line = &lines[0];
    assert!(line.starts_with("→ cmd"), "{line:?}");
    assert!(line.contains("does things"), "{line:?}");
    // Description starts past the primary column.
    let desc_col = line.find("does things").unwrap();
    assert!(desc_col >= 32, "{line:?}");
}

#[test]
fn render_skips_two_column_when_narrow() {
    let list = SelectList::new(
        vec![SelectItem {
            value: "cmd".to_string(),
            label: "cmd".to_string(),
            description: Some("does things".to_string()),
        }],
        5,
        SelectListLayoutOptions::default(),
    );
    let lines = list.render(40, &theme());
    // width 40 is not > 40, so no two-column layout.
    let line = &lines[0];
    assert!(line.contains("cmd"), "{line:?}");
    assert!(!line.contains("does things"), "{line:?}");
}

#[test]
fn render_truncates_long_labels() {
    let list = SelectList::new(
        items(&["averyveryverylongcommandnamethatkeepsgoing"]),
        5,
        SelectListLayoutOptions::default(),
    );
    // max width = 20 - prefix(2) - 2 safety = 16 → line width 18.
    let lines = list.render(20, &theme());
    assert_eq!(visible_width(&lines[0]), 18);
}

#[test]
fn custom_truncate_primary_hook_receives_context() {
    let hook_seen = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, usize, bool)>));
    let hook = {
        let hook_seen = hook_seen.clone();
        move |ctx: pillar_tui::select_list::TruncatePrimaryContext<'_>| {
            *hook_seen.lock().unwrap() =
                Some((ctx.text.to_string(), ctx.max_width, ctx.is_selected));
            ctx.text.to_uppercase()
        }
    };
    let list = SelectList::new(
        items(&["deploy"]),
        5,
        SelectListLayoutOptions {
            truncate_primary: Some(Box::new(hook)),
            ..Default::default()
        },
    );
    let lines = list.render(40, &theme());
    // Selected index defaults to 0 → selected prefix.
    assert_eq!(lines, vec!["→ DEPLOY"]);
    let captured = hook_seen.lock().unwrap().take().unwrap();
    assert_eq!(captured.0, "deploy");
    assert!(captured.2);
}

#[test]
fn primary_column_respects_min_max_bounds() {
    // Widest item is 3+2=5 columns; min bound of 10 wins. Descriptions
    // force the two-column path where the primary width applies.
    let list = SelectList::new(
        vec![
            SelectItem {
                value: "ab".into(),
                label: "ab".into(),
                description: Some("d1".into()),
            },
            SelectItem {
                value: "cde".into(),
                label: "cde".into(),
                description: Some("d2".into()),
            },
        ],
        5,
        SelectListLayoutOptions {
            min_primary_column_width: Some(10),
            ..Default::default()
        },
    );
    let lines = list.render(80, &theme());
    for (index, line) in lines.iter().enumerate() {
        let desc = if index == 0 { "d1" } else { "d2" };
        let desc_col = line.find(desc).unwrap();
        // 2-col prefix + primary column ≥ 10 before the description.
        assert!(desc_col >= 2 + 10, "{lines:?}");
    }
}

#[test]
fn display_value_falls_back_to_value() {
    let list = SelectList::new(
        vec![SelectItem {
            value: "val".to_string(),
            label: String::new(),
            description: None,
        }],
        5,
        SelectListLayoutOptions::default(),
    );
    let lines = list.render(40, &theme());
    assert!(lines[0].contains("val"), "{lines:?}");
}

#[test]
fn get_selected_item_reflects_filter() {
    let mut list = SelectList::new(
        items(&["alpha", "beta"]),
        5,
        SelectListLayoutOptions::default(),
    );
    list.set_filter("be");
    assert_eq!(list.get_selected_item().unwrap().value, "beta");
    assert_eq!(
        list.get_selected_item().map(|i| i.value.as_str()),
        Some("beta")
    );
}

#[test]
fn description_newlines_collapsed() {
    let list = SelectList::new(
        vec![SelectItem {
            value: "cmd".to_string(),
            label: "cmd".to_string(),
            description: Some("line1\r\nline2\nline3".to_string()),
        }],
        5,
        SelectListLayoutOptions::default(),
    );
    let lines = list.render(80, &theme());
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("line1 line2 line3"), "{lines:?}");
}
