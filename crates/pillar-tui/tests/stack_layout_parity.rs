//! Parity tests for tui layout helpers (pi v0.84.3): sliceByColumn,
//! extractSegments, compositeTuiLine, and the Stack flex sizing
//! algorithm (VStack/HStack).

use pillar_tui::text_utils::strip_terminal_sequences;

use pillar_tui::stack_layout::{
    StackChild, StackEntryOptions, allocate_stack_sizes, composite_tui_line, extract_segments,
    hstack_render, slice_by_column, slice_with_width, vstack_render,
};

// --- sliceByColumn -------------------------------------------------------------------------

#[test]
fn slice_by_column_basic_and_wide_chars() {
    assert_eq!(slice_by_column("hello", 1, 3, false), "ell");
    // Wide chars: width 5 total; slicing cols 0..2 with strict excludes
    // the wide char that would extend past.
    assert_eq!(slice_by_column("日本", 0, 2, true), "日");
    // Non-strict with length 2 still stops at the boundary (col 2 == end).
    assert_eq!(slice_by_column("日本", 0, 2, false), "日");
    // Length 3 includes both (non-strict lets 本 overflow the boundary).
    assert_eq!(slice_by_column("日本", 0, 3, true), "日");
    assert_eq!(slice_by_column("日本", 0, 3, false), "日本");
    // ANSI codes don't consume columns.
    assert_eq!(
        slice_by_column("\u{1b}[31mab\u{1b}[0m", 0, 1, false),
        "\u{1b}[31ma"
    );
}

#[test]
fn slice_with_width_reports_actual_width() {
    let (text, width) = slice_with_width("日本", 0, 2, true);
    assert_eq!((text.as_str(), width), ("日", 2));
}

// --- extractSegments -------------------------------------------------------------------------

#[test]
fn extract_segments_before_and_after() {
    let segments = extract_segments("hello world", 5, 8, 3, false);
    assert_eq!(segments.before, "hello");
    assert_eq!(segments.before_width, 5);
    assert_eq!(
        segments.after,
        "world".to_string().get(3..6).unwrap_or("rld")
    );
    assert_eq!(segments.after_width, 3);
}

#[test]
fn extract_segments_after_inherits_style() {
    // No reset before the boundary: "after" inherits the active color.
    let segments = extract_segments("\u{1b}[31mred green", 3, 4, 5, false);
    // "after" inherits the active SGR state at the overlay boundary.
    assert!(segments.after.contains("\u{1b}[31m"), "{segments:?}");
    assert!(segments.after.contains("green"), "{segments:?}");
    assert_eq!(segments.after_width, 5);
}

// --- compositeTuiLine ---------------------------------------------------------------------------

#[test]
fn composite_line_overlays_at_column() {
    let result = composite_tui_line("hello world", "XYZ", 6, 3, 11);
    // Segment resets wrap the overlay; visible content is "hello XYZ" +
    // the retained base tail after the overlay region.
    assert_eq!(strip_terminal_sequences(&result), "hello XYZld".to_string());
    assert_eq!(visible_width_of(&result), 11);
}

fn visible_width_of(line: &str) -> usize {
    pillar_tui::text_utils::visible_width(line)
}

#[test]
fn composite_line_wide_overlay_expands() {
    // Overlay "日" renders 2 columns wide.
    let result = composite_tui_line("abcd", "日", 1, 2, 8);
    assert_eq!(visible_width_of(&result), 8);
}

#[test]
fn composite_line_resets_segments() {
    // Overlay content is wrapped in segment resets so styles don't bleed.
    let result = composite_tui_line("base", "xy", 0, 2, 10);
    assert!(result.contains("\u{1b}[0m\u{1b}]8;;\u{7}"), "{result:?}");
    // "after" retains the base tail past the overlay region.
    assert_eq!(strip_terminal_sequences(&result).trim_end(), "xyse");
}

// --- stack sizing -----------------------------------------------------------------------------

fn options(basis: Option<usize>, grow: usize, shrink: Option<usize>) -> StackEntryOptions {
    StackEntryOptions {
        basis,
        grow,
        shrink,
        ..Default::default()
    }
}

#[test]
fn allocate_sizes_uses_intrinsic_when_unavailable() {
    let entries = vec![options(None, 0, None), options(None, 0, None)];
    let sizes = allocate_stack_sizes(&entries, &[3, 5], None, 0);
    assert_eq!(sizes, vec![3, 5]);
}

#[test]
fn allocate_sizes_basis_overrides_intrinsic() {
    let entries = vec![options(Some(10), 0, None)];
    let sizes = allocate_stack_sizes(&entries, &[3], None, 0);
    assert_eq!(sizes, vec![10]);
}

#[test]
fn allocate_sizes_grows_to_fill() {
    let entries = vec![options(None, 1, None), options(None, 1, None)];
    let sizes = allocate_stack_sizes(&entries, &[2, 2], Some(10), 0);
    assert_eq!(sizes.iter().sum::<usize>(), 10);
}

#[test]
fn allocate_sizes_grow_proportional_to_weight() {
    let entries = vec![options(None, 1, None), options(None, 3, None)];
    // The upstream distribute loop iterates by weight share per round;
    // weights 1:3 over 8 columns converge to 3:5 (each round proposes
    // floor(remaining * weight / totalWeight)).
    let sizes = allocate_stack_sizes(&entries, &[0, 0], Some(8), 0);
    assert_eq!(sizes, vec![3, 5]);
    assert_eq!(sizes.iter().sum::<usize>(), 8);
}

#[test]
fn allocate_sizes_shrinks_respecting_min() {
    let entries = vec![
        StackEntryOptions {
            min_size: 2,
            ..options(None, 0, Some(1))
        },
        options(None, 0, Some(1)),
    ];
    // Available 4, intrinsics 3+3=6 → need to shrink 2; the min-size-2
    // entry cannot go below 2.
    let sizes = allocate_stack_sizes(&entries, &[3, 3], Some(4), 0);
    assert!(sizes[0] >= 2);
    assert_eq!(sizes.iter().sum::<usize>(), 4);
}

#[test]
fn allocate_sizes_gap_reduces_content_size() {
    let entries = vec![options(None, 1, None), options(None, 1, None)];
    let sizes = allocate_stack_sizes(&entries, &[2, 2], Some(11), 1);
    // Content size = 11 - 1 gap = 10, distributed to 5+5.
    assert_eq!(sizes.iter().sum::<usize>(), 10);
}

#[test]
fn allocate_sizes_max_size_caps_growth() {
    let entries = vec![
        StackEntryOptions {
            max_size: Some(3),
            ..options(None, 1, None)
        },
        options(None, 1, None),
    ];
    let sizes = allocate_stack_sizes(&entries, &[1, 1], Some(10), 0);
    assert_eq!(sizes[0], 3);
    assert_eq!(sizes[1], 7);
}

// --- VStack / HStack ------------------------------------------------------------------------------

#[test]
fn vstack_stacks_children_with_gap() {
    let mut children = vec![
        StackChild {
            render: Box::new(|_w| vec!["aaa".to_string(), "bbb".to_string()]),
            options: StackEntryOptions::default(),
        },
        StackChild {
            render: Box::new(|_w| vec!["ccc".to_string()]),
            options: StackEntryOptions::default(),
        },
    ];
    let lines = vstack_render(&mut children, 1, 10);
    assert_eq!(lines, vec!["aaa", "bbb", "", "ccc"]);
}

#[test]
fn vstack_pads_children_to_allocated_size() {
    let mut children = vec![
        StackChild {
            render: Box::new(|_w| vec!["one".to_string()]),
            options: StackEntryOptions {
                basis: Some(3),
                ..Default::default()
            },
        },
        StackChild {
            render: Box::new(|_w| vec!["two".to_string()]),
            options: StackEntryOptions::default(),
        },
    ];
    let lines = vstack_render(&mut children, 0, 10);
    // First child padded to 3 lines.
    assert_eq!(lines, vec!["one", "", "", "two"]);
}

#[test]
fn hstack_composites_side_by_side() {
    let mut children = vec![
        StackChild {
            render: Box::new(|_w| vec!["ab".to_string(), "cd".to_string()]),
            options: StackEntryOptions::default(),
        },
        StackChild {
            render: Box::new(|_w| vec!["xy".to_string()]),
            options: StackEntryOptions::default(),
        },
    ];
    let lines = hstack_render(&mut children, 1, "start", 10);
    let visible: Vec<String> = lines.iter().map(|l| strip_terminal_sequences(l)).collect();
    assert_eq!(visible, vec!["ab xy     ", "cd        "]);
}

#[test]
fn hstack_align_center_offsets() {
    let mut children = vec![
        StackChild {
            render: Box::new(|_w| vec!["a".to_string(), "b".to_string(), "c".to_string()]),
            options: StackEntryOptions::default(),
        },
        StackChild {
            render: Box::new(|_w| vec!["X".to_string()]),
            options: StackEntryOptions::default(),
        },
    ];
    let lines = hstack_render(&mut children, 1, "center", 10);
    let visible: Vec<String> = lines.iter().map(|l| strip_terminal_sequences(l)).collect();
    // "X" is centered vertically between the 3-line child.
    assert_eq!(visible, vec!["a         ", "b X       ", "c         "]);
}

#[test]
fn hstack_align_end_offsets() {
    let mut children = vec![
        StackChild {
            render: Box::new(|_w| vec!["a".to_string(), "b".to_string(), "c".to_string()]),
            options: StackEntryOptions::default(),
        },
        StackChild {
            render: Box::new(|_w| vec!["X".to_string()]),
            options: StackEntryOptions::default(),
        },
    ];
    let lines = hstack_render(&mut children, 1, "end", 10);
    let visible: Vec<String> = lines.iter().map(|l| strip_terminal_sequences(l)).collect();
    assert_eq!(visible, vec!["a         ", "b         ", "c X       "]);
}
