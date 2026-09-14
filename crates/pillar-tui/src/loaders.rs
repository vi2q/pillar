//! Port of packages/tui/src/components (pi v0.84.3): loader.ts,
//! cancellable-loader.ts, alt-screen-flash.ts, and scroll-view.ts.
//!
//! divergences: the animation timers (Loader setInterval, flash
//! setTimeout, scrollbar hide delay) are driven by the host through
//! explicit `tick`/expiry methods instead of real timers; keybinding
//! dispatch stays host-side.

use std::collections::VecDeque;
use std::time::Instant;

use crate::text_utils::{truncate_to_width, visible_width};

// ============================================================================
// Loader (upstream loader.ts)
// ============================================================================

const DEFAULT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DEFAULT_INTERVAL_MS: u64 = 80;

/// Loader indicator options (upstream `LoaderIndicatorOptions`).
#[derive(Clone)]
pub struct LoaderIndicatorOptions {
    /// Animation frames. Empty hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds for animated indicators.
    pub interval_ms: Option<u64>,
}

/// Loader with an optional spinning animation (upstream `Loader`).
/// Rendering is one line: optional indicator + colored message.
pub struct Loader {
    frames: Vec<String>,
    interval_ms: u64,
    current_frame: usize,
    render_indicator_verbatim: bool,
    spinner_color: Box<dyn Fn(&str) -> String + Send>,
    message_color: Box<dyn Fn(&str) -> String + Send>,
    message: String,
    /// The composed Text content (upstream extends Text).
    text: String,
}

impl Loader {
    pub fn new(
        spinner_color: Box<dyn Fn(&str) -> String + Send>,
        message_color: Box<dyn Fn(&str) -> String + Send>,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        let mut loader = Self {
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current_frame: 0,
            render_indicator_verbatim: false,
            spinner_color,
            message_color,
            message: message.to_string(),
            text: String::new(),
        };
        loader.set_indicator(indicator);
        loader
    }

    pub fn start(&mut self) {
        self.update_display();
        // Timer restart is host-driven via `tick`.
    }

    pub fn stop(&mut self) {
        // Host-driven: nothing to clear without a real timer.
    }

    pub fn set_message(&mut self, message: &str) {
        self.message = message.to_string();
        self.update_display();
    }

    /// The raw message (upstream the inherited Text content).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The composed, styled display text.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.render_indicator_verbatim = indicator.is_some();
        if let Some(indicator) = indicator {
            if let Some(frames) = indicator.frames {
                self.frames = frames;
            } else {
                self.frames = DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect();
            }
            if let Some(interval) = indicator.interval_ms {
                if interval > 0 {
                    self.interval_ms = interval;
                } else {
                    self.interval_ms = DEFAULT_INTERVAL_MS;
                }
            } else {
                self.interval_ms = DEFAULT_INTERVAL_MS;
            }
        } else {
            self.frames = DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect();
            self.interval_ms = DEFAULT_INTERVAL_MS;
        }
        self.current_frame = 0;
        self.start();
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Advance one animation frame (host timer callback, upstream the
    /// setInterval body). Returns true when the display changed.
    pub fn tick(&mut self) -> bool {
        if self.frames.len() <= 1 {
            return false;
        }
        self.current_frame = (self.current_frame + 1) % self.frames.len();
        self.update_display();
        true
    }

    fn update_display(&mut self) {
        let frame = self
            .frames
            .get(self.current_frame)
            .cloned()
            .unwrap_or_default();
        let rendered_frame = if self.render_indicator_verbatim {
            frame.clone()
        } else {
            (self.spinner_color)(&frame)
        };
        let indicator = if !frame.is_empty() {
            format!("{rendered_frame} ")
        } else {
            String::new()
        };
        self.text = format!("{indicator}{}", (self.message_color)(&self.message));
    }

    /// Render one line with a leading blank (upstream `render` returns
    /// ["", ...super.render(width)] — the leading blank is the Text
    /// component's top margin of 1).
    pub fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(render_text(&self.text, 1, 0, width));
        lines
    }
}

// ============================================================================
// Alt-screen flash (upstream alt-screen-flash.ts)
// ============================================================================

const DEFAULT_DURATION_MS: u64 = 1000;

struct FlashEntry {
    #[allow(dead_code)]
    id: u64,
    message: String,
    expires_at: Instant,
}

/// Transient messages composited by the alternate-screen renderer
/// (upstream `AltScreenFlashContainer`). Timer expiry is host-driven
/// via [`expire`].
///
/// [`expire`]: AltScreenFlashContainer::expire
pub struct AltScreenFlashContainer {
    entries: VecDeque<FlashEntry>,
    next_id: u64,
}

impl AltScreenFlashContainer {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 0,
        }
    }

    /// Show a message for a duration (upstream `flash`). The host should
    /// schedule a render check at `duration_ms`.
    pub fn flash(&mut self, message: &str, duration_ms: Option<u64>) -> Instant {
        let duration = duration_ms.unwrap_or(DEFAULT_DURATION_MS);
        let expires_at = Instant::now() + std::time::Duration::from_millis(duration);
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(FlashEntry {
            id,
            message: message.to_string(),
            expires_at,
        });
        expires_at
    }

    /// Remove expired entries (host timer callback). Returns true when
    /// anything was removed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at > now);
        self.entries.len() != before
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn dispose(&mut self) {
        self.entries.clear();
    }

    /// Render each entry as an inverse-video line (upstream `render`).
    pub fn render(&self, width: usize) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                let message = truncate_to_width(&format!(" {} ", entry.message), width, "", false);
                format!("\u{1b}[7m{message}\u{1b}[27m")
            })
            .collect()
    }
}

impl Default for AltScreenFlashContainer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Scroll view (upstream scroll-view.ts)
// ============================================================================

/// Scrollbar visibility mode (upstream `ScrollViewScrollbar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollViewScrollbar {
    Hidden,
    Auto,
    Always,
}

/// Scroll view options (upstream `ScrollViewOptions`).
pub struct ScrollViewOptions {
    pub follow_end: bool,
    pub primary: bool,
    pub overscroll_contain: bool,
    pub scrollbar: ScrollViewScrollbar,
    pub scrollbar_hide_delay_ms: u64,
}

impl Default for ScrollViewOptions {
    fn default() -> Self {
        Self {
            follow_end: false,
            primary: false,
            overscroll_contain: false,
            scrollbar: ScrollViewScrollbar::Hidden,
            scrollbar_hide_delay_ms: 1000,
        }
    }
}

/// Vertical scroll container with follow-end and scrollbar state
/// (upstream `ScrollView`). The child renders through a closure; layout
/// updates and scrollbar timers are host-driven.
pub struct ScrollView {
    follow_end: bool,
    pub primary: bool,
    pub overscroll_contain: bool,
    scrollbar_hide_delay_ms: u64,
    current_scrollbar: ScrollViewScrollbar,
    current_scroll_top: usize,
    content_height: usize,
    current_viewport_height: usize,
    following_end: bool,
    follow_suppressed_at_end: bool,
    scrollbar_active: bool,
    transient_scrollbar_visible: bool,
    next_hide_deadline: Option<Instant>,
}

impl ScrollView {
    pub fn new(options: ScrollViewOptions) -> Self {
        let follow_end = options.follow_end;
        Self {
            follow_end,
            primary: options.primary,
            overscroll_contain: options.overscroll_contain,
            scrollbar_hide_delay_ms: options.scrollbar_hide_delay_ms,
            current_scrollbar: options.scrollbar,
            current_scroll_top: 0,
            content_height: 0,
            current_viewport_height: 0,
            following_end: follow_end,
            follow_suppressed_at_end: false,
            scrollbar_active: false,
            transient_scrollbar_visible: false,
            next_hide_deadline: None,
        }
    }

    pub fn scroll_top(&self) -> usize {
        self.current_scroll_top
    }

    pub fn is_following_end(&self) -> bool {
        self.following_end
    }

    pub fn viewport_height(&self) -> usize {
        self.current_viewport_height
    }

    pub fn scrollbar(&self) -> ScrollViewScrollbar {
        self.current_scrollbar
    }

    pub fn is_scrollbar_visible(&self) -> bool {
        if self.current_scrollbar == ScrollViewScrollbar::Always {
            return self.current_viewport_height > 0;
        }
        self.current_scrollbar == ScrollViewScrollbar::Auto
            && self.content_height > self.current_viewport_height
            && self.transient_scrollbar_visible
    }

    pub fn set_scrollbar(&mut self, scrollbar: ScrollViewScrollbar) {
        if scrollbar == self.current_scrollbar {
            return;
        }
        self.current_scrollbar = scrollbar;
        if scrollbar != ScrollViewScrollbar::Auto {
            self.hide_transient_scrollbar();
        } else if self.scrollbar_active {
            self.mark_scrollbar_activity(Instant::now());
        }
    }

    /// Content width given the viewport width (upstream
    /// `getContentWidth`): one column less when the scrollbar is always
    /// shown.
    pub fn get_content_width(&self, width: usize) -> usize {
        if self.current_scrollbar == ScrollViewScrollbar::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    fn mark_scrollbar_activity(&mut self, now: Instant) {
        if self.current_scrollbar != ScrollViewScrollbar::Auto
            || self.content_height <= self.current_viewport_height
        {
            return;
        }
        self.transient_scrollbar_visible = true;
        if self.scrollbar_active && self.next_hide_deadline.is_some() {
            // Already scheduled; keep the existing deadline only if it is
            // in the future (upstream clearTimeout + reschedule).
        }
        self.next_hide_deadline =
            Some(now + std::time::Duration::from_millis(self.scrollbar_hide_delay_ms));
    }

    fn hide_transient_scrollbar(&mut self) {
        self.transient_scrollbar_visible = false;
        self.next_hide_deadline = None;
    }

    /// Fire the pending scrollbar hide timer if due. Returns true when
    /// the visibility changed (host should re-render).
    pub fn tick_scrollbar(&mut self, now: Instant) -> bool {
        if let Some(deadline) = self.next_hide_deadline {
            if now >= deadline {
                self.next_hide_deadline = None;
                if self.transient_scrollbar_visible {
                    self.transient_scrollbar_visible = false;
                    return true;
                }
            }
        }
        false
    }

    pub fn set_scrollbar_active(&mut self, active: bool) {
        if active == self.scrollbar_active {
            return;
        }
        self.scrollbar_active = active;
        self.mark_scrollbar_activity(Instant::now());
    }

    /// Scroll to an absolute offset (upstream `scrollTo`).
    pub fn scroll_to(&mut self, scroll_top: isize, disable_follow: bool) {
        let max_scroll_top = max_scroll_top(self.content_height, self.current_viewport_height);
        let next = scroll_top.clamp(0, max_scroll_top as isize) as usize;
        let next_follow_suppressed_at_end = disable_follow && next == max_scroll_top;
        let next_following_end =
            !next_follow_suppressed_at_end && self.follow_end && next == max_scroll_top;
        if next == self.current_scroll_top
            && next_following_end == self.following_end
            && next_follow_suppressed_at_end == self.follow_suppressed_at_end
        {
            return;
        }
        let moved = next != self.current_scroll_top;
        self.current_scroll_top = next;
        self.following_end = next_following_end;
        self.follow_suppressed_at_end = next_follow_suppressed_at_end;
        if moved {
            self.mark_scrollbar_activity(Instant::now());
        }
    }

    /// Scroll by a relative amount (upstream `scrollBy`). Returns the
    /// unconsumed remainder (overscroll).
    pub fn scroll_by(&mut self, lines: isize) -> isize {
        if lines == 0 {
            return 0;
        }
        let max_scroll_top = max_scroll_top(self.content_height, self.current_viewport_height);
        let start = if self.following_end {
            max_scroll_top as isize
        } else {
            self.current_scroll_top as isize
        };
        let next = (start + lines).clamp(0, max_scroll_top as isize);
        let moved = next - start;
        let was_following_end = self.following_end;
        self.current_scroll_top = next.max(0) as usize;
        self.following_end = self.follow_end && next == max_scroll_top as isize;
        self.follow_suppressed_at_end = false;
        if moved != 0 {
            self.mark_scrollbar_activity(Instant::now());
        }
        let _ = was_following_end;
        lines - moved
    }

    pub fn scroll_to_start(&mut self) {
        let expected_following =
            self.follow_end && self.content_height <= self.current_viewport_height;
        let changed = self.current_scroll_top != 0 || self.following_end != expected_following;
        self.current_scroll_top = 0;
        self.following_end = expected_following;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_scrollbar_activity(Instant::now());
        }
    }

    pub fn scroll_to_end(&mut self) {
        let next = max_scroll_top(self.content_height, self.current_viewport_height);
        let changed = self.current_scroll_top != next || self.following_end != self.follow_end;
        self.current_scroll_top = next;
        self.following_end = self.follow_end;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_scrollbar_activity(Instant::now());
        }
    }

    /// Update layout metrics (upstream `updateLayout`): clamps scrollTop
    /// and re-evaluates follow-end.
    pub fn update_layout(&mut self, content_height: usize, viewport_height: usize) {
        self.content_height = content_height;
        self.current_viewport_height = viewport_height;
        let max_scroll_top = max_scroll_top(self.content_height, self.current_viewport_height);
        if self.following_end {
            self.current_scroll_top = max_scroll_top;
        } else {
            self.current_scroll_top = self.current_scroll_top.min(max_scroll_top);
        }
        if self.current_scroll_top < max_scroll_top {
            self.follow_suppressed_at_end = false;
        }
        if self.follow_end
            && self.current_scroll_top == max_scroll_top
            && !self.follow_suppressed_at_end
        {
            self.following_end = true;
        }
        if self.content_height <= self.current_viewport_height {
            self.hide_transient_scrollbar();
        }
    }

    /// Render the child through a closure; appends one space per line
    /// when the scrollbar reserves a column (upstream `render`).
    pub fn render(&self, width: usize, child: &mut dyn FnMut(usize) -> Vec<String>) -> Vec<String> {
        let content_width = self.get_content_width(width);
        let mut lines = child(content_width);
        if content_width != width {
            for line in &mut lines {
                line.push(' ');
            }
        }
        lines
    }
}

fn max_scroll_top(content_height: usize, viewport_height: usize) -> usize {
    content_height.saturating_sub(viewport_height)
}

// ============================================================================
// Cancellable loader (upstream cancellable-loader.ts)
// ============================================================================

/// Loader with abort support (upstream `CancellableLoader`). The
/// keybinding check stays host-side: the host calls [`abort`] when the
/// cancel key matches.
///
/// [`abort`]: CancellableLoader::abort
pub struct CancellableLoader {
    loader: Loader,
    aborted: bool,
}

impl CancellableLoader {
    pub fn new(
        spinner_color: Box<dyn Fn(&str) -> String + Send>,
        message_color: Box<dyn Fn(&str) -> String + Send>,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        Self {
            loader: Loader::new(spinner_color, message_color, message, indicator),
            aborted: false,
        }
    }

    pub fn abort(&mut self) {
        self.aborted = true;
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl std::ops::Deref for CancellableLoader {
    type Target = Loader;

    fn deref(&self) -> &Self::Target {
        &self.loader
    }
}

impl std::ops::DerefMut for CancellableLoader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.loader
    }
}

/// Re-exported Text rendering used by the loader (upstream Loader
/// extends Text with margin 1, pad 0).
pub fn render_text(text: &str, margin_x: usize, margin_y: usize, width: usize) -> Vec<String> {
    let _ = margin_y;
    let content_width = width.saturating_sub(margin_x * 2).max(1);
    let line = truncate_to_width(text, content_width, "", true);
    vec![format!("{}{}", " ".repeat(margin_x), line)]
}

// Keep visible_width linked for parity tests.
#[allow(dead_code)]
fn _unused_visible_width(s: &str) -> usize {
    visible_width(s)
}


// ============================================================================
// Component impls (upstream Loader/CancellableLoader extend Text)
// ============================================================================

impl crate::tui::Component for Loader {
    fn render(&mut self, width: usize) -> Vec<String> {
        Loader::render(self, width)
    }
}

impl crate::tui::Component for CancellableLoader {
    fn render(&mut self, width: usize) -> Vec<String> {
        Loader::render(self, width)
    }
}
