//! Port of the main-screen render decision core from
//! packages/tui/src/tui-main-screen.ts (pi v0.84.3): the differential
//! render decision tree, changed-line range computation, and Kitty
//! image reserved-row bookkeeping.
//!
//! divergences: the terminal write path (BoundedTerminalWriter chunk
//! streaming, synchronized-output wrapping, cursor movement escapes)
//! and the TuiBase scheduling stay host-side; the port exposes the
//! pure decision functions over render state.

use crate::terminal_image::{delete_kitty_image, is_image_line};
use crate::text_utils::visible_width;

/// Kitty image header extracted from a line (upstream
/// `KittyImageHeader`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImageHeader {
    pub ids: Vec<u32>,
    pub rows: usize,
}

/// Parse the Kitty image controls from a line (upstream
/// `parseKittyImageHeader`).
pub fn parse_kitty_image_header(line: &str) -> Option<KittyImageHeader> {
    const PREFIX: &str = "\u{1b}_G";
    let sequence_start = line.find(PREFIX)?;
    let params_start = sequence_start + PREFIX.len();
    let params_end = line[params_start..].find(';')? + params_start;

    let mut ids: Vec<u32> = Vec::new();
    let mut rows = 1usize;
    for param in line[params_start..params_end].split(',') {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        let Ok(number_value) = value.parse::<u32>() else {
            continue;
        };
        if number_value == 0 {
            continue;
        }
        if key == "i" {
            ids.push(number_value);
        } else if key == "r" {
            rows = number_value as usize;
        }
    }
    Some(KittyImageHeader { ids, rows })
}

/// Extract Kitty image ids from a line (upstream
/// `extractKittyImageIds`).
pub fn extract_kitty_image_ids(line: &str) -> Vec<u32> {
    parse_kitty_image_header(line)
        .map(|h| h.ids)
        .unwrap_or_default()
}

/// Extract the reserved row count from a line (upstream
/// `extractKittyImageRows`).
pub fn extract_kitty_image_rows(line: &str) -> usize {
    parse_kitty_image_header(line).map_or(1, |h| h.rows)
}

/// Delete escape sequences for the given image ids (upstream
/// `deleteKittyImages`).
pub fn delete_kitty_images(ids: &[u32]) -> String {
    ids.iter().map(|id| delete_kitty_image(*id)).collect()
}

/// Reserved rows for an image block starting at `index` (upstream
/// `getKittyImageReservedRows`): the declared row count bounded by the
/// remaining lines, extended only through empty rows.
pub fn get_kitty_image_reserved_rows(lines: &[String], index: usize, max_index: usize) -> usize {
    let rows = extract_kitty_image_rows(lines.get(index).map(String::as_str).unwrap_or(""));
    if rows <= 1 {
        return 1;
    }
    let max_rows = rows
        .min(max_index.saturating_sub(index) + 1)
        .min(lines.len() - index);
    let mut reserved_rows = 1usize;
    while reserved_rows < max_rows {
        let line = lines
            .get(index + reserved_rows)
            .map(String::as_str)
            .unwrap_or("");
        if is_image_line(line) || visible_width(line) > 0 {
            break;
        }
        reserved_rows += 1;
    }
    reserved_rows
}

/// Expand a changed-line range to cover whole Kitty image blocks
/// intersecting it (upstream `expandChangedRangeForKittyImages`).
pub fn expand_changed_range_for_kitty_images(
    first_changed: usize,
    last_changed: usize,
    previous_lines: &[String],
    new_lines: &[String],
) -> (usize, usize) {
    let mut expanded_first = first_changed;
    let mut expanded_last = last_changed;
    let max_index = new_lines.len().saturating_sub(1);
    for lines in [previous_lines, new_lines] {
        for index in 0..lines.len() {
            if extract_kitty_image_ids(&lines[index]).is_empty() {
                continue;
            }
            let block_end = index + get_kitty_image_reserved_rows(lines, index, max_index) - 1;
            if index >= first_changed || (index <= last_changed && block_end >= first_changed) {
                expanded_first = expanded_first.min(index);
                expanded_last = expanded_last.max(block_end);
            }
        }
    }
    (expanded_first, expanded_last)
}

/// Delete sequences for image ids in the changed range of the previous
/// lines (upstream `deleteChangedKittyImages`).
pub fn delete_changed_kitty_images(
    first_changed: usize,
    last_changed: usize,
    previous_lines: &[String],
) -> String {
    let mut ids = Vec::new();
    let max_line = last_changed.min(previous_lines.len().saturating_sub(1));
    for line in previous_lines.iter().take(max_line + 1).skip(first_changed) {
        for id in extract_kitty_image_ids(line) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    delete_kitty_images(&ids)
}

/// The render-mode decision (upstream the doRender decision tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderDecision {
    /// First render: output everything without clearing.
    FirstRender,
    /// Terminal width changed: full clear.
    WidthChanged,
    /// Terminal height changed (non-Termux): full clear.
    HeightChanged,
    /// Content shrunk and clearOnShrink is on with no overlays: full clear.
    ClearOnShrink,
    /// Differential update.
    Differential,
    /// Only deletions (previous lines exceed new lines).
    DeletedOnly,
    /// No changes at all.
    NoChanges,
}

/// Render state tracked between frames (upstream the private fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: usize,
    pub previous_height: usize,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: usize,
    pub has_overlays: bool,
    pub clear_on_shrink: bool,
    pub is_termux: bool,
}

impl Default for MainScreenRenderState {
    fn default() -> Self {
        Self {
            previous_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            has_overlays: false,
            clear_on_shrink: true,
            is_termux: false,
        }
    }
}

/// Decide how to render the new lines given the tracked state
/// (upstream the doRender decision tree).
pub fn decide_render(
    state: &MainScreenRenderState,
    new_lines: &[String],
    width: usize,
    height: usize,
) -> RenderDecision {
    let width_changed = state.previous_width != 0 && state.previous_width != width;
    let height_changed = state.previous_height != 0 && state.previous_height != height;

    // First render.
    if state.previous_lines.is_empty() && !width_changed && !height_changed {
        return RenderDecision::FirstRender;
    }
    if width_changed {
        return RenderDecision::WidthChanged;
    }
    if height_changed && !state.is_termux {
        return RenderDecision::HeightChanged;
    }
    if state.clear_on_shrink && new_lines.len() < state.max_lines_rendered && !state.has_overlays {
        return RenderDecision::ClearOnShrink;
    }
    let (first_changed, last_changed, appended) = changed_range(&state.previous_lines, new_lines);
    let _ = appended;
    if first_changed.is_none() {
        return RenderDecision::NoChanges;
    }
    let first_changed = first_changed.unwrap();
    if first_changed >= new_lines.len() {
        return RenderDecision::DeletedOnly;
    }
    // Differential rendering can only touch what was visible.
    if first_changed < state.previous_viewport_top {
        return RenderDecision::WidthChanged;
    }
    let _ = last_changed;
    RenderDecision::Differential
}

/// First/last changed line indices plus whether lines were appended
/// (upstream the changed-range loop).
pub fn changed_range(
    previous_lines: &[String],
    new_lines: &[String],
) -> (Option<usize>, Option<usize>, bool) {
    let mut first_changed: Option<usize> = None;
    let mut last_changed: Option<usize> = None;
    let max_lines = previous_lines.len().max(new_lines.len());
    for i in 0..max_lines {
        let old_line = previous_lines.get(i).map(String::as_str).unwrap_or("");
        let new_line = new_lines.get(i).map(String::as_str).unwrap_or("");
        if old_line != new_line {
            if first_changed.is_none() {
                first_changed = Some(i);
            }
            last_changed = Some(i);
        }
    }
    let appended = new_lines.len() > previous_lines.len();
    if appended {
        if first_changed.is_none() {
            first_changed = Some(previous_lines.len());
        }
        last_changed = Some(new_lines.len() - 1);
    }
    (first_changed, last_changed, appended)
}

/// Whether the append-only fast path applies (upstream `appendStart`):
/// appending from the very end of the previous content.
pub fn is_append_start(first_changed: usize, appended: bool, previous_lines: &[String]) -> bool {
    appended && first_changed == previous_lines.len() && first_changed > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- kitty header parsing ------------------------------------------------------------------

    #[test]
    fn parse_kitty_header_ids_and_rows() {
        let header =
            parse_kitty_image_header("\u{1b}_Ga=T,f=100,q=2,i=42,r=3;QUJD\u{1b}\\").unwrap();
        assert_eq!(header.ids, vec![42]);
        assert_eq!(header.rows, 3);
    }

    #[test]
    fn parse_kitty_header_defaults_rows_to_one() {
        let header = parse_kitty_image_header("\u{1b}_Ga=T,i=7;QUJD\u{1b}\\").unwrap();
        assert_eq!(header.ids, vec![7]);
        assert_eq!(header.rows, 1);
    }

    #[test]
    fn parse_kitty_header_rejects_non_image_lines() {
        assert!(parse_kitty_image_header("plain text").is_none());
        assert!(parse_kitty_image_header("\u{1b}_Gno-terminator").is_none());
    }

    #[test]
    fn extract_ids_and_rows_helpers() {
        assert_eq!(
            extract_kitty_image_ids("\u{1b}_Gi=1,i=2;X\u{1b}\\"),
            vec![1, 2]
        );
        assert_eq!(extract_kitty_image_rows("plain"), 1);
    }

    #[test]
    fn delete_kitty_images_builds_sequence() {
        assert_eq!(
            delete_kitty_images(&[3, 4]),
            "\u{1b}_Ga=d,d=I,i=3,q=2\u{1b}\\\u{1b}_Ga=d,d=I,i=4,q=2\u{1b}\\"
        );
    }

    // --- reserved rows ---------------------------------------------------------------------------

    #[test]
    fn reserved_rows_single_row_image() {
        let lines = lines(&["\u{1b}_Gi=1;X\u{1b}\\", "text"]);
        assert_eq!(get_kitty_image_reserved_rows(&lines, 0, 1), 1);
    }

    #[test]
    fn reserved_rows_extends_through_empty_rows() {
        let mut lines = lines(&["\u{1b}_Gi=1,r=4;X\u{1b}\\"]);
        lines.push(String::new());
        lines.push(String::new());
        lines.push(String::new());
        lines.push("text".to_string());
        // Declared 4 rows, empty rows confirmed.
        assert_eq!(get_kitty_image_reserved_rows(&lines, 0, 4), 4);
    }

    #[test]
    fn reserved_rows_stops_at_non_empty_row() {
        let mut lines = lines(&["\u{1b}_Gi=1,r=4;X\u{1b}\\"]);
        lines.push(String::new());
        lines.push("text".to_string());
        assert_eq!(get_kitty_image_reserved_rows(&lines, 0, 3), 2);
    }

    #[test]
    fn reserved_rows_bounded_by_max_index() {
        let mut lines = lines(&["\u{1b}_Gi=1,r=8;X\u{1b}\\"]);
        for _ in 0..4 {
            lines.push(String::new());
        }
        // Only 5 lines total → max 5 rows.
        assert_eq!(get_kitty_image_reserved_rows(&lines, 0, 4), 5);
    }

    // --- changed range ------------------------------------------------------------------------------

    #[test]
    fn changed_range_detects_middle_change() {
        let prev = lines(&["a", "b", "c"]);
        let new = lines(&["a", "X", "c"]);
        let (first, last, appended) = changed_range(&prev, &new);
        assert_eq!((first, last, appended), (Some(1), Some(1), false));
    }

    #[test]
    fn changed_range_appended_lines() {
        let prev = lines(&["a", "b"]);
        let new = lines(&["a", "b", "c", "d"]);
        let (first, last, appended) = changed_range(&prev, &new);
        assert_eq!((first, last, appended), (Some(2), Some(3), true));
    }

    #[test]
    fn changed_range_identical_lines() {
        let prev = lines(&["a", "b"]);
        let new = lines(&["a", "b"]);
        let (first, last, appended) = changed_range(&prev, &new);
        assert_eq!((first, last, appended), (None, None, false));
    }

    #[test]
    fn changed_range_shrunk_content() {
        let prev = lines(&["a", "b", "c"]);
        let new = lines(&["a"]);
        let (first, last, appended) = changed_range(&prev, &new);
        // Line 1 becomes "" (deleted), line 2 as well.
        assert_eq!((first, last, appended), (Some(1), Some(2), false));
    }

    #[test]
    fn append_start_only_from_exact_end() {
        let prev = lines(&["a", "b"]);
        assert!(is_append_start(2, true, &prev));
        assert!(!is_append_start(1, true, &prev));
        assert!(!is_append_start(2, false, &prev));
    }

    // --- range expansion ----------------------------------------------------------------------------

    #[test]
    fn expand_changed_range_covers_image_block() {
        let mut prev = lines(&["plain", "\u{1b}_Gi=1,r=3;X\u{1b}\\", "", "", "tail"]);
        prev[2] = String::new();
        prev[3] = String::new();
        let new = lines(&["plain", "\u{1b}_Gi=1,r=3;X\u{1b}\\", "", "", "tail"]);
        // Change only the tail (row 4): the image block at 1..=3 is
        // untouched, so the range stays.
        let (first, last) = expand_changed_range_for_kitty_images(4, 4, &prev, &new);
        assert_eq!((first, last), (4, 4));
    }

    #[test]
    fn expand_changed_range_includes_intersecting_block() {
        let mut prev = lines(&["plain", "\u{1b}_Gi=1,r=3;X\u{1b}\\", "", "", "tail"]);
        prev[2] = String::new();
        prev[3] = String::new();
        let new = lines(&["plain", "\u{1b}_Gi=1,r=3;X\u{1b}\\", "", "", "CHANGED"]);
        // Row 2 changed; the image block starting at 1 ends at 3, which
        // intersects the change → expanded to 1..=3.
        let (first, last) = expand_changed_range_for_kitty_images(2, 2, &prev, &new);
        assert_eq!((first, last), (1, 3));
    }

    // --- delete changed ------------------------------------------------------------------------------

    #[test]
    fn delete_changed_collects_ids_in_range() {
        let prev = lines(&[
            "\u{1b}_Gi=1;X\u{1b}\\",
            "\u{1b}_Gi=2;X\u{1b}\\",
            "\u{1b}_Gi=3;X\u{1b}\\",
        ]);
        let out = delete_changed_kitty_images(1, 2, &prev);
        assert!(out.contains("i=2"), "{out:?}");
        assert!(out.contains("i=3"), "{out:?}");
        assert!(!out.contains("i=1"), "{out:?}");
    }

    // --- decision tree --------------------------------------------------------------------------------

    fn state_with_previous(items: &[&str], width: usize, height: usize) -> MainScreenRenderState {
        MainScreenRenderState {
            previous_lines: lines(items),
            previous_width: width,
            previous_height: height,
            ..MainScreenRenderState::default()
        }
    }

    #[test]
    fn first_render_when_no_previous_lines() {
        let state = MainScreenRenderState::default();
        assert_eq!(
            decide_render(&state, &lines(&["new"]), 80, 24),
            RenderDecision::FirstRender
        );
    }

    #[test]
    fn width_change_requires_full_clear() {
        let state = state_with_previous(&["a"], 60, 24);
        assert_eq!(
            decide_render(&state, &lines(&["a"]), 80, 24),
            RenderDecision::WidthChanged
        );
    }

    #[test]
    fn height_change_requires_full_clear_unless_termux() {
        let state = state_with_previous(&["a"], 80, 24);
        assert_eq!(
            decide_render(&state, &lines(&["a"]), 80, 30),
            RenderDecision::HeightChanged
        );
        let termux = MainScreenRenderState {
            is_termux: true,
            ..state_with_previous(&["a"], 80, 24)
        };
        // Termux keeps the differential path across height toggles; with
        // changed content it diffs, with identical content nothing runs.
        assert_eq!(
            decide_render(&termux, &lines(&["changed"]), 80, 30),
            RenderDecision::Differential
        );
        assert_eq!(
            decide_render(&termux, &lines(&["a"]), 80, 30),
            RenderDecision::NoChanges
        );
    }

    #[test]
    fn clear_on_shrink_with_no_overlays() {
        // New content changed but is shorter than the high-water mark.
        let mut previous = vec!["a".to_string(); 10];
        previous[0] = "old".to_string();
        let state = MainScreenRenderState {
            max_lines_rendered: 10,
            previous_width: 80,
            previous_height: 24,
            previous_lines: previous,
            ..MainScreenRenderState::default()
        };
        assert_eq!(
            decide_render(&state, &lines(&["a"]), 80, 24),
            RenderDecision::ClearOnShrink
        );
        // Overlays keep the padding: differential.
        let with_overlays = MainScreenRenderState {
            has_overlays: true,
            ..state.clone()
        };
        assert_eq!(
            decide_render(&with_overlays, &lines(&["a"]), 80, 24),
            RenderDecision::Differential
        );
        // Disabled clear-on-shrink: differential.
        let disabled = MainScreenRenderState {
            clear_on_shrink: false,
            ..state
        };
        assert_eq!(
            decide_render(&disabled, &lines(&["a"]), 80, 24),
            RenderDecision::Differential
        );
    }

    #[test]
    fn no_changes_decision() {
        let state = state_with_previous(&["a", "b"], 80, 24);
        assert_eq!(
            decide_render(&state, &lines(&["a", "b"]), 80, 24),
            RenderDecision::NoChanges
        );
    }

    #[test]
    fn deleted_only_decision() {
        let state = state_with_previous(&["a", "b", "c"], 80, 24);
        assert_eq!(
            decide_render(&state, &lines(&["a"]), 80, 24),
            RenderDecision::DeletedOnly
        );
    }

    #[test]
    fn change_above_viewport_requires_full_redraw() {
        let state = MainScreenRenderState {
            previous_lines: lines(&["a", "b", "c"]),
            previous_width: 80,
            previous_height: 2,
            previous_viewport_top: 2,
            ..MainScreenRenderState::default()
        };
        // Line 0 changed, which is above the previous viewport top.
        assert_eq!(
            decide_render(&state, &lines(&["X", "b", "c"]), 80, 2),
            RenderDecision::WidthChanged
        );
    }

    #[test]
    fn differential_for_visible_change() {
        let state = state_with_previous(&["a", "b"], 80, 24);
        assert_eq!(
            decide_render(&state, &lines(&["a", "X"]), 80, 24),
            RenderDecision::Differential
        );
    }
}
