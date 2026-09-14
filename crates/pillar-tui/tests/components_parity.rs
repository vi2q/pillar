//! Parity tests for tui components: Text, Spacer, Box, TruncatedText
//! (pi v0.84.3).

use pillar_tui::components::{BoxComponent, Spacer, Text, TruncatedText};
use pillar_tui::text_utils::visible_width;

// --- Text ---------------------------------------------------------------------------------------

#[test]
fn text_wraps_and_pads() {
    let mut text = Text::new("hello world foo", 1, 1);
    let lines = text.render(15);
    // paddingY(1) + 2 content lines + paddingY(1)
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], " ".repeat(15));
    // Content lines are padded to full width.
    assert_eq!(lines[1], " hello world   ");
    assert_eq!(lines[2], " foo           ");
    assert_eq!(lines[3], " ".repeat(15));
}

#[test]
fn text_reduces_padding_to_fit() {
    // width 7, paddingX 2 → content width 3: the word hard-breaks.
    let mut text = Text::new("hello", 2, 0);
    let lines = text.render(7);
    assert_eq!(lines, vec!["  hel  ", "  lo   "]);
}

#[test]
fn text_empty_renders_nothing() {
    let mut text = Text::new("", 1, 1);
    assert!(text.render(20).is_empty());
    let mut text = Text::new("   ", 1, 1);
    assert!(text.render(20).is_empty());
}

#[test]
fn text_preserves_explicit_newlines() {
    let mut text = Text::new("a\nb", 0, 0);
    let lines = text.render(10);
    assert_eq!(lines, vec!["a         ", "b         "]);
}

#[test]
fn text_tabs_become_three_spaces() {
    let mut text = Text::new("a\tb", 0, 0);
    let lines = text.render(10);
    assert_eq!(lines[0], "a   b     ");
}

#[test]
fn text_cache_invalidates_on_set_text() {
    let mut text = Text::new("first", 0, 0);
    assert_eq!(text.render(10), vec!["first     "]);
    text.set_text("second");
    assert_eq!(text.render(10), vec!["second    "]);
}

#[test]
fn text_background_applied_and_padded() {
    let mut text = Text::with_bg("ab", 0, 0, Box::new(|t| format!("<{t}>")));
    let lines = text.render(5);
    assert_eq!(lines, vec!["<ab   >"]);
}

// --- Spacer ---------------------------------------------------------------------------------------

#[test]
fn spacer_renders_empty_lines() {
    let mut spacer = Spacer::new(3);
    assert_eq!(spacer.render(10), vec!["", "", ""]);
    spacer.set_lines(1);
    assert_eq!(spacer.render(10), vec![""]);
}

// --- Box ------------------------------------------------------------------------------------------

#[test]
fn box_applies_padding_and_background() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["hello".to_string()]));
    base.set_bg_fn(Some(Box::new(|t| format!("<{t}>"))));
    let lines = base.render(11);
    assert_eq!(lines, vec!["< hello     >"]);
}

#[test]
fn box_top_bottom_padding_and_children() {
    let mut base = BoxComponent::new(1, 1);
    base.add_child(Box::new(|_width| vec!["content".to_string()]));
    let lines = base.render(12);
    // paddingY(1) top + content + paddingY(1) bottom; width 12 with
    // paddingX 1 → content width 10, " content  " + pad.
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], " ".repeat(12));
    assert_eq!(lines[1], " content    ");
    assert_eq!(lines[2], " ".repeat(12));
}

#[test]
fn box_clear_removes_children() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["x".to_string()]));
    base.clear();
    assert!(base.render(10).is_empty());
}

#[test]
fn box_cache_tracks_bg_changes_by_sampling() {
    let mut base = BoxComponent::new(1, 0);
    base.add_child(Box::new(|_width| vec!["x".to_string()]));
    base.set_bg_fn(Some(Box::new(|t| format!("A{t}A"))));
    let first = base.render(5);
    assert_eq!(first, vec!["A x   A"]);
    // Same bg: cached output stays.
    let cached = base.render(5);
    assert_eq!(cached, vec!["A x   A"]);
    // Changed bg: re-render picks it up (bg change detected by sampling).
    base.set_bg_fn(Some(Box::new(|t| format!("B{t}B"))));
    let second = base.render(5);
    assert_eq!(second, vec!["B x   B"]);
}

// --- TruncatedText ----------------------------------------------------------------------------------

#[test]
fn truncated_text_truncates_with_ellipsis() {
    let component = TruncatedText::new("hello world", 0, 0);
    let lines = component.render(8);
    // finalizeTruncatedResult appends the reset around the ellipsis.
    assert_eq!(lines, vec!["hello\u{1b}[0m...\u{1b}[0m"]);
}

#[test]
fn truncated_text_stops_at_newline() {
    let component = TruncatedText::new("first\nsecond", 0, 0);
    let lines = component.render(20);
    assert_eq!(lines, vec!["first               "]);
}

#[test]
fn truncated_text_pads_and_applies_padding() {
    let component = TruncatedText::new("hi", 1, 1);
    let lines = component.render(10);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1], " hi       ");
}

#[test]
fn truncated_text_wide_chars() {
    let component = TruncatedText::new("日本語", 0, 0);
    let lines = component.render(5);
    // width 5 - ellipsis(3) = 2 target → 日 only, then "..." (wrapped in
    // reset codes like the ASCII case).
    assert_eq!(lines[0], "日\u{1b}[0m...\u{1b}[0m");
}

// --- Image (upstream components/image.ts) ---------------------------------------------------------

use pillar_tui::components::{Image, ImageOptions};
use pillar_tui::terminal_image::{
    CellDimensions, ImageDimensions, ImageProtocol, TerminalCapabilities, calculate_image_cell_size,
    get_cell_dimensions, get_kitty_image_metadata, set_capabilities, set_cell_dimensions,
};

fn caps(images: Option<ImageProtocol>, hyperlinks: bool) -> TerminalCapabilities {
    TerminalCapabilities {
        images,
        true_color: true,
        hyperlinks,
    }
}

fn image(dimensions: Option<ImageDimensions>, options: ImageOptions) -> Image {
    Image::new(
        "AAAA",
        "image/png",
        Box::new(|text| text.to_string()),
        options,
        dimensions,
    )
}

#[test]
fn image_falls_back_without_a_protocol() {
    set_capabilities(caps(None, false));
    let mut image = image(
        Some(ImageDimensions {
            width_px: 800,
            height_px: 600,
        }),
        ImageOptions {
            filename: Some("/tmp/shot.png".to_string()),
            ..Default::default()
        },
    );
    let lines = image.render(60);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0], "[Image: /tmp/shot.png [image/png] 800x600]");

    // The fallback is truncated to the requested width.
    let lines = image.render(18);
    assert_eq!(visible_width(&lines[0]), 18, "{:?}", lines[0]);
    // Same width answers the cached lines; a new width re-renders.
    let cached = image.render(18);
    assert_eq!(cached, lines);
    image.invalidate();
    assert_eq!(image.render(18).len(), 1);
}

#[test]
fn kitty_image_reserves_its_rows() {
    set_capabilities(caps(Some(ImageProtocol::Kitty), false));
    set_cell_dimensions(CellDimensions {
        width_px: 10,
        height_px: 20,
    });
    let dimensions = ImageDimensions {
        width_px: 200,
        height_px: 100,
    };
    let mut image = image(Some(dimensions), ImageOptions::default());
    assert!(image.image_id().is_none());

    let lines = image.render(80);
    let expected = calculate_image_cell_size(
        dimensions,
        60,
        None,
        get_cell_dimensions(),
    );
    assert_eq!(lines.len(), expected.rows, "{lines:?}");
    assert!(lines[0].starts_with("\u{1b}_G"), "{:?}", lines[0]);
    assert!(lines[0].contains("C=1"), "cursor stays put: {:?}", lines[0]);
    assert!(lines[0].contains(&format!("c={}", expected.columns)));
    assert!(lines[0].contains(&format!("r={}", expected.rows)));
    // Kitty allocates an id so the TUI can evict the placement later.
    let image_id = image.image_id().expect("kitty image id");
    assert!(lines[0].contains(&format!("i={image_id}")));
    assert_eq!(
        get_kitty_image_metadata(&lines[0]).map(|metadata| metadata.image_id),
        Some(image_id)
    );
    assert!(
        lines[1..].iter().all(|line| line.is_empty()),
        "{lines:?}"
    );
}

#[test]
fn iterm2_image_moves_the_cursor_back_up() {
    set_capabilities(caps(Some(ImageProtocol::Iterm2), false));
    set_cell_dimensions(CellDimensions {
        width_px: 10,
        height_px: 20,
    });
    let dimensions = ImageDimensions {
        width_px: 200,
        height_px: 100,
    };
    let mut image = image(Some(dimensions), ImageOptions::default());
    let lines = image.render(80);
    let expected = calculate_image_cell_size(
        dimensions,
        60,
        None,
        get_cell_dimensions(),
    );
    assert_eq!(lines.len(), expected.rows);
    // The first rows are blank; the last one moves up and draws the image.
    assert!(lines[..lines.len() - 1].iter().all(|line| line.is_empty()));
    let last = lines.last().expect("last line");
    assert!(last.starts_with(&format!("\u{1b}[{}A", expected.rows - 1)), "{last:?}");
    assert!(last.contains("\u{1b}]1337;File="), "{last:?}");
    assert!(image.image_id().is_none(), "iTerm2 needs no image id");
}

#[test]
fn image_respects_max_width_and_height_cells() {
    set_capabilities(caps(Some(ImageProtocol::Kitty), false));
    set_cell_dimensions(CellDimensions {
        width_px: 10,
        height_px: 20,
    });
    let dimensions = ImageDimensions {
        width_px: 400,
        height_px: 400,
    };
    let mut image = image(
        Some(dimensions),
        ImageOptions {
            max_width_cells: Some(12),
            max_height_cells: Some(2),
            ..Default::default()
        },
    );
    let lines = image.render(80);
    let expected = calculate_image_cell_size(dimensions, 12, Some(2), get_cell_dimensions());
    // The height cap wins for a square image: the cell count shrinks below
    // the width cap.
    assert_eq!(expected.rows, 2);
    assert_eq!(expected.columns, 4);
    assert_eq!(lines.len(), expected.rows);
    assert!(lines[0].contains("c=4"), "{:?}", lines[0]);
    assert!(lines[0].contains("r=2"), "{:?}", lines[0]);
}
