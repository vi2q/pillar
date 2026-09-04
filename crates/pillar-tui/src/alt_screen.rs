//! Port of the alt-screen render decision core from
//! packages/tui/src/tui-alt-screen.ts (pi v0.84.3): the screen diff —
//! full redraw vs row-local updates, image redraw detection, Kitty
//! offscreen cache eviction, and per-row update emission.
//!
//! divergences: the layout pipeline, search/selection highlights,
//! flash compositing, mouse handling, and the terminal write path stay
//! host-side; the port exposes the diff decisions over plain line
//! vectors. Kitty placement replacement is approximated: the upstream
//! rewrites transmission into placement-only commands via
//! getKittyImagePlacement, which the port leaves to the host because
//! transmission bookkeeping lives with the image uploader.

use std::collections::BTreeMap;

use crate::terminal_image::{delete_all_kitty_placements, delete_kitty_image, is_image_line};

const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 5;
const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 30 * 1024 * 1024;
const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: usize = 300 * 1024 * 1024;

/// Cached Kitty upload bookkeeping (upstream `CachedKittyImage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedKittyImage {
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: usize,
}

/// Result of preparing the Kitty screen (upstream the
/// prepareKittyScreen result).
pub struct PreparedKittyScreen {
    pub lines: Vec<String>,
    pub evicted_image_deletion: String,
}

/// Uploaded-image registry with offscreen eviction (upstream
/// `prepareKittyScreen`). `visible_image_ids` are the ids present on
/// the new screen; the registry evicts the oldest offscreen uploads
/// until all three budget metrics fit.
pub fn prepare_kitty_screen(
    screen: &[String],
    uploaded: &mut BTreeMap<u32, CachedKittyImage>,
    placement_generations: impl Fn(&str) -> Option<(u32, u64)>,
) -> PreparedKittyScreen {
    let mut visible_image_ids = std::collections::HashSet::new();
    let mut lines: Vec<String> = Vec::with_capacity(screen.len());
    for line in screen {
        let Some((image_id, generation)) = placement_generations(line) else {
            lines.push(line.clone());
            continue;
        };
        visible_image_ids.insert(image_id);
        let cached = uploaded.remove(&image_id);
        uploaded.insert(
            image_id,
            CachedKittyImage {
                transmission_generation: generation,
                transmission_bytes: cached.map(|c| c.transmission_bytes).unwrap_or(0),
                estimated_decoded_bytes: cached.map(|c| c.estimated_decoded_bytes).unwrap_or(0),
            },
        );
        // Same generation → only the placement needs re-emitting; the
        // transmission line is replaced host-side (kept verbatim here).
        lines.push(line.clone());
    }

    let mut cached_offscreen_image_count = 0usize;
    let mut cached_offscreen_transmission_bytes = 0usize;
    let mut cached_offscreen_decoded_bytes = 0usize;
    for (image_id, cached) in uploaded.iter() {
        if visible_image_ids.contains(image_id) {
            continue;
        }
        cached_offscreen_image_count += 1;
        cached_offscreen_transmission_bytes += cached.transmission_bytes;
        cached_offscreen_decoded_bytes += cached.estimated_decoded_bytes;
    }

    let mut evicted_image_deletion = String::new();
    let ids: Vec<u32> = uploaded.keys().copied().collect();
    for image_id in ids {
        if cached_offscreen_image_count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
            && cached_offscreen_transmission_bytes <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
            && cached_offscreen_decoded_bytes <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
        {
            break;
        }
        if visible_image_ids.contains(&image_id) {
            continue;
        }
        if let Some(cached) = uploaded.remove(&image_id) {
            evicted_image_deletion.push_str(&delete_kitty_image(image_id));
            cached_offscreen_image_count = cached_offscreen_image_count.saturating_sub(1);
            cached_offscreen_transmission_bytes =
                cached_offscreen_transmission_bytes.saturating_sub(cached.transmission_bytes);
            cached_offscreen_decoded_bytes =
                cached_offscreen_decoded_bytes.saturating_sub(cached.estimated_decoded_bytes);
        }
    }
    PreparedKittyScreen {
        lines,
        evicted_image_deletion,
    }
}

/// The alt-screen diff decision (upstream the doRender body's update
/// classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltScreenDiff {
    /// Full clear + all rows: first render or a dimension change.
    FullRedraw,
    /// Same rows, but image rows changed: clear images and repaint.
    ImagesNeedRedraw,
    /// Row-local updates only.
    RowUpdates,
}

/// Classify the diff between the previous screen and the new screen
/// (upstream the fullRedraw/imagesNeedRedraw computation).
pub fn classify_alt_screen_diff(
    previous_screen: &[String],
    previous_width: usize,
    previous_height: usize,
    screen: &[String],
    width: usize,
    height: usize,
) -> AltScreenDiff {
    let full_redraw =
        previous_screen.is_empty() || previous_width != width || previous_height != height;
    if full_redraw {
        return AltScreenDiff::FullRedraw;
    }
    let images_need_redraw = screen.iter().enumerate().any(|(row, line)| {
        *line != previous_screen.get(row).map(String::as_str).unwrap_or("")
            && (is_image_line(line) || previous_screen.get(row).is_some_and(|l| is_image_line(l)))
    });
    if images_need_redraw {
        AltScreenDiff::ImagesNeedRedraw
    } else {
        AltScreenDiff::RowUpdates
    }
}

/// The rows that need repainting (upstream the per-row skip loop):
/// every row when redrawing fully, otherwise only changed rows.
pub fn rows_to_paint(
    diff: AltScreenDiff,
    previous_screen: &[String],
    screen: &[String],
    height: usize,
) -> Vec<usize> {
    match diff {
        AltScreenDiff::FullRedraw | AltScreenDiff::ImagesNeedRedraw => (0..height).collect(),
        AltScreenDiff::RowUpdates => (0..height.min(screen.len()))
            .filter(|row| {
                screen[*row] != previous_screen.get(*row).map(String::as_str).unwrap_or("")
            })
            .collect(),
    }
}

/// The clear-images prefix for a diff (upstream the clearImages /
/// imagesNeedRedraw branch).
pub fn image_clear_prefix(
    diff: AltScreenDiff,
    image_protocol_kitty: bool,
    image_protocol_iterm2: bool,
    had_uploaded_kitty_images: bool,
) -> String {
    match diff {
        AltScreenDiff::FullRedraw => {
            if image_protocol_kitty && had_uploaded_kitty_images {
                delete_all_kitty_placements()
            } else {
                String::new()
            }
        }
        AltScreenDiff::ImagesNeedRedraw => {
            if image_protocol_iterm2 {
                "\u{1b}[2J".to_string()
            } else if image_protocol_kitty {
                delete_all_kitty_placements()
            } else {
                String::new()
            }
        }
        AltScreenDiff::RowUpdates => String::new(),
    }
}

/// The hardware-cursor positioning suffix (upstream the cursorPos
/// branch): absolute position + show/hide.
pub fn cursor_position_suffix(
    cursor_pos: Option<(usize, usize)>,
    width: usize,
    show_hardware_cursor: bool,
) -> String {
    match cursor_pos {
        Some((row, col)) => {
            let mut out = format!("\u{1b}[{};{}H", row + 1, width.min(col) + 1);
            out.push_str(if show_hardware_cursor {
                "\u{1b}[?25h"
            } else {
                "\u{1b}[?25l"
            });
            out
        }
        None => "\u{1b}[?25l".to_string(),
    }
}

/// Emit a row-update escape for one row (upstream the per-row write):
/// move to row, clear line, write content.
pub fn row_update_escape(row: usize, line: &str) -> String {
    format!("\u{1b}[{};1H\u{1b}[2K{line}", row + 1)
}

/// Whether an offscreen image exceeds the eviction budgets (upstream
/// the budget check).
pub fn offscreen_within_budget(
    image_count: usize,
    transmission_bytes: usize,
    decoded_bytes: usize,
) -> bool {
    image_count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
        && transmission_bytes <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
        && decoded_bytes <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- diff classification -----------------------------------------------------------------

    #[test]
    fn first_render_is_full_redraw() {
        let screen = lines(&["a", "b"]);
        assert_eq!(
            classify_alt_screen_diff(&[], 0, 0, &screen, 80, 24),
            AltScreenDiff::FullRedraw
        );
    }

    #[test]
    fn width_change_is_full_redraw() {
        let prev = lines(&["a", "b"]);
        let screen = lines(&["a", "b"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 60, 24, &screen, 80, 24),
            AltScreenDiff::FullRedraw
        );
    }

    #[test]
    fn height_change_is_full_redraw() {
        let prev = lines(&["a", "b"]);
        let screen = lines(&["a", "b"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 80, 10, &screen, 80, 24),
            AltScreenDiff::FullRedraw
        );
    }

    #[test]
    fn unchanged_screen_is_row_updates_with_no_rows() {
        let prev = lines(&["a", "b"]);
        let screen = lines(&["a", "b"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 80, 24, &screen, 80, 24),
            AltScreenDiff::RowUpdates
        );
        assert!(rows_to_paint(AltScreenDiff::RowUpdates, &prev, &screen, 24).is_empty());
    }

    #[test]
    fn changed_text_row_paints_only_that_row() {
        let prev = lines(&["a", "b", "c"]);
        let screen = lines(&["a", "X", "c"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 80, 24, &screen, 80, 24),
            AltScreenDiff::RowUpdates
        );
        assert_eq!(
            rows_to_paint(AltScreenDiff::RowUpdates, &prev, &screen, 24),
            vec![1]
        );
    }

    #[test]
    fn image_line_change_triggers_image_redraw() {
        let prev = lines(&["a", "b"]);
        let screen = lines(&["a", "\u{1b}_Gi=1;X\u{1b}\\"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 80, 24, &screen, 80, 24),
            AltScreenDiff::ImagesNeedRedraw
        );
        // Image diffs repaint every row.
        assert_eq!(
            rows_to_paint(AltScreenDiff::ImagesNeedRedraw, &prev, &screen, 2),
            vec![0, 1]
        );
    }

    #[test]
    fn image_removal_also_triggers_image_redraw() {
        let prev = lines(&["a", "\u{1b}_Gi=1;X\u{1b}\\"]);
        let screen = lines(&["a", "b"]);
        assert_eq!(
            classify_alt_screen_diff(&prev, 80, 24, &screen, 80, 24),
            AltScreenDiff::ImagesNeedRedraw
        );
    }

    // --- clear prefixes -------------------------------------------------------------------------

    #[test]
    fn full_redraw_kitty_with_uploads_clears_placements() {
        let prefix = image_clear_prefix(AltScreenDiff::FullRedraw, true, false, true);
        assert_eq!(prefix, "\u{1b}_Ga=d,d=a,q=2\u{1b}\\");
    }

    #[test]
    fn full_redraw_without_kitty_uploads_clears_nothing() {
        assert_eq!(
            image_clear_prefix(AltScreenDiff::FullRedraw, true, false, false),
            ""
        );
        assert_eq!(
            image_clear_prefix(AltScreenDiff::FullRedraw, false, false, true),
            ""
        );
    }

    #[test]
    fn image_redraw_uses_protocol_specific_clear() {
        assert_eq!(
            image_clear_prefix(AltScreenDiff::ImagesNeedRedraw, false, true, true),
            "\u{1b}[2J"
        );
        assert_eq!(
            image_clear_prefix(AltScreenDiff::ImagesNeedRedraw, true, false, false),
            "\u{1b}_Ga=d,d=a,q=2\u{1b}\\"
        );
        assert_eq!(
            image_clear_prefix(AltScreenDiff::ImagesNeedRedraw, false, false, true),
            ""
        );
    }

    #[test]
    fn row_updates_clear_nothing() {
        assert_eq!(
            image_clear_prefix(AltScreenDiff::RowUpdates, true, true, true),
            ""
        );
    }

    // --- cursor suffix -----------------------------------------------------------------------------

    #[test]
    fn cursor_suffix_positions_and_shows() {
        let out = cursor_position_suffix(Some((2, 5)), 80, true);
        assert_eq!(out, "\u{1b}[3;6H\u{1b}[?25h");
    }

    #[test]
    fn cursor_suffix_hides_when_disabled() {
        let out = cursor_position_suffix(Some((0, 0)), 80, false);
        assert_eq!(out, "\u{1b}[1;1H\u{1b}[?25l");
    }

    #[test]
    fn no_cursor_hides() {
        assert_eq!(cursor_position_suffix(None, 80, true), "\u{1b}[?25l");
    }

    #[test]
    fn cursor_col_clamped_to_width() {
        // Upstream: Math.min(width, col) + 1 → 1-indexed col 81 for a
        // clamped col 80.
        let out = cursor_position_suffix(Some((0, 999)), 80, false);
        assert_eq!(out, "\u{1b}[1;81H\u{1b}[?25l");
    }

    // --- row emission --------------------------------------------------------------------------------

    #[test]
    fn row_update_escape_targets_row() {
        assert_eq!(row_update_escape(4, "hello"), "\u{1b}[5;1H\u{1b}[2Khello");
    }

    // --- kitty cache eviction --------------------------------------------------------------------------

    fn generation_of(line: &str) -> Option<(u32, u64)> {
        // Extract i= and treat a g= control as the generation if present.
        let sequence = line.find("\u{1b}_G")? + 3;
        let end = line[sequence..].find(';')? + sequence;
        let mut image_id = None;
        let mut generation = 0u64;
        for param in line[sequence..end].split(',') {
            if let Some(v) = param.strip_prefix("i=") {
                image_id = v.parse().ok();
            } else if let Some(v) = param.strip_prefix("g=") {
                generation = v.parse().unwrap_or(0);
            }
        }
        image_id.map(|id| (id, generation))
    }

    #[test]
    fn prepare_kitty_screen_keeps_lines_verbatim() {
        let mut uploaded = BTreeMap::new();
        let screen = lines(&["\u{1b}_Ga=T,i=1,g=1;abc\u{1b}\\", "text"]);
        let prepared = prepare_kitty_screen(&screen, &mut uploaded, generation_of);
        assert_eq!(prepared.lines, screen);
        assert_eq!(prepared.evicted_image_deletion, "");
        // Upload bookkeeping registered the image.
        assert!(uploaded.contains_key(&1));
    }

    #[test]
    fn prepare_kitty_screen_evicts_offscreen_uploads_over_budget() {
        let mut uploaded = BTreeMap::new();
        // Seed 6 offscreen uploads (budget is 5).
        for id in 1..=6u32 {
            uploaded.insert(
                id,
                CachedKittyImage {
                    transmission_generation: 1,
                    transmission_bytes: 100,
                    estimated_decoded_bytes: 1000,
                },
            );
        }
        // New screen shows only image 7 (never uploaded) → 6 offscreen
        // uploads over the budget of 5.
        let screen = lines(&["\u{1b}_Ga=T,i=7,g=2;abc\u{1b}\\"]);
        let prepared = prepare_kitty_screen(&screen, &mut uploaded, generation_of);
        // Oldest offscreen images evicted until the count fits.
        assert!(!uploaded.contains_key(&1), "{uploaded:?}");
        assert!(uploaded.contains_key(&7));
        assert!(
            prepared.evicted_image_deletion.contains("i=1"),
            "{}",
            prepared.evicted_image_deletion
        );
    }

    #[test]
    fn prepare_kitty_screen_never_evicts_visible_images() {
        let mut uploaded = BTreeMap::new();
        // Six uploads, all visible on the new screen.
        let mut screen = Vec::new();
        for id in 1..=6u32 {
            uploaded.insert(
                id,
                CachedKittyImage {
                    transmission_generation: 1,
                    transmission_bytes: 100,
                    estimated_decoded_bytes: 1000,
                },
            );
            screen.push(format!("\u{1b}_Ga=T,i={id},g=2;abc\u{1b}\\"));
        }
        let prepared = prepare_kitty_screen(&screen, &mut uploaded, generation_of);
        assert_eq!(uploaded.len(), 6);
        assert_eq!(prepared.evicted_image_deletion, "");
    }

    #[test]
    fn offscreen_budget_limits() {
        assert!(offscreen_within_budget(5, 0, 0));
        assert!(!offscreen_within_budget(6, 0, 0));
        assert!(!offscreen_within_budget(0, 30 * 1024 * 1024 + 1, 0));
        assert!(!offscreen_within_budget(0, 0, 300 * 1024 * 1024 + 1));
    }
}
