//! Parity tests for loader / cancellable-loader / alt-screen-flash /
//! scroll-view components (pi v0.84.3).

use std::time::{Duration, Instant};

use pillar_tui::loaders::{
    AltScreenFlashContainer, CancellableLoader, Loader, LoaderIndicatorOptions, ScrollView,
    ScrollViewOptions, ScrollViewScrollbar, render_text,
};

// --- Loader -----------------------------------------------------------------------------------

fn noop_color() -> Box<dyn Fn(&str) -> String + Send> {
    Box::new(|t| t.to_string())
}

fn bracket_color() -> Box<dyn Fn(&str) -> String + Send> {
    Box::new(|t| format!("<{t}>"))
}

#[test]
fn loader_default_frames_and_message() {
    let loader = Loader::new(noop_color(), noop_color(), "Loading...", None);
    let lines = loader.render(40);
    // Leading blank (Text margin 1) + indicator + message.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].starts_with(" ⠋ Loading..."), "{lines:?}");
}

#[test]
fn loader_custom_message() {
    let mut loader = Loader::new(noop_color(), noop_color(), "Working", None);
    loader.set_message("Done");
    let lines = loader.render(40);
    assert!(lines[1].starts_with(" ⠋ Done"), "{lines:?}");
}

#[test]
fn loader_colors_applied() {
    let mut loader = Loader::new(bracket_color(), bracket_color(), "msg", None);
    loader.start();
    let lines = loader.render(40);
    assert!(lines[1].starts_with(" <⠋> <msg>"), "{lines:?}");
}

#[test]
fn loader_tick_advances_frames() {
    let mut loader = Loader::new(noop_color(), noop_color(), "m", None);
    assert!(loader.tick());
    assert!(
        loader.render(40)[1].starts_with(" ⠙"),
        "{:?}",
        loader.render(40)
    );
    for _ in 0..9 {
        loader.tick();
    }
    // 10 frames wrap after ⠏ back to ⠋.
    assert!(
        loader.render(40)[1].starts_with(" ⠋"),
        "{:?}",
        loader.render(40)
    );
}

#[test]
fn loader_single_frame_does_not_animate() {
    let mut loader = Loader::new(
        noop_color(),
        noop_color(),
        "m",
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["*".to_string()]),
            interval_ms: None,
        }),
    );
    assert!(!loader.tick());
    assert!(
        loader.render(40)[1].starts_with(" * m"),
        "{:?}",
        loader.render(40)
    );
}

#[test]
fn loader_empty_frames_hide_indicator() {
    let mut loader = Loader::new(
        noop_color(),
        noop_color(),
        "m",
        Some(LoaderIndicatorOptions {
            frames: Some(vec![]),
            interval_ms: None,
        }),
    );
    loader.start();
    let lines = loader.render(40);
    assert!(lines[1].starts_with(" m"), "{lines:?}");
    assert!(!loader.tick());
}

#[test]
fn loader_indicator_renders_verbatim_without_color() {
    // Custom indicator frames render verbatim (no spinner color fn).
    let mut loader = Loader::new(
        bracket_color(),
        bracket_color(),
        "m",
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["-".to_string(), "+".to_string()]),
            interval_ms: None,
        }),
    );
    loader.start();
    let lines = loader.render(40);
    assert!(lines[1].starts_with(" - <m>"), "{lines:?}");
    assert!(loader.tick());
    assert!(
        loader.render(40)[1].starts_with(" + <m>"),
        "{:?}",
        loader.render(40)
    );
}

#[test]
fn loader_interval_default_and_custom() {
    let loader = Loader::new(noop_color(), noop_color(), "m", None);
    assert_eq!(loader.interval_ms(), 80);
    let loader = Loader::new(
        noop_color(),
        noop_color(),
        "m",
        Some(LoaderIndicatorOptions {
            frames: None,
            interval_ms: Some(250),
        }),
    );
    assert_eq!(loader.interval_ms(), 250);
    // Zero interval falls back to the default.
    let loader = Loader::new(
        noop_color(),
        noop_color(),
        "m",
        Some(LoaderIndicatorOptions {
            frames: None,
            interval_ms: Some(0),
        }),
    );
    assert_eq!(loader.interval_ms(), 80);
}

#[test]
fn render_text_pads_to_width() {
    let line = render_text("hi", 1, 0, 10);
    assert!(line[0].starts_with(" hi"), "{line:?}");
}

// --- CancellableLoader -------------------------------------------------------------------------

#[test]
fn cancellable_loader_aborts() {
    let mut loader = CancellableLoader::new(noop_color(), noop_color(), "m", None);
    assert!(!loader.aborted());
    loader.abort();
    assert!(loader.aborted());
}

#[test]
fn cancellable_loader_delegates_to_loader() {
    let mut loader = CancellableLoader::new(noop_color(), noop_color(), "working", None);
    let lines = loader.render(40);
    assert!(lines[1].starts_with(" ⠋ working"), "{lines:?}");
    loader.set_message("done");
    assert!(loader.render(40)[1].starts_with(" ⠋ done"));
    loader.dispose();
}

// --- AltScreenFlashContainer ----------------------------------------------------------------------

#[test]
fn flash_renders_inverse_video() {
    let mut flash = AltScreenFlashContainer::new();
    flash.flash("Saved", None);
    let lines = flash.render(40);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("\u{1b}[7m Saved \u{1b}[27m"),
        "{lines:?}"
    );
}

#[test]
fn flash_truncates_to_width() {
    let mut flash = AltScreenFlashContainer::new();
    flash.flash("a very long flash message indeed", None);
    let lines = flash.render(10);
    let visible = pillar_tui::text_utils::visible_width(&lines[0]);
    assert!(visible <= 10, "{lines:?}");
}

#[test]
fn flash_expiry_removes_entries() {
    let mut flash = AltScreenFlashContainer::new();
    let deadline = flash.flash("msg", Some(50));
    assert!(!flash.is_empty());
    // Not yet expired.
    assert!(!flash.expire(Instant::now()));
    assert!(!flash.is_empty());
    // After the deadline.
    let later = deadline + Duration::from_millis(1);
    assert!(flash.expire(later));
    assert!(flash.is_empty());
}

#[test]
fn flash_multiple_entries_render_in_order() {
    let mut flash = AltScreenFlashContainer::new();
    flash.flash("first", Some(10_000));
    flash.flash("second", Some(10_000));
    let lines = flash.render(40);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("first"), "{lines:?}");
    assert!(lines[1].contains("second"), "{lines:?}");
}

#[test]
fn flash_dispose_clears() {
    let mut flash = AltScreenFlashContainer::new();
    flash.flash("msg", Some(10_000));
    flash.dispose();
    assert!(flash.is_empty());
    assert_eq!(flash.render(40).len(), 0);
}

// --- ScrollView --------------------------------------------------------------------------------------

fn options() -> ScrollViewOptions {
    ScrollViewOptions::default()
}

#[test]
fn scroll_view_initial_state() {
    let view = ScrollView::new(options());
    assert_eq!(view.scroll_top(), 0);
    assert!(!view.is_following_end());
    assert_eq!(view.viewport_height(), 0);
}

#[test]
fn scroll_view_clamps_scroll_to() {
    let mut view = ScrollView::new(options());
    view.update_layout(100, 10);
    view.scroll_to(500, false);
    assert_eq!(view.scroll_top(), 90);
    view.scroll_to(-10, false);
    assert_eq!(view.scroll_top(), 0);
}

#[test]
fn scroll_view_scroll_by_returns_overscroll() {
    let mut view = ScrollView::new(options());
    view.update_layout(100, 10);
    let remainder = view.scroll_by(500);
    assert_eq!(view.scroll_top(), 90);
    assert_eq!(remainder, 410);
    let remainder = view.scroll_by(-500);
    assert_eq!(view.scroll_top(), 0);
    assert_eq!(remainder, -410);
}

#[test]
fn scroll_view_follow_end_tracks_bottom() {
    let mut view = ScrollView::new(ScrollViewOptions {
        follow_end: true,
        ..options()
    });
    view.update_layout(100, 10);
    assert!(view.is_following_end());
    assert_eq!(view.scroll_top(), 90);
    // Content grows while following → stays pinned to the end.
    view.update_layout(120, 10);
    assert!(view.is_following_end());
    assert_eq!(view.scroll_top(), 110);
}

#[test]
fn scroll_view_scrolling_up_disables_follow() {
    let mut view = ScrollView::new(ScrollViewOptions {
        follow_end: true,
        ..options()
    });
    view.update_layout(100, 10);
    assert!(view.is_following_end());
    view.scroll_by(-5);
    assert!(!view.is_following_end());
    // Growing content no longer moves the viewport.
    view.update_layout(120, 10);
    assert_eq!(view.scroll_top(), 85);
}

#[test]
fn scroll_view_scroll_to_end_reenables_follow() {
    let mut view = ScrollView::new(ScrollViewOptions {
        follow_end: true,
        ..options()
    });
    view.update_layout(100, 10);
    view.scroll_by(-5);
    assert!(!view.is_following_end());
    view.scroll_to_end();
    assert!(view.is_following_end());
    assert_eq!(view.scroll_top(), 90);
}

#[test]
fn scroll_view_scroll_to_start_resets() {
    let mut view = ScrollView::new(options());
    view.update_layout(100, 10);
    view.scroll_to(50, false);
    view.scroll_to_start();
    assert_eq!(view.scroll_top(), 0);
}

#[test]
fn scroll_view_disable_follow_suppresses_at_end() {
    let mut view = ScrollView::new(ScrollViewOptions {
        follow_end: true,
        ..options()
    });
    view.update_layout(100, 10);
    assert!(view.is_following_end());
    // scrollTo(max, disableFollow) jumps to the end but does not follow.
    view.scroll_to(90, true);
    assert_eq!(view.scroll_top(), 90);
    assert!(!view.is_following_end());
    // Growing content no longer pins.
    view.update_layout(120, 10);
    assert_eq!(view.scroll_top(), 90);
}

#[test]
fn scroll_view_content_width_with_always_scrollbar() {
    let mut view = ScrollView::new(options());
    assert_eq!(view.get_content_width(40), 40);
    view.set_scrollbar(ScrollViewScrollbar::Always);
    assert_eq!(view.get_content_width(40), 39);
    // Very narrow viewport keeps full width.
    assert_eq!(view.get_content_width(1), 1);
}

#[test]
fn scroll_view_render_pads_reserved_scrollbar_column() {
    let mut view = ScrollView::new(options());
    view.set_scrollbar(ScrollViewScrollbar::Always);
    let lines = view.render(40, &mut |_w| vec!["hello".to_string()]);
    assert_eq!(lines, vec!["hello ".to_string()]);
    // No padding when the scrollbar is hidden.
    view.set_scrollbar(ScrollViewScrollbar::Hidden);
    let lines = view.render(40, &mut |_w| vec!["hello".to_string()]);
    assert_eq!(lines, vec!["hello".to_string()]);
}

#[test]
fn scroll_view_always_scrollbar_visible() {
    let mut view = ScrollView::new(options());
    assert!(!view.is_scrollbar_visible());
    view.set_scrollbar(ScrollViewScrollbar::Always);
    view.update_layout(100, 10);
    assert!(view.is_scrollbar_visible());
}

#[test]
fn scroll_view_auto_scrollbar_transient_visibility() {
    let mut view = ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Auto,
        ..options()
    });
    view.update_layout(100, 10);
    assert!(!view.is_scrollbar_visible());
    // Scrolling marks activity → visible until the hide delay passes.
    view.scroll_by(1);
    assert!(view.is_scrollbar_visible());
    // After the delay (default 1000ms) it hides.
    let now = Instant::now() + Duration::from_millis(1001);
    assert!(view.tick_scrollbar(now));
    assert!(!view.is_scrollbar_visible());
}

#[test]
fn scroll_view_auto_scrollbar_not_shown_when_content_fits() {
    let mut view = ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Auto,
        ..options()
    });
    view.update_layout(5, 10);
    view.scroll_by(1);
    assert!(!view.is_scrollbar_visible());
}

#[test]
fn scroll_view_set_scrollbar_active_marks_visibility() {
    let mut view = ScrollView::new(ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Auto,
        ..options()
    });
    view.update_layout(100, 10);
    view.set_scrollbar_active(true);
    assert!(view.is_scrollbar_visible());
    view.set_scrollbar(ScrollViewScrollbar::Hidden);
    assert!(!view.is_scrollbar_visible());
}

#[test]
fn scroll_view_short_content_clamps_scroll_top() {
    let mut view = ScrollView::new(options());
    view.update_layout(100, 10);
    view.scroll_to(50, false);
    // Content shrinks below the viewport.
    view.update_layout(5, 10);
    assert_eq!(view.scroll_top(), 0);
}
