//! Port of the overlay layout decision core from packages/tui/src/tui.ts
//! (pi v0.84.3): `resolveOverlayLayout` — size/position resolution with
//! anchors, margins, percentage sizes, offsets, and clamping — plus
//! `compositeOverlays` line compositing.
//!
//! divergences: the TuiBase lifecycle (render scheduling, terminal
//! wiring, focus/overlay-stack focus restore, OSC 11 / color-scheme /
//! cell-size query handling) stays host-side; the port exposes the
//! pure layout math and compositing the upstream tests exercise.

use crate::stack_layout::composite_tui_line;
use crate::stack_layout::slice_by_column;
use crate::text_utils::visible_width;

/// Overlay anchor (upstream `OverlayAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

impl OverlayAnchor {
    pub fn parse(s: &str) -> Self {
        match s {
            "top-left" => Self::TopLeft,
            "top-right" => Self::TopRight,
            "bottom-left" => Self::BottomLeft,
            "bottom-right" => Self::BottomRight,
            "top-center" => Self::TopCenter,
            "bottom-center" => Self::BottomCenter,
            "left-center" => Self::LeftCenter,
            "right-center" => Self::RightCenter,
            _ => Self::Center,
        }
    }
}

/// Absolute number or percentage string like "50%" (upstream
/// `SizeValue`).
#[derive(Debug, Clone, PartialEq)]
pub enum SizeValue {
    Absolute(usize),
    /// Percentage stored ×10 for the decimal part (e.g. 12.5% → 125).
    Percent(u32),
}

impl SizeValue {
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim().trim_end_matches('%');
        if value.trim().ends_with('%') {
            let percent: f64 = trimmed.parse().ok()?;
            if percent < 0.0 {
                return None;
            }
            Some(Self::Percent((percent * 10.0).round() as u32))
        } else {
            let n: usize = trimmed.parse().ok()?;
            Some(Self::Absolute(n))
        }
    }

    fn resolve(&self, reference: usize) -> usize {
        match self {
            Self::Absolute(n) => *n,
            Self::Percent(tenths) => (reference * *tenths as usize) / 1000,
        }
    }

    /// Resolve against a reference size (upstream `parseSizeValue`).
    pub fn resolve_size(&self, reference: usize) -> usize {
        self.resolve(reference)
    }
}

/// Overlay margins (upstream `OverlayMargin`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: isize,
    pub right: isize,
    pub bottom: isize,
    pub left: isize,
}

/// Overlay positioning and sizing options (upstream `OverlayOptions`).
#[derive(Clone, Default)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    pub max_height: Option<SizeValue>,
    pub anchor: Option<OverlayAnchor>,
    pub offset_x: isize,
    pub offset_y: isize,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: Option<OverlayMargin>,
    /// Only render when this answers true for the terminal size (upstream
    /// `options.visible`).
    pub visible: Option<std::sync::Arc<dyn Fn(usize, usize) -> bool + Send + Sync>>,
    /// Do not capture keyboard focus when shown (upstream `nonCapturing`).
    pub non_capturing: bool,
}

impl std::fmt::Debug for OverlayOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OverlayOptions")
            .field("width", &self.width)
            .field("min_width", &self.min_width)
            .field("max_height", &self.max_height)
            .field("anchor", &self.anchor)
            .field("offset_x", &self.offset_x)
            .field("offset_y", &self.offset_y)
            .field("row", &self.row)
            .field("col", &self.col)
            .field("margin", &self.margin)
            .field("visible", &self.visible.is_some())
            .field("non_capturing", &self.non_capturing)
            .finish()
    }
}

impl OverlayOptions {
    /// The effective anchor (upstream defaults to `center`).
    pub fn resolved_anchor(&self) -> OverlayAnchor {
        self.anchor.unwrap_or_default()
    }

    /// Whether the overlay is visible at this terminal size (upstream calls
    /// `options.visible` each render).
    pub fn is_visible(&self, term_width: usize, term_height: usize) -> bool {
        match &self.visible {
            Some(visible) => visible(term_width, term_height),
            None => true,
        }
    }
}

/// Resolved overlay layout (upstream the resolveOverlayLayout result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOverlayLayout {
    pub width: usize,
    pub row: usize,
    pub col: usize,
    pub max_height: Option<usize>,
}

fn resolve_anchor_row(
    anchor: OverlayAnchor,
    height: usize,
    avail_height: usize,
    margin_top: usize,
) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin_top,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin_top + avail_height.saturating_sub(height)
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin_top + (avail_height.saturating_sub(height)) / 2
        }
    }
}

fn resolve_anchor_col(
    anchor: OverlayAnchor,
    width: usize,
    avail_width: usize,
    margin_left: usize,
) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
            margin_left
        }
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin_left + avail_width.saturating_sub(width)
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin_left + (avail_width.saturating_sub(width)) / 2
        }
    }
}

/// Resolve overlay layout from options (upstream `resolveOverlayLayout`).
pub fn resolve_overlay_layout(
    options: Option<&OverlayOptions>,
    overlay_height: usize,
    term_width: usize,
    term_height: usize,
) -> ResolvedOverlayLayout {
    let default = OverlayOptions::default();
    let opt = options.unwrap_or(&default);

    let margin = opt.margin.unwrap_or_default();
    let margin_top = margin.top.max(0) as usize;
    let margin_right = margin.right.max(0) as usize;
    let margin_bottom = margin.bottom.max(0) as usize;
    let margin_left = margin.left.max(0) as usize;

    let avail_width = term_width.saturating_sub(margin_left + margin_right).max(1);
    let avail_height = term_height
        .saturating_sub(margin_top + margin_bottom)
        .max(1);

    // Width: percentage/absolute, then minWidth, clamped to available.
    let mut width = match &opt.width {
        Some(value) => value.resolve(term_width),
        None => 80.min(avail_width),
    };
    if let Some(min_width) = opt.min_width {
        width = width.max(min_width);
    }
    width = width.clamp(1, avail_width);

    // Max height clamped to available.
    let max_height = opt
        .max_height
        .as_ref()
        .map(|value| value.resolve(term_height).clamp(1, avail_height));

    let effective_height = match max_height {
        Some(max) => overlay_height.min(max),
        None => overlay_height,
    };

    // Row.
    let row = match &opt.row {
        Some(SizeValue::Percent(tenths)) => {
            let max_row = avail_height.saturating_sub(effective_height);
            margin_top + (max_row * *tenths as usize) / 1000
        }
        Some(SizeValue::Absolute(n)) => *n,
        None => {
            let anchor = opt.anchor.unwrap_or_default();
            resolve_anchor_row(anchor, effective_height, avail_height, margin_top)
        }
    };

    // Col.
    let col = match &opt.col {
        Some(SizeValue::Percent(tenths)) => {
            let max_col = avail_width.saturating_sub(width);
            margin_left + (max_col * *tenths as usize) / 1000
        }
        Some(SizeValue::Absolute(n)) => *n,
        None => {
            let anchor = opt.anchor.unwrap_or_default();
            resolve_anchor_col(anchor, width, avail_width, margin_left)
        }
    };

    // Offsets + clamp to terminal bounds (respecting margins).
    let row_signed = row as isize + opt.offset_y;
    let col_signed = col as isize + opt.offset_x;
    let row_min = margin_top as isize;
    let row_max = (term_height.saturating_sub(margin_bottom + effective_height)) as isize;
    let col_min = margin_left as isize;
    let col_max = (term_width.saturating_sub(margin_right + width)) as isize;
    let row = row_signed.clamp(row_min, row_max.max(row_min)) as usize;
    let col = col_signed.clamp(col_min, col_max.max(col_min)) as usize;

    ResolvedOverlayLayout {
        width,
        row,
        col,
        max_height,
    }
}

/// One overlay prepared for compositing (upstream the `rendered`
/// entries).
pub struct PreparedOverlay<'a> {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub component: &'a OverlayOptions,
}

/// Prepare an overlay's lines and position (upstream the first loop of
/// `compositeOverlays`): resolve width/maxHeight, render, truncate to
/// maxHeight, re-resolve the position with the final height.
pub fn prepare_overlay(
    options: Option<&OverlayOptions>,
    overlay_lines: Vec<String>,
    term_width: usize,
    term_height: usize,
) -> (Vec<String>, ResolvedOverlayLayout) {
    let first = resolve_overlay_layout(options, 0, term_width, term_height);
    let mut lines = overlay_lines;
    if let Some(max_height) = first.max_height {
        if lines.len() > max_height {
            lines.truncate(max_height);
        }
    }
    let second = resolve_overlay_layout(options, lines.len(), term_width, term_height);
    (lines, second)
}

/// Composite prepared overlays over base lines, sorted by focus order
/// (higher later = on top) (upstream `compositeOverlays`).
pub fn composite_overlays(
    base_lines: Vec<String>,
    overlays: &mut [(Vec<String>, ResolvedOverlayLayout)],
    term_width: usize,
    term_height: usize,
) -> Vec<String> {
    if overlays.is_empty() {
        return base_lines;
    }
    let mut result = base_lines;
    let mut min_lines_needed = result.len();
    for (lines, layout) in overlays.iter() {
        min_lines_needed = min_lines_needed.max(layout.row + lines.len());
    }
    // Pad to at least terminal height so overlays have screen-relative
    // positions.
    let working_height = result.len().max(term_height).max(min_lines_needed);
    while result.len() < working_height {
        result.push(String::new());
    }
    let viewport_start = working_height.saturating_sub(term_height);

    for (lines, layout) in overlays {
        for (index, overlay_line) in lines.iter().enumerate() {
            let idx = viewport_start + layout.row + index;
            if idx < result.len() {
                // Truncate overlay line to declared width before
                // compositing.
                let truncated = if visible_width(overlay_line) > layout.width {
                    slice_by_column(overlay_line, 0, layout.width, true)
                } else {
                    overlay_line.clone()
                };
                result[idx] = composite_tui_line(
                    &result[idx],
                    &truncated,
                    layout.col,
                    layout.width,
                    term_width,
                );
            }
        }
    }
    result
}

/// Extract and strip the cursor marker from rendered lines, scanning
/// the bottom `height` lines (upstream `extractCursorPosition`).
pub fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    const CURSOR_MARKER: &str = "\u{1b}_pi:c\u{7}";
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        let line = &lines[row];
        if let Some(marker_index) = line.find(CURSOR_MARKER) {
            let col = visible_width(&line[..marker_index]);
            let stripped = format!(
                "{}{}",
                &line[..marker_index],
                &line[marker_index + CURSOR_MARKER.len()..]
            );
            lines[row] = stripped;
            return Some((row, col));
        }
    }
    None
}

/// Apply trailing segment resets per line (upstream `applyLineResets`).
pub fn apply_line_resets(lines: Vec<String>) -> Vec<String> {
    const SEGMENT_RESET: &str = "\u{1b}[0m\u{1b}]8;;\u{7}";
    lines
        .into_iter()
        .map(|line| {
            if crate::terminal_image::is_image_line(&line) {
                line
            } else {
                format!("{line}{SEGMENT_RESET}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_center_positions_centered() {
        // 80-col terminal, width 10, height 1 → col 35, row 11.
        let layout = resolve_overlay_layout(None, 1, 80, 24);
        // Default width = min(80, available).
        assert_eq!(layout.width, 80);
        assert_eq!(layout.col, 0);
        assert_eq!(layout.row, 11);
        // With an explicit narrow width the overlay centers horizontally.
        let options = OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!(layout.col, 35);
    }

    #[test]
    fn anchor_top_left() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::TopLeft),
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!((layout.row, layout.col), (0, 0));
    }

    #[test]
    fn anchor_bottom_right_ends_at_edge() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::BottomRight),
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!(layout.row, 23);
        assert_eq!(layout.col, 70);
    }

    #[test]
    fn width_percentage() {
        let options = OverlayOptions {
            width: SizeValue::parse("50%"),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 100, 24);
        assert_eq!(layout.width, 50);
    }

    #[test]
    fn min_width_wins_over_smaller_percentage() {
        let options = OverlayOptions {
            width: SizeValue::parse("10%"),
            min_width: Some(30),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 100, 24);
        assert_eq!(layout.width, 30);
    }

    #[test]
    fn margin_number_offsets_position() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::TopLeft),
            width: Some(SizeValue::Absolute(10)),
            margin: Some(OverlayMargin {
                top: 5,
                right: 5,
                bottom: 5,
                left: 5,
            }),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!((layout.row, layout.col), (5, 5));
    }

    #[test]
    fn negative_margins_clamped_to_zero() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::TopLeft),
            width: Some(SizeValue::Absolute(12)),
            margin: Some(OverlayMargin {
                top: -5,
                left: -10,
                ..Default::default()
            }),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!((layout.row, layout.col), (0, 0));
    }

    #[test]
    fn offsets_shift_from_anchor() {
        let options = OverlayOptions {
            anchor: Some(OverlayAnchor::TopLeft),
            width: Some(SizeValue::Absolute(10)),
            offset_x: 3,
            offset_y: 2,
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!((layout.row, layout.col), (2, 3));
    }

    #[test]
    fn row_col_override_anchor() {
        let options = OverlayOptions {
            width: Some(SizeValue::Absolute(20)),
            row: Some(SizeValue::Absolute(4)),
            col: Some(SizeValue::Absolute(60)),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!((layout.row, layout.col), (4, 60));
    }

    #[test]
    fn row_percentage_zero_is_top_hundred_is_bottom() {
        let options = OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: SizeValue::parse("0%"),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!(layout.row, 0);
        let options = OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: SizeValue::parse("100%"),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 1, 80, 24);
        assert_eq!(layout.row, 23);
    }

    #[test]
    fn max_height_truncates() {
        let options = OverlayOptions {
            max_height: SizeValue::parse("50%"),
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 20, 80, 24);
        assert_eq!(layout.max_height, Some(12));
        let (lines, _) = prepare_overlay(
            Some(&options),
            (0..20).map(|i| format!("l{i}")).collect(),
            80,
            24,
        );
        assert_eq!(lines.len(), 12);
    }

    #[test]
    fn max_height_percentage() {
        let options = OverlayOptions {
            max_height: SizeValue::parse("50%"),
            ..Default::default()
        };
        let layout = resolve_overlay_layout(Some(&options), 30, 80, 24);
        assert_eq!(layout.max_height, Some(12));
    }

    #[test]
    fn compositing_truncates_overwide_overlay_lines() {
        let base = vec![String::new(); 24];
        let layout = ResolvedOverlayLayout {
            width: 5,
            row: 0,
            col: 0,
            max_height: None,
        };
        let overlays = vec![(vec!["X".repeat(50)], layout)];
        let result = composite_overlays(base, &mut overlays.clone(), 80, 24);
        let line = &result[0];
        assert!(visible_width(line) <= 80, "{line:?}");
    }

    #[test]
    fn compositing_pads_to_terminal_height() {
        let base = vec!["content".to_string()];
        let layout = ResolvedOverlayLayout {
            width: 5,
            row: 20,
            col: 0,
            max_height: None,
        };
        let overlays = vec![(vec!["ov".to_string()], layout)];
        let result = composite_overlays(base, &mut overlays.clone(), 80, 24);
        assert_eq!(result.len(), 24);
        assert!(result[20].contains("ov"), "{:?}", result[20]);
    }

    #[test]
    fn extract_cursor_position_finds_and_strips_marker() {
        let marker = "\u{1b}_pi:c\u{7}";
        let mut lines = vec![format!("ab{marker}c"), "other".to_string()];
        let pos = extract_cursor_position(&mut lines, 24);
        assert_eq!(pos, Some((0, 2)));
        assert_eq!(lines[0], "abc");
        assert!(!lines[0].contains(marker));
    }

    #[test]
    fn extract_cursor_position_scans_only_bottom_viewport() {
        let marker = "\u{1b}_pi:c\u{7}";
        let mut lines = vec![
            format!("{marker}top"),
            "mid".to_string(),
            format!("{marker}bot"),
        ];
        let pos = extract_cursor_position(&mut lines, 2);
        // Bottom 2 rows only → the row-2 marker.
        assert_eq!(pos, Some((2, 0)));
        assert!(lines[0].contains(marker));
    }

    #[test]
    fn line_resets_appended() {
        let result = apply_line_resets(vec!["plain".to_string()]);
        assert_eq!(result[0], "plain\u{1b}[0m\u{1b}]8;;\u{7}");
    }
}
