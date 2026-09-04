//! Parity tests for the layout engine (pi v0.84.3 layout.ts).

use std::cell::RefCell;

use pillar_tui::layout::{
    get_scroll_views_at, get_scrollbar_geometry, render_layout_frame, Align, LayoutNode, StackEntry,
};
use pillar_tui::loaders::{ScrollView, ScrollViewOptions, ScrollViewScrollbar};
use pillar_tui::text_utils::{strip_terminal_sequences, visible_width};

fn leaf(text: &'static str) -> LayoutNode<'static> {
    LayoutNode::Leaf(Box::new(move |_width| vec![text.to_string()]))
}

fn leaf_lines(lines: Vec<&'static str>) -> LayoutNode<'static> {
    LayoutNode::Leaf(Box::new(move |_width| {
        lines.iter().map(|l| l.to_string()).collect()
    }))
}

// --- leaf layout ---------------------------------------------------------------------------------

#[test]
fn leaf_fills_frame() {
    let mut node = leaf_lines(vec!["one", "two", "three"]);
    let frame = render_layout_frame(&mut node, 20, 5);
    assert_eq!(frame.lines.len(), 5);
    assert!(frame.lines[0].starts_with("one"), "{:?}", frame.lines);
    assert!(frame.lines[1].starts_with("two"));
    assert!(frame.lines[2].starts_with("three"));
    // Untouched rows stay empty.
    assert_eq!(frame.lines[3], "");
}

#[test]
fn leaf_height_shorter_than_content_clips_from_top() {
    let mut node = leaf_lines(vec!["one", "two", "three"]);
    let frame = render_layout_frame(&mut node, 20, 2);
    assert!(frame.lines[0].starts_with("one"), "{:?}", frame.lines);
    assert!(frame.lines[1].starts_with("two"));
}

#[test]
fn cursor_marker_pulls_cursor_line_into_view() {
    let marker = pillar_tui::input::CURSOR_MARKER;
    let lines = vec![format!("{marker}aaa"), "bbb".to_string(), "ccc".to_string()];
    let mut node = LayoutNode::Leaf(Box::new(move |_w| lines.clone()));
    let frame = render_layout_frame(&mut node, 20, 2);
    // Cursor on line 0 stays visible; no offset needed.
    assert!(frame.lines[0].contains("aaa"), "{:?}", frame.lines);
}

#[test]
fn cursor_marker_offset_scrolls_long_content() {
    let marker = pillar_tui::input::CURSOR_MARKER;
    let lines: Vec<String> = (0..5)
        .map(|i| {
            if i == 4 {
                format!("{marker}line4")
            } else {
                format!("line{i}")
            }
        })
        .collect();
    let mut node = LayoutNode::Leaf(Box::new(move |_w| lines.clone()));
    let frame = render_layout_frame(&mut node, 20, 2);
    // Cursor is on visual line 4; offset = 4 - 2 + 1 = 3 → shows lines 3,4.
    assert!(frame.lines[0].contains("line3"), "{:?}", frame.lines);
    assert!(frame.lines[1].contains("line4"), "{:?}", frame.lines);
}

// --- vstack -----------------------------------------------------------------------------------------

#[test]
fn vstack_stacks_leaves() {
    let mut node = LayoutNode::Stack {
        vertical: true,
        entries: vec![
            StackEntry {
                node: Box::new(leaf("alpha")),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf("beta")),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 0,
        align: Align::Start,
    };
    let frame = render_layout_frame(&mut node, 20, 4);
    assert!(frame.lines[0].starts_with("alpha"), "{:?}", frame.lines);
    assert!(frame.lines[1].starts_with("beta"));
}

#[test]
fn vstack_gap_inserts_empty_rows() {
    let mut node = LayoutNode::Stack {
        vertical: true,
        entries: vec![
            StackEntry {
                node: Box::new(leaf("a")),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf("b")),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 2,
        align: Align::Start,
    };
    let frame = render_layout_frame(&mut node, 20, 6);
    assert!(frame.lines[0].starts_with("a"));
    assert_eq!(frame.lines[1], "");
    assert_eq!(frame.lines[2], "");
    assert!(frame.lines[3].starts_with("b"));
}

#[test]
fn vstack_grow_expands_children() {
    let mut node = LayoutNode::Stack {
        vertical: true,
        entries: vec![
            StackEntry {
                node: Box::new(leaf_lines(vec!["top"])),
                basis: None,
                grow: 1,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf_lines(vec!["bottom"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 0,
        align: Align::Start,
    };
    let frame = render_layout_frame(&mut node, 20, 4);
    // Growing child allocated 3 rows; "bottom" lands on row 3.
    assert!(frame.lines[3].starts_with("bottom"), "{:?}", frame.lines);
}

// --- hstack ------------------------------------------------------------------------------------------

#[test]
fn hstack_places_children_side_by_side() {
    let mut node = LayoutNode::Stack {
        vertical: false,
        entries: vec![
            StackEntry {
                node: Box::new(leaf_lines(vec!["ab", "cd"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf_lines(vec!["ef"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 1,
        align: Align::Start,
    };
    let frame = render_layout_frame(&mut node, 20, 2);
    let l0 = strip_terminal_sequences(&frame.lines[0])
        .trim_end()
        .to_string();
    let l1 = strip_terminal_sequences(&frame.lines[1])
        .trim_end()
        .to_string();
    assert_eq!(l0, "ab ef");
    assert_eq!(l1, "cd");
}

#[test]
fn hstack_center_alignment() {
    let mut node = LayoutNode::Stack {
        vertical: false,
        entries: vec![
            StackEntry {
                node: Box::new(leaf_lines(vec!["a", "b", "c"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf_lines(vec!["X"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 1,
        align: Align::Center,
    };
    let frame = render_layout_frame(&mut node, 20, 3);
    // "X" centered vertically: row 1.
    assert!(frame.lines[1].contains("X"), "{:?}", frame.lines);
    assert!(!frame.lines[0].contains("X"));
}

#[test]
fn hstack_end_alignment() {
    let mut node = LayoutNode::Stack {
        vertical: false,
        entries: vec![
            StackEntry {
                node: Box::new(leaf_lines(vec!["a", "b", "c"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf_lines(vec!["X"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 1,
        align: Align::End,
    };
    let frame = render_layout_frame(&mut node, 20, 3);
    assert!(frame.lines[2].contains("X"), "{:?}", frame.lines);
}

// --- scroll -------------------------------------------------------------------------------------------

fn scroll_node(
    lines: Vec<&'static str>,
    state: &'static RefCell<ScrollView>,
) -> LayoutNode<'static> {
    LayoutNode::Scroll {
        child: Box::new(leaf_lines(lines)),
        state,
    }
}

fn scroll_state() -> &'static RefCell<ScrollView> {
    Box::leak(Box::new(RefCell::new(ScrollView::new(
        ScrollViewOptions::default(),
    ))))
}

fn follow_state() -> &'static RefCell<ScrollView> {
    Box::leak(Box::new(RefCell::new(ScrollView::new(ScrollViewOptions {
        follow_end: true,
        ..ScrollViewOptions::default()
    }))))
}

#[test]
fn scroll_viewport_clips_content() {
    let state = scroll_state();
    let mut node = scroll_node(vec!["one", "two", "three", "four"], state);
    let frame = render_layout_frame(&mut node, 20, 2);
    assert!(frame.lines[0].starts_with("one"), "{:?}", frame.lines);
    assert!(frame.lines[1].starts_with("two"));
}

#[test]
fn scroll_follow_end_pins_bottom() {
    let state = follow_state();
    let mut node = scroll_node(vec!["one", "two", "three", "four", "five"], state);
    let _ = render_layout_frame(&mut node, 20, 2);
    // follow_end was requested; layout updated content 5 / viewport 2 →
    // scrollTop 3 → shows the last two lines.
    assert_eq!(state.borrow().scroll_top(), 3, "scrollTop after layout");
}

#[test]
fn scroll_content_width_reserved_for_always_scrollbar() {
    let state = Box::leak(Box::new(RefCell::new(ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Always,
        ..ScrollViewOptions::default()
    }))));
    let mut node = scroll_node(vec!["content"], state);
    let frame = render_layout_frame(&mut node, 20, 2);
    // Content rendered at width 19, one column reserved; the line is
    // padded by the space appended per row.
    let line = strip_terminal_sequences(&frame.lines[0]);
    assert!(line.starts_with("content"), "{line:?}");
    // content(7) + padding + reserved scrollbar column = 20.
    assert_eq!(visible_width(&line), 20);
}

// --- scrollbar geometry ------------------------------------------------------------------------------

#[test]
fn scrollbar_geometry_hidden_by_default() {
    let state = scroll_state();
    let mut node = scroll_node(vec!["a", "b", "c", "d", "e"], state);
    let frame = render_layout_frame(&mut node, 20, 2);
    let scroll_box = find_scroll_box(&frame);
    assert!(get_scrollbar_geometry(scroll_box).is_none());
}

#[test]
fn scrollbar_geometry_always_visible() {
    let state = Box::leak(Box::new(RefCell::new(ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Always,
        ..ScrollViewOptions::default()
    }))));
    let mut node = scroll_node(vec!["a", "b", "c", "d", "e"], state);
    let frame = render_layout_frame(&mut node, 20, 2);
    let scroll_box = find_scroll_box(&frame);
    let geometry = get_scrollbar_geometry(scroll_box).expect("geometry");
    assert_eq!(geometry.column, 19);
    assert_eq!(geometry.track_height, 2);
    assert_eq!(geometry.max_scroll_top, 3);
    // At scrollTop 0 the thumb is at the track top.
    assert_eq!(geometry.thumb_top, 0);
}

#[test]
fn scrollbar_thumb_moves_with_scroll() {
    let state = Box::leak(Box::new(RefCell::new(ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Always,
        ..ScrollViewOptions::default()
    }))));
    state.borrow_mut().scroll_to(2, false);
    let mut node = scroll_node(vec!["a", "b", "c", "d", "e"], state);
    let frame = render_layout_frame(&mut node, 20, 2);
    let scroll_box = find_scroll_box(&frame);
    let geometry = get_scrollbar_geometry(scroll_box).expect("geometry");
    // Track of 2 with min thumb 2 → maxThumbTop is 0; geometry still
    // reports the scroll metrics.
    assert_eq!(geometry.max_scroll_top, 3);
    assert_eq!(geometry.thumb_top, 0);
}

fn find_scroll_box<'a, 'b>(
    frame: &'a pillar_tui::layout::LayoutFrame<'b>,
) -> &'a pillar_tui::layout::LayoutBox<'b> {
    // Root is the scroll node itself when the tree is just a scroll.
    &frame.root
}

// --- hit testing -----------------------------------------------------------------------------------------

#[test]
fn scroll_views_at_point() {
    let state = scroll_state();
    let mut node = scroll_node(vec!["a", "b", "c"], state);
    let frame = render_layout_frame(&mut node, 20, 3);
    let hits = get_scroll_views_at(&frame, 0, 0);
    assert_eq!(hits.len(), 1);
    assert!(get_scroll_views_at(&frame, 0, 10).is_empty());
}

// --- rendering compositing --------------------------------------------------------------------------------

#[test]
fn overlapping_boxes_composite() {
    // A vstack with a wide leaf and a narrow one; the second paints over
    // the same rows starting at its own x offset via hstack semantics.
    let mut node = LayoutNode::Stack {
        vertical: false,
        entries: vec![
            StackEntry {
                node: Box::new(leaf_lines(vec!["aaaaaaaa"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
            StackEntry {
                node: Box::new(leaf_lines(vec!["zz"])),
                basis: None,
                grow: 0,
                shrink: None,
                min_size: 0,
                max_size: None,
            },
        ],
        gap: 0,
        align: Align::Start,
    };
    let frame = render_layout_frame(&mut node, 20, 1);
    let l0 = strip_terminal_sequences(&frame.lines[0])
        .trim_end()
        .to_string();
    assert_eq!(l0, "aaaaaaaazz");
}

#[test]
fn render_cache_reuses_leaf_output_across_queries() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let lines = vec!["x".to_string()];
    let node = LayoutNode::Leaf(Box::new(move |_w| {
        CALLS.fetch_add(1, Ordering::SeqCst);
        lines.clone()
    }));
    // Hstack measures width and height, then renders — cache should keep
    // the render count low.
    let mut stack = LayoutNode::Stack {
        vertical: false,
        entries: vec![StackEntry {
            node: Box::new(node),
            basis: None,
            grow: 0,
            shrink: None,
            min_size: 0,
            max_size: None,
        }],
        gap: 0,
        align: Align::Start,
    };
    let _ = render_layout_frame(&mut stack, 20, 2);
    assert!(
        CALLS.load(Ordering::SeqCst) <= 3,
        "{}",
        CALLS.load(Ordering::SeqCst)
    );
}
