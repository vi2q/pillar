//! Port of packages/tui/src layout helpers (pi v0.84.3): sliceByColumn /
//! sliceWithWidth, extractSegments, compositeTuiLine, and the Stack
//! sizing algorithm (allocateStackSizes / distribute) shared by VStack
//! and HStack.
//!
//! divergences: the full layout-node + TUI render loop machinery
//! (LAYOUT_NODE protocol, Container lifecycle, image lines) is not
//! ported; stacks are modeled over render closures like BoxComponent.
//! compositeTuiLine omits image-line passthrough (terminal-image is
//! host-side).

use crate::text_utils::{
    AnsiCodeTracker, extract_ansi_code, grapheme_clusters, grapheme_width,
    strip_terminal_sequences, visible_width,
};

const SEGMENT_RESET: &str = "\u{1b}[0m\u{1b}]8;;\u{7}";

// ============================================================================
// Column slicing (upstream sliceByColumn / sliceWithWidth)
// ============================================================================}

/// Extract a range of visible columns from a line (upstream
/// `sliceByColumn`). `strict` excludes wide chars at the boundary that
/// would extend past the range.
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).0
}

/// Like [`slice_by_column`] but also returns the visible width (upstream
/// `sliceWithWidth`).
pub fn slice_with_width(
    line: &str,
    start_col: usize,
    length: usize,
    strict: bool,
) -> (String, usize) {
    if length == 0 {
        return (String::new(), 0);
    }
    let end_col = start_col + length;
    let mut result = String::new();
    let mut result_width = 0usize;
    let mut current_col = 0usize;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(&ansi.code);
            } else if current_col < start_col {
                pending_ansi.push_str(&ansi.code);
            }
            i += ansi.length;
            continue;
        }
        let mut text_end = i;
        while text_end < chars.len() && extract_ansi_code(&chars, text_end).is_none() {
            text_end += 1;
        }
        for segment in grapheme_clusters(&chars[i..text_end].iter().collect::<String>()) {
            let w = grapheme_width(&segment);
            let in_range = current_col >= start_col && current_col < end_col;
            let fits = !strict || current_col + w <= end_col;
            if in_range && fits {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(&segment);
                result_width += w;
            }
            current_col += w;
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }
    (result, result_width)
}

// ============================================================================
// Segment extraction (upstream extractSegments)
// ============================================================================}

/// The before/after halves around an overlay region (upstream the
/// `extractSegments` result).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedSegments {
    pub before: String,
    pub before_width: usize,
    pub after: String,
    pub after_width: usize,
}

/// Extract "before" and "after" segments in a single pass (upstream
/// `extractSegments`): "after" inherits the styling state from before the
/// overlay.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> ExtractedSegments {
    let mut out = ExtractedSegments::default();
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi_before = String::new();
    let mut after_started = false;
    let after_end = after_start + after_len;
    let chars: Vec<char> = line.chars().collect();
    let mut style_tracker = AnsiCodeTracker::new();

    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            style_tracker.process(&ansi.code);
            if current_col < before_end {
                pending_ansi_before.push_str(&ansi.code);
            } else if current_col >= after_start && current_col < after_end && after_started {
                out.after.push_str(&ansi.code);
            }
            i += ansi.length;
            continue;
        }
        let mut text_end = i;
        while text_end < chars.len() && extract_ansi_code(&chars, text_end).is_none() {
            text_end += 1;
        }
        for segment in grapheme_clusters(&chars[i..text_end].iter().collect::<String>()) {
            let w = grapheme_width(&segment);
            if current_col < before_end && current_col + w <= before_end {
                if !pending_ansi_before.is_empty() {
                    out.before.push_str(&pending_ansi_before);
                    pending_ansi_before.clear();
                }
                out.before.push_str(&segment);
                out.before_width += w;
            } else if current_col >= after_start && current_col < after_end {
                let fits = !strict_after || current_col + w <= after_end;
                if fits {
                    if !after_started {
                        out.after.push_str(&style_tracker.get_active_codes());
                        after_started = true;
                    }
                    out.after.push_str(&segment);
                    out.after_width += w;
                }
            }
            current_col += w;
            let done = if after_len == 0 {
                current_col >= before_end
            } else {
                current_col >= after_end
            };
            if done {
                break;
            }
        }
        i = text_end;
        let done = if after_len == 0 {
            current_col >= before_end
        } else {
            current_col >= after_end
        };
        if done {
            break;
        }
    }
    out
}

/// Composite an overlay line onto a base line at a column (upstream
/// `compositeTuiLine`): preserves base styling before/after the overlay,
/// resets segments so styles don't bleed.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    let after_start = start_col + overlay_width;
    let base = extract_segments(
        base_line,
        start_col,
        after_start,
        total_width.saturating_sub(after_start),
        true,
    );
    let (overlay_text, overlay_text_width) = slice_with_width(overlay_line, 0, overlay_width, true);
    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay_text_width);
    let actual_before_width = start_col.max(base.before_width);
    let actual_overlay_width = overlay_width.max(overlay_text_width);
    let after_target = total_width.saturating_sub(actual_before_width + actual_overlay_width);
    let after_pad = after_target.saturating_sub(base.after_width);
    let result = format!(
        "{}{}{}{}{}{}{}{}",
        base.before,
        " ".repeat(before_pad),
        SEGMENT_RESET,
        overlay_text,
        " ".repeat(overlay_pad),
        SEGMENT_RESET,
        base.after,
        " ".repeat(after_pad)
    );
    if visible_width(&result) <= total_width {
        result
    } else {
        slice_by_column(&result, 0, total_width, true)
    }
}

// ============================================================================
// Stack sizing (upstream allocateStackSizes / distribute)
// ============================================================================}

/// Per-entry flex options (upstream `StackEntryOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackEntryOptions {
    /// Fixed size, or None for "auto" (intrinsic).
    pub basis: Option<usize>,
    pub grow: usize,
    /// None means the default shrink factor of 1.
    pub shrink: Option<usize>,
    pub min_size: usize,
    /// None means unbounded.
    pub max_size: Option<usize>,
}

impl StackEntryOptions {
    pub fn grow(value: usize) -> Self {
        Self {
            grow: value,
            ..Self::default()
        }
    }
}

fn clamp_size(size: usize, options: &StackEntryOptions) -> usize {
    let min = options.min_size;
    let max = options.max_size.unwrap_or(usize::MAX).max(min);
    size.clamp(min, max)
}

fn distribute(sizes: &mut [usize], entries: &[StackEntryOptions], mut amount: usize, mode: &str) {
    let max_for = |entry: &StackEntryOptions| entry.max_size.unwrap_or(usize::MAX);
    let min_for = |entry: &StackEntryOptions| entry.min_size;
    let shrink_for = |entry: &StackEntryOptions| entry.shrink.unwrap_or(1);

    while amount > 0 {
        let candidates: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                if mode == "grow" {
                    entry.grow > 0 && sizes[*index] < max_for(entry)
                } else {
                    shrink_for(entry) > 0 && sizes[*index] > min_for(entry)
                }
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let total_weight: usize = candidates
            .iter()
            .map(|index| {
                let entry = &entries[*index];
                if mode == "grow" {
                    entry.grow
                } else {
                    shrink_for(entry) * sizes[*index].max(1)
                }
            })
            .sum();
        if total_weight == 0 {
            return;
        }
        let mut distributed = 0usize;
        for index in &candidates {
            if amount == 0 {
                break;
            }
            let entry = &entries[*index];
            let weight = if mode == "grow" {
                entry.grow
            } else {
                shrink_for(entry) * sizes[*index].max(1)
            };
            let proposed = ((amount * weight) / total_weight).max(1);
            let capacity = if mode == "grow" {
                max_for(entry).saturating_sub(sizes[*index])
            } else {
                sizes[*index].saturating_sub(min_for(entry))
            };
            let delta = amount.min(proposed).min(capacity);
            if delta == 0 {
                continue;
            }
            if mode == "grow" {
                sizes[*index] += delta;
            } else {
                sizes[*index] -= delta;
            }
            amount -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            return;
        }
    }
}

/// Resolve flex sizes (upstream `allocateStackSizes`): basis or intrinsic
/// size clamped, then grow/shrink distribute the remaining (or excess)
/// space after gaps. `available_size` of None skips distribution.
pub fn allocate_stack_sizes(
    entries: &[StackEntryOptions],
    intrinsic_sizes: &[usize],
    available_size: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let base = match entry.basis {
                Some(basis) => basis,
                None => intrinsic_sizes.get(index).copied().unwrap_or(0),
            };
            clamp_size(base, entry)
        })
        .collect();
    let Some(available_size) = available_size else {
        return sizes;
    };
    let content_size = available_size.saturating_sub(entries.len().saturating_sub(1) * gap);
    let total: usize = sizes.iter().sum();
    if total < content_size {
        distribute(&mut sizes.clone(), entries, content_size - total, "grow");
        // distribute mutates in place; do it on the real buffer instead.
        let mut sizes = sizes;
        let deficit = content_size - total;
        distribute(&mut sizes, entries, deficit, "grow");
        sizes
    } else if total > content_size {
        let mut sizes = sizes;
        distribute(&mut sizes, entries, total - content_size, "shrink");
        sizes
    } else {
        sizes
    }
}

// ============================================================================
// VStack / HStack over render closures
// ============================================================================}

/// A stack child: a render closure plus its flex options (upstream
/// `StackEntry`).
pub struct StackChild<'a> {
    pub render: Box<dyn Fn(usize) -> Vec<String> + Send + 'a>,
    pub options: StackEntryOptions,
}

/// Vertical stack (upstream `VStack.render`): children stack top to
/// bottom with gap lines, each sized by the flex algorithm, padded to
/// their allocated size.
pub fn vstack_render(children: &mut [StackChild<'_>], gap: usize, width: usize) -> Vec<String> {
    let safe_width = width.max(1);
    let rendered: Vec<Vec<String>> = children
        .iter_mut()
        .map(|child| (child.render)(safe_width))
        .collect();
    let options: Vec<StackEntryOptions> = children.iter().map(|c| c.options.clone()).collect();
    let sizes = allocate_stack_sizes(
        &options,
        &rendered.iter().map(Vec::len).collect::<Vec<_>>(),
        None,
        gap,
    );
    let mut lines: Vec<String> = Vec::new();
    for (index, child_lines) in rendered.iter().enumerate() {
        if index > 0 {
            for _ in 0..gap {
                lines.push(String::new());
            }
        }
        let sized = &child_lines[..child_lines.len().min(sizes[index])];
        lines.extend(sized.iter().cloned());
        for _ in sized.len()..sizes[index] {
            lines.push(String::new());
        }
    }
    lines
}

/// Horizontal stack (upstream `HStack.render`): children composite left
/// to right with gaps, vertically aligned per `align`.
pub fn hstack_render(
    children: &mut [StackChild<'_>],
    gap: usize,
    align: &str,
    width: usize,
) -> Vec<String> {
    let safe_width = width.max(1);
    if children.is_empty() {
        return Vec::new();
    }
    let intrinsic_widths: Vec<usize> = children
        .iter()
        .map(|child| {
            (child.render)(safe_width)
                .iter()
                .map(|line| visible_width(&strip_terminal_sequences(line)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let options: Vec<StackEntryOptions> = children.iter().map(|c| c.options.clone()).collect();
    let widths = allocate_stack_sizes(&options, &intrinsic_widths, Some(safe_width), gap);
    let rendered: Vec<Vec<String>> = children
        .iter_mut()
        .enumerate()
        .map(|(index, child)| {
            if widths[index] == 0 {
                Vec::new()
            } else {
                (child.render)(widths[index])
            }
        })
        .collect();
    let height = rendered.iter().map(Vec::len).max().unwrap_or(0);
    let mut result = vec![String::new(); height];
    let mut x = 0usize;
    for (index, lines) in rendered.iter().enumerate() {
        let child_width = widths[index];
        let offset = match align {
            "center" => (height - lines.len()) / 2,
            "end" => height - lines.len(),
            _ => 0,
        };
        for (row, line) in lines.iter().enumerate() {
            let target = row + offset;
            if target >= result.len() {
                continue;
            }
            result[target] = composite_tui_line(&result[target], line, x, child_width, safe_width);
        }
        x += child_width + gap;
    }
    result
}
