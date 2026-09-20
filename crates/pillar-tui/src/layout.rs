//! Port of packages/tui/src/layout.ts (pi v0.84.3): the layout engine
//! that positions a component tree into a fixed-size screen of lines —
//! leaf boxes, scroll views, and vstack/hstack flex containers, with
//! clipping, cursor-marker line offset, scrollbar geometry, and paint.
//!
//! divergences: the upstream Component trait + LAYOUT_NODE symbol
//! protocol is replaced by a [`LayoutNode`] enum over render closures
//! (matching the closure-based component convention used throughout
//! the port); Kitty image cropping in paint and the OSC 133 zone
//! prefix stay host-side; the render cache is keyed by node path (the
//! upstream keys by component identity).

use std::collections::HashMap;

use crate::input::CURSOR_MARKER;
use crate::loaders::ScrollView;
use crate::stack_layout::slice_by_column;
use crate::stack_layout::{allocate_stack_sizes, composite_tui_line};
use crate::terminal_image::is_image_line;
use crate::text_utils::{extract_ansi_code, visible_width};

/// A positioned rectangle (upstream `LayoutRect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

fn intersect(a: LayoutRect, b: LayoutRect) -> LayoutRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    LayoutRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn contains_point(rect: LayoutRect, x: usize, y: usize) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// What a layout node renders as (upstream LAYOUT_NODE variants).
pub enum LayoutNode<'a> {
    /// A leaf: renders lines at a width.
    Leaf(Box<dyn Fn(usize) -> Vec<String> + 'a>),
    /// A scroll container with its scroll state and child node. The
    /// state is shared (upstream mutates through the component tree).
    Scroll {
        child: Box<LayoutNode<'a>>,
        state: &'a std::cell::RefCell<ScrollView>,
    },
    /// A flex stack.
    Stack {
        vertical: bool,
        entries: Vec<StackEntry<'a>>,
        gap: usize,
        align: Align,
    },
}

/// Stack entry options (upstream `StackLayoutEntry`).
pub struct StackEntry<'a> {
    pub node: Box<LayoutNode<'a>>,
    pub basis: Option<usize>,
    pub grow: usize,
    pub shrink: Option<usize>,
    pub min_size: usize,
    pub max_size: Option<usize>,
}

/// Stack alignment (upstream `"stretch" | "start" | "center" | "end"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Stretch,
    Start,
    Center,
    End,
}

/// Stack entry options mapped to the flex algorithm's expected struct
/// (reuses `StackEntryOptions` from stack_layout).
fn flex_options(entry: &StackEntry<'_>) -> crate::stack_layout::StackEntryOptions {
    crate::stack_layout::StackEntryOptions {
        basis: entry.basis,
        grow: entry.grow,
        shrink: entry.shrink,
        min_size: entry.min_size,
        max_size: entry.max_size,
    }
}

/// A laid-out box (upstream `LayoutBox`).
pub struct LayoutBox<'a> {
    pub rect: LayoutRect,
    pub clip: LayoutRect,
    pub children: Vec<LayoutBox<'a>>,
    /// Leaf render output.
    pub lines: Option<Vec<String>>,
    /// Cursor-driven skip offset within `lines` (upstream `lineOffset`).
    pub line_offset: usize,
    /// Scroll state when this box is a scroll container.
    pub scroll_view: Option<&'a std::cell::RefCell<ScrollView>>,
    pub scroll_content_lines: Option<Vec<String>>,
}

/// The finished frame (upstream `LayoutFrame`).
pub struct LayoutFrame<'a> {
    pub root: LayoutBox<'a>,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
}

struct LayoutContext<'a> {
    render_cache: HashMap<(usize, usize), Vec<String>>,
    _marker: std::marker::PhantomData<&'a ()>,
}

fn render_cached(
    context: &mut LayoutContext<'_>,
    key: usize,
    width: usize,
    render: &dyn Fn(usize) -> Vec<String>,
) -> Vec<String> {
    let cache_key = (key, width.max(1));
    if let Some(lines) = context.render_cache.get(&cache_key) {
        return lines.clone();
    }
    let lines = render(width.max(1));
    context.render_cache.insert(cache_key, lines.clone());
    lines
}

/// Lay out `node` into a box at (x, y) with width and optional height,
/// clipped to `clip` (upstream `layoutComponent`).
#[allow(clippy::too_many_arguments)]
fn layout_component<'a>(
    context: &mut LayoutContext<'_>,
    node: &mut LayoutNode<'a>,
    key: usize,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
) -> LayoutBox<'a> {
    let safe_width = width.max(1);
    match node {
        LayoutNode::Leaf(render) => {
            let lines = render_cached(context, key, safe_width, render.as_ref());
            let allocated_height = match height {
                Some(h) => h,
                None => lines.len(),
            };
            let mut line_offset = 0usize;
            if lines.len() > allocated_height && allocated_height > 0 {
                let cursor_line = lines
                    .iter()
                    .position(|line| line.contains(CURSOR_MARKER))
                    .unwrap_or(0);
                if cursor_line >= allocated_height {
                    line_offset = cursor_line - allocated_height + 1;
                }
            }
            let rect = LayoutRect {
                x,
                y,
                width: safe_width,
                height: allocated_height,
            };
            LayoutBox {
                rect,
                clip: intersect(clip, rect),
                children: Vec::new(),
                lines: Some(lines),
                line_offset,
                scroll_view: None,
                scroll_content_lines: None,
            }
        }
        LayoutNode::Scroll { child, state } => {
            let previous_scroll_top = state.borrow().scroll_top();
            let content_width = state.borrow().get_content_width(safe_width);
            let mut child_box = layout_component(
                context,
                child,
                key + 1,
                x,
                y.saturating_sub(previous_scroll_top),
                content_width,
                None,
                clip,
            );
            let content_height = child_box.rect.height;
            let viewport_height = height.unwrap_or(content_height);
            state
                .borrow_mut()
                .update_layout(content_height, viewport_height);
            // Re-translate the child to the (possibly changed) scroll top.
            // The child was laid out at y - previousScrollTop; upstream
            // translateBox(previousScrollTop - scrollTop) shifts it to
            // y - scrollTop. Downward scroll (larger new top) moves rows
            // up, so the delta is signed.
            let new_scroll_top = state.borrow().scroll_top();
            if previous_scroll_top >= new_scroll_top {
                translate_box(&mut child_box, previous_scroll_top - new_scroll_top);
            } else {
                shift_box_up(&mut child_box, new_scroll_top - previous_scroll_top);
            }
            let rect = LayoutRect {
                x,
                y,
                width: safe_width,
                height: viewport_height,
            };
            let child_clip = intersect(clip, rect);
            child_box.clip = intersect(child_box.clip, child_clip);
            update_clips(&mut child_box, child_clip);
            let scroll_content_lines = {
                let child_node: &mut LayoutNode<'a> = child;
                match &mut *child_node {
                    LayoutNode::Leaf(render) => {
                        render_cached(context, key + 1, content_width, render.as_mut())
                    }
                    _ => Vec::new(),
                }
            };
            LayoutBox {
                rect,
                clip: child_clip,
                children: vec![child_box],
                lines: None,
                line_offset: 0,
                scroll_view: Some(state),
                scroll_content_lines: Some(scroll_content_lines),
            }
        }
        LayoutNode::Stack {
            vertical,
            entries,
            gap,
            align,
        } => {
            let gap = *gap;
            let align = *align;
            let flex: Vec<crate::stack_layout::StackEntryOptions> =
                entries.iter().map(flex_options).collect();
            if *vertical {
                let intrinsic_heights: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| match entry.basis {
                        Some(basis) => basis,
                        None => {
                            let child_key = key * 100 + index + 1;
                            match &*entry.node {
                                LayoutNode::Leaf(render) => {
                                    render_cached(context, child_key, safe_width, render.as_ref())
                                        .len()
                                }
                                _ => 0,
                            }
                        }
                    })
                    .collect();
                let sizes = allocate_stack_sizes(&flex, &intrinsic_heights, height, gap);
                let natural_height: usize =
                    sizes.iter().sum::<usize>() + entries.len().saturating_sub(1) * gap;
                let allocated_height = height.unwrap_or(natural_height);
                let rect = LayoutRect {
                    x,
                    y,
                    width: safe_width,
                    height: allocated_height,
                };
                let box_clip = intersect(clip, rect);
                let mut children = Vec::new();
                let mut child_y = y;
                for (index, entry) in entries.iter_mut().enumerate() {
                    children.push(layout_component(
                        context,
                        &mut entry.node,
                        key * 100 + index + 1,
                        x,
                        child_y,
                        safe_width,
                        Some(sizes[index]),
                        box_clip,
                    ));
                    child_y += sizes[index] + gap;
                }
                LayoutBox {
                    rect,
                    clip: box_clip,
                    children,
                    lines: None,
                    line_offset: 0,
                    scroll_view: None,
                    scroll_content_lines: None,
                }
            } else {
                let intrinsic_widths: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| match entry.basis {
                        Some(basis) => basis,
                        None => {
                            let child_key = key * 100 + index + 1;
                            match &*entry.node {
                                LayoutNode::Leaf(render) => {
                                    render_cached(context, child_key, safe_width, render.as_ref())
                                        .iter()
                                        .map(|line| visible_width(line))
                                        .max()
                                        .unwrap_or(0)
                                }
                                _ => 0,
                            }
                        }
                    })
                    .collect();
                let widths = allocate_stack_sizes(&flex, &intrinsic_widths, Some(safe_width), gap);
                let intrinsic_heights: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let child_key = key * 100 + index + 1;
                        match &*entry.node {
                            LayoutNode::Leaf(render) => render_cached(
                                context,
                                child_key,
                                widths[index].max(1),
                                render.as_ref(),
                            )
                            .len(),
                            _ => 0,
                        }
                    })
                    .collect();
                let allocated_height =
                    height.unwrap_or_else(|| intrinsic_heights.iter().copied().max().unwrap_or(0));
                let rect = LayoutRect {
                    x,
                    y,
                    width: safe_width,
                    height: allocated_height,
                };
                let box_clip = intersect(clip, rect);
                let mut children = Vec::new();
                let mut child_x = x;
                for (index, entry) in entries.iter_mut().enumerate() {
                    let natural_child_height = intrinsic_heights[index];
                    let child_height = if align == Align::Stretch {
                        allocated_height
                    } else {
                        allocated_height.min(natural_child_height)
                    };
                    let mut child_y = y;
                    if align == Align::Center {
                        child_y += (allocated_height - child_height) / 2;
                    } else if align == Align::End {
                        child_y += allocated_height - child_height;
                    }
                    let child_width = widths[index];
                    if child_width == 0 {
                        children.push(LayoutBox {
                            rect: LayoutRect {
                                x: child_x,
                                y: child_y,
                                width: 0,
                                height: child_height,
                            },
                            clip: LayoutRect {
                                x: child_x,
                                y: child_y,
                                width: 0,
                                height: 0,
                            },
                            children: Vec::new(),
                            lines: None,
                            line_offset: 0,
                            scroll_view: None,
                            scroll_content_lines: None,
                        });
                    } else {
                        children.push(layout_component(
                            context,
                            &mut entry.node,
                            key * 100 + index + 1,
                            child_x,
                            child_y,
                            child_width,
                            Some(child_height),
                            box_clip,
                        ));
                    }
                    child_x += child_width + gap;
                }
                LayoutBox {
                    rect,
                    clip: box_clip,
                    children,
                    lines: None,
                    line_offset: 0,
                    scroll_view: None,
                    scroll_content_lines: None,
                }
            }
        }
    }
}

fn translate_box(box_: &mut LayoutBox<'_>, delta_y: usize) {
    box_.rect.y += delta_y;
    for child in &mut box_.children {
        translate_box(child, delta_y);
    }
}

fn shift_box_up(box_: &mut LayoutBox<'_>, delta_y: usize) {
    box_.rect.y = box_.rect.y.saturating_sub(delta_y);
    for child in &mut box_.children {
        shift_box_up(child, delta_y);
    }
}

fn update_clips(box_: &mut LayoutBox<'_>, parent_clip: LayoutRect) {
    box_.clip = intersect(parent_clip, box_.rect);
    for child in &mut box_.children {
        update_clips(child, box_.clip);
    }
}

/// Scrollbar geometry (upstream `ScrollbarGeometry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: usize,
    pub track_top: usize,
    pub track_height: usize,
    pub thumb_top: usize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

/// Compute scrollbar geometry for a scroll box (upstream
/// `getScrollbarGeometry`).
pub fn get_scrollbar_geometry(box_: &LayoutBox<'_>) -> Option<ScrollbarGeometry> {
    let scroll_view = box_.scroll_view?;
    let scroll_view = scroll_view.borrow();
    if !scroll_view.is_scrollbar_visible() || box_.rect.width == 0 || box_.rect.height == 0 {
        return None;
    }
    let content_height = box_
        .children
        .first()
        .map(|child| child.rect.height)
        .or_else(|| box_.scroll_content_lines.as_ref().map(Vec::len))
        .unwrap_or(0);
    let track_height = box_.rect.height;
    let min_thumb_height = track_height.min(2);
    let thumb_height = min_thumb_height
        .max((track_height * track_height) / content_height.max(1))
        .min(track_height);
    let max_scroll_top = content_height.saturating_sub(track_height);
    let max_thumb_top = track_height - thumb_height;
    let thumb_offset = (scroll_view.scroll_top() * max_thumb_top + max_scroll_top / 2)
        .checked_div(max_scroll_top)
        .unwrap_or(0);
    let column = box_.rect.x + box_.rect.width - 1;
    if column < box_.clip.x || column >= box_.clip.x + box_.clip.width {
        return None;
    }
    Some(ScrollbarGeometry {
        column,
        track_top: box_.rect.y,
        track_height,
        thumb_top: box_.rect.y + thumb_offset,
        thumb_height,
        max_scroll_top,
    })
}

fn style_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    style: &dyn Fn(&str) -> String,
) -> String {
    if is_image_line(line) {
        return line.to_string();
    }
    // Grapheme-cell range for the target column: approximate with 1 cell.
    let start = column;
    let end = column + 1;
    let before = slice_by_column(line, 0, start, true);
    let target = slice_by_column(line, start, end - start, true);
    let after = slice_by_column(line, end, total_width.saturating_sub(end), true);

    let chars: Vec<char> = target.chars().collect();
    let mut target_prefix = String::new();
    let mut target_index = 0usize;
    while let Some(ansi) = extract_ansi_code(&chars, target_index) {
        target_prefix.push_str(&ansi.code);
        target_index += ansi.length;
    }
    let target_text = if target_index < chars.len() {
        chars[target_index..].iter().collect()
    } else {
        " ".repeat(end - start)
    };
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    format!(
        "{before}{before_padding}{target_prefix}{}{after}",
        style(&target_text)
    )
}

fn paint_scrollbar(box_: &LayoutBox<'_>, screen: &mut [String], total_width: usize) {
    let Some(_geometry) = get_scrollbar_geometry(box_) else {
        return;
    };
    let geometry = _geometry;
    let default_style = |text: &str| format!("\u{1b}[100m{text}\u{1b}[49m");
    for offset in 0..geometry.thumb_height {
        let row = geometry.thumb_top + offset;
        if row < box_.clip.y || row >= box_.clip.y + box_.clip.height || row >= screen.len() {
            continue;
        }
        let line = screen[row].clone();
        let styled = style_scrollbar_cell(&line, geometry.column, total_width, &default_style);
        screen[row] = styled;
    }
}

fn paint_box(box_: &LayoutBox<'_>, screen: &mut Vec<String>, total_width: usize) {
    if let Some(lines) = &box_.lines {
        let offset = box_.line_offset;
        let first_row = box_.rect.y.max(box_.clip.y);
        let last_row = (box_.rect.y + box_.rect.height)
            .min(box_.clip.y + box_.clip.height)
            .min(screen.len());
        for (row, source_index) in
            (first_row..last_row).map(|row| (row, offset + row - box_.rect.y))
        {
            let Some(source_line) = lines.get(source_index) else {
                continue;
            };
            let line = source_line.as_str();
            // Fast path: full-width box onto an untouched row.
            if box_.rect.x == 0 && box_.rect.width >= total_width && screen[row].is_empty() {
                screen[row] = line.to_string();
            } else {
                screen[row] = composite_tui_line(
                    &screen[row],
                    line,
                    box_.rect.x,
                    box_.rect.width,
                    total_width,
                );
            }
        }
    }
    for child in &box_.children {
        paint_box(child, screen, total_width);
    }
    paint_scrollbar(box_, screen, total_width);
}

/// Lay out and paint the tree into a `height`-row screen (upstream
/// `renderLayoutFrame`).
pub fn render_layout_frame<'a>(
    node: &mut LayoutNode<'a>,
    width: usize,
    height: usize,
) -> LayoutFrame<'a> {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let mut context = LayoutContext {
        render_cache: HashMap::new(),
        _marker: std::marker::PhantomData,
    };
    let clip = LayoutRect {
        x: 0,
        y: 0,
        width: safe_width,
        height: safe_height,
    };
    let root_box = layout_component(
        &mut context,
        node,
        1,
        0,
        0,
        safe_width,
        Some(safe_height),
        clip,
    );
    let mut lines = vec![String::new(); safe_height];
    paint_box(&root_box, &mut lines, safe_width);
    LayoutFrame {
        root: root_box,
        width: safe_width,
        height: safe_height,
        lines,
    }
}

/// Scroll views whose clip contains a point, outermost last (upstream
/// `getScrollViewsAt`).
pub fn get_scroll_views_at<'a>(
    frame: &'a LayoutFrame<'a>,
    x: usize,
    y: usize,
) -> Vec<&'a std::cell::RefCell<ScrollView>> {
    fn visit<'a>(
        box_: &'a LayoutBox<'a>,
        x: usize,
        y: usize,
        out: &mut Vec<&'a std::cell::RefCell<ScrollView>>,
    ) {
        if !contains_point(box_.clip, x, y) {
            return;
        }
        if let Some(scroll_view) = box_.scroll_view
            && contains_point(box_.rect, x, y)
        {
            out.push(scroll_view);
        }
        for child in &box_.children {
            visit(child, x, y, out);
        }
    }
    let mut result = Vec::new();
    visit(&frame.root, x, y, &mut result);
    result.reverse();
    result
}
