//! Port of packages/tui/src/tui-alt-screen.ts (pi v0.84.3), first slice: the
//! alternate-screen renderer's frame pipeline, viewport scrolling, search
//! state and flashes.
//!
//! Deferred to the next slices (recorded in docs/TASKS.md): mouse handling
//! (wheel / SGR / scrollbar), text selection with copy-on-select, and the
//! search overlay component (`AltScreenSearchComponent`). External layout
//! roots need the component → `LayoutNode` bridge, which the port does not
//! have yet, so only the implicit scroll view is supported.
//!
//! divergences:
//! - upstream stores the rendered `LayoutFrame` between frames and slices the
//!   viewport through it; the port's frame borrows its layout node, so the
//!   implicit viewport is sliced directly from the scroll state
//!   (`update_layout` + `scroll_top`/`viewport_height`) and only the derived
//!   state (rendered lines, scroll positions) is kept.
//! - kitty image upload/caching is not ported: images render as-is and the
//!   `evictedImageDeletion` step is a no-op.
//! - `TERM`/`TMUX`/`ZELLIJ` multiplexer detection chooses the mouse-motion
//!   sequence; mouse reporting is deferred, so only alt-screen entry/exit and
//!   autowrap toggling are emitted.

use std::cell::RefCell;
use std::time::Instant;

use crate::alt_screen::{AltScreenDiff, classify_alt_screen_diff, rows_to_paint};
use crate::alt_screen_search::{
    AltScreenSearchMatch, find_alt_screen_search_matches, get_alt_screen_search_match_key,
};
use crate::loaders::{AltScreenFlashContainer, ScrollView, ScrollViewOptions};
use crate::overlay::{
    apply_line_resets, composite_overlays, extract_cursor_position, prepare_overlay,
};
use crate::process_terminal::Terminal;
use crate::stack_layout::slice_by_column;
use crate::terminal_image::is_image_line;
use crate::text_utils::visible_width;
use crate::tui::{TuiBase, TuiMode, TuiStopOptions};

const ENTER_ALT_SCREEN: &str = "\u{1b}[?1049h";
const BEGIN_SYNCHRONIZED_OUTPUT: &str = "\u{1b}[?2026h";
const END_SYNCHRONIZED_OUTPUT: &str = "\u{1b}[?2026l";
const DISABLE_AUTOWRAP: &str = "\u{1b}[?7l";
const ENABLE_AUTOWRAP: &str = "\u{1b}[?7h";
const DISABLE_MOUSE: &str = "\u{1b}[?1000l\u{1b}[?1002l\u{1b}[?1003l\u{1b}[?1006l";
const OSC133_ZONE_PREFIX: &str = "\u{1b}]133;";
const OSC133_PROMPT_START: &str = "\u{1b}]133;A";

/// Styles a search match in the rendered frame.
pub type SearchMatchStyle = Box<dyn Fn(&str) -> String + Send>;
/// Opens an OSC 8 hyperlink clicked in the viewport.
pub type OpenUrlCallback = Box<dyn Fn(&str) + Send>;
/// Handles a secondary-button press for clipboard paste.
pub type RightClickPasteCallback = Box<dyn Fn() + Send>;

/// Options for [`TuiAltScreen`] (upstream `TuiAltScreenOptions`).
#[derive(Default)]
pub struct TuiAltScreenOptions {
    /// Logical lines moved per wheel event (upstream `wheelScrollLines`).
    pub wheel_scroll_lines: Option<usize>,
    /// Capture mouse events (deferred; used for the enable sequence).
    pub mouse: Option<bool>,
    /// Style a non-current search match.
    pub search_match_style: Option<SearchMatchStyle>,
    /// Style the current search match.
    pub search_current_match_style: Option<SearchMatchStyle>,
    /// Open an OSC 8 hyperlink activated with a primary click (deferred).
    pub open_url: Option<OpenUrlCallback>,
    /// Handle a secondary-button press for paste (deferred).
    pub on_right_click_paste: Option<RightClickPasteCallback>,
    /// Copy the selection on release (deferred; default true).
    pub copy_on_select: Option<bool>,
}

/// How the search selected its current match (upstream `selectionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSelectionMode {
    /// A new query: select the first match at/after the anchor row.
    #[default]
    Query,
    /// Move to the next match.
    Next,
    /// Move to the previous match.
    Previous,
    /// Keep the current selection when possible.
    Retain,
}

/// The active viewport search (upstream `ActiveSearch` minus the overlay).
#[derive(Debug, Clone, Default)]
pub struct ActiveSearch {
    pub query: String,
    pub matches: Vec<AltScreenSearchMatch>,
    pub selected_index: Option<usize>,
    pub selected_key: Option<String>,
    pub anchor_row: usize,
    pub selection_mode: SearchSelectionMode,
}

/// The alternate-screen renderer (upstream `TuiAltScreen`).
pub struct TuiAltScreen {
    base: TuiBase,
    previous_screen: Vec<String>,
    previous_screen_width: usize,
    previous_screen_height: usize,
    scroll: RefCell<ScrollView>,
    flashes: AltScreenFlashContainer,
    alt_screen_active: bool,
    active_search: Option<ActiveSearch>,
    wheel_scroll_lines: usize,
    mouse_enabled: bool,
    copy_on_select: bool,
    /// The scroll content lines of the last frame (search input).
    last_content_lines: Vec<String>,
}

impl TuiAltScreen {
    pub fn new(terminal: Box<dyn Terminal>, options: TuiAltScreenOptions) -> Self {
        Self {
            base: TuiBase::new(terminal, TuiMode::Fullscreen),
            previous_screen: Vec::new(),
            previous_screen_width: 0,
            previous_screen_height: 0,
            scroll: RefCell::new(ScrollView::new(ScrollViewOptions {
                follow_end: true,
                primary: true,
                ..Default::default()
            })),
            flashes: AltScreenFlashContainer::new(),
            alt_screen_active: false,
            active_search: None,
            wheel_scroll_lines: options.wheel_scroll_lines.unwrap_or(1).max(1),
            mouse_enabled: options.mouse.unwrap_or(true),
            copy_on_select: options.copy_on_select.unwrap_or(true),
            last_content_lines: Vec::new(),
        }
    }

    pub fn base(&self) -> &TuiBase {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }

    /// The viewport's scroll offset (upstream `viewportTop`).
    pub fn viewport_top(&self) -> usize {
        self.scroll.borrow().scroll_top()
    }

    /// Whether the viewport follows new output (upstream `isFollowingOutput`).
    pub fn is_following_output(&self) -> bool {
        self.scroll.borrow().is_following_end()
    }

    pub fn get_copy_on_select(&self) -> bool {
        self.copy_on_select
    }

    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.copy_on_select = enabled;
    }

    pub fn wheel_scroll_lines(&self) -> usize {
        self.wheel_scroll_lines
    }

    pub fn mouse_enabled(&self) -> bool {
        self.mouse_enabled
    }

    /// The active search, if any.
    pub fn active_search(&self) -> Option<&ActiveSearch> {
        self.active_search.as_ref()
    }

    pub fn flash(&mut self, message: &str, duration_ms: Option<u64>) {
        self.flashes.flash(message, duration_ms);
        self.base.request_render(false);
    }

    /// Drop a frame's incremental state (upstream `resetRenderState`).
    pub fn reset_render_state(&mut self) {
        self.previous_screen.clear();
        self.previous_screen_width = 0;
        self.previous_screen_height = 0;
    }

    pub fn scroll_by(&mut self, lines: i64) {
        self.scroll.borrow_mut().scroll_by(lines as isize);
        self.base.request_render(false);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll.borrow_mut().scroll_to_start();
        self.base.request_render(false);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll.borrow_mut().scroll_to_end();
        self.base.request_render(false);
    }

    /// Scroll to the previous/next OSC 133 prompt marker (upstream
    /// `scrollToPrompt`).
    pub fn scroll_to_prompt(&mut self, direction: i64) {
        if direction == 0 {
            return;
        }
        let current = self.scroll.borrow().scroll_top();
        let lines = self.last_content_lines.clone();
        let mut row = current as i64 + direction;
        while row >= 0 && (row as usize) < lines.len() {
            if lines[row as usize].contains(OSC133_PROMPT_START) {
                self.scroll.borrow_mut().scroll_to(row as isize, false);
                self.base.request_render(false);
                return;
            }
            row += direction;
        }
    }

    // --- search ----------------------------------------------------------

    /// Open the viewport search (upstream `openSearch`; the overlay component
    /// itself is deferred).
    pub fn open_search(&mut self) {
        if self.active_search.is_some() {
            return;
        }
        self.active_search = Some(ActiveSearch {
            anchor_row: self.viewport_top(),
            ..Default::default()
        });
    }

    pub fn close_search(&mut self) {
        if self.active_search.take().is_some() {
            self.base.request_render(false);
        }
    }

    pub fn update_search_query(&mut self, query: &str) {
        let top = self.viewport_top();
        let Some(search) = self.active_search.as_mut() else {
            return;
        };
        if search.query == query {
            return;
        }
        // Anchor on the current match so re-querying keeps the position.
        search.anchor_row = search
            .selected_index
            .and_then(|index| search.matches.get(index))
            .and_then(|matched| matched.segments.first())
            .map(|segment| segment.row)
            .unwrap_or(top);
        search.query = query.to_string();
        search.selection_mode = SearchSelectionMode::Query;
        self.base.request_render(false);
    }

    pub fn navigate_search(&mut self, direction: i64) {
        let Some(search) = self.active_search.as_mut() else {
            return;
        };
        if search.query.is_empty() {
            return;
        }
        search.selection_mode = if direction < 0 {
            SearchSelectionMode::Previous
        } else {
            SearchSelectionMode::Next
        };
        self.base.request_render(false);
    }

    /// Recompute the matches for the current frame (upstream `refreshSearch`),
    /// answering whether the viewport scrolled to reveal the selection.
    fn refresh_search(&mut self) -> bool {
        let Some(search) = self.active_search.as_mut() else {
            return false;
        };
        let lines: Vec<&str> = self.last_content_lines.iter().map(String::as_str).collect();
        if search.query.trim().is_empty() || lines.is_empty() {
            search.matches.clear();
            search.selected_index = None;
            search.selected_key = None;
            search.selection_mode = SearchSelectionMode::Retain;
            return false;
        }

        let should_reveal = search.selection_mode != SearchSelectionMode::Retain;
        let matches = find_alt_screen_search_matches(&lines, &search.query);
        let exact_index = search
            .selected_key
            .as_ref()
            .and_then(|key| {
                matches
                    .iter()
                    .position(|matched| &get_alt_screen_search_match_key(matched) == key)
            })
            .map(|index| index as isize)
            .unwrap_or(-1);
        let selected_index = if matches.is_empty() {
            None
        } else {
            let selected = match search.selection_mode {
                SearchSelectionMode::Query => matches
                    .iter()
                    .position(|matched| {
                        matched
                            .segments
                            .first()
                            .map(|segment| segment.row)
                            .unwrap_or(0)
                            >= search.anchor_row
                    })
                    .unwrap_or(0),
                SearchSelectionMode::Next => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.unwrap_or(0).min(matches.len() - 1) as isize
                    };
                    if base < 0 {
                        0
                    } else {
                        ((base + 1) as usize) % matches.len()
                    }
                }
                SearchSelectionMode::Previous => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.unwrap_or(0).min(matches.len() - 1) as isize
                    };
                    if base < 0 {
                        matches.len() - 1
                    } else {
                        ((base - 1 + matches.len() as isize) as usize) % matches.len()
                    }
                }
                SearchSelectionMode::Retain => {
                    if exact_index >= 0 {
                        exact_index as usize
                    } else {
                        search.selected_index.unwrap_or(0).min(matches.len() - 1)
                    }
                }
            };
            Some(selected)
        };

        search.selected_key = selected_index
            .and_then(|index| matches.get(index))
            .map(get_alt_screen_search_match_key);
        search.matches = matches;
        search.selected_index = selected_index;
        search.selection_mode = SearchSelectionMode::Retain;
        if !should_reveal {
            return false;
        }

        let Some(index) = selected_index else {
            return false;
        };
        let viewport_height = self.scroll.borrow().viewport_height();
        if viewport_height == 0 {
            return false;
        }
        let before = self.scroll.borrow().scroll_top();
        let visible_bottom = before + viewport_height - 1;
        let (first_row, last_row) = {
            let matched = &self.active_search.as_ref().expect("search").matches[index];
            let Some(first) = matched.segments.first() else {
                return false;
            };
            let Some(last) = matched.segments.last() else {
                return false;
            };
            (first.row, last.row)
        };
        let mut target = before;
        if first_row < before || last_row > visible_bottom {
            target = first_row.saturating_sub(viewport_height / 3);
        }
        self.scroll.borrow_mut().scroll_to(target as isize, true);
        self.scroll.borrow().scroll_top() != before
    }

    // --- lifecycle -------------------------------------------------------

    pub fn start(&mut self) {
        self.alt_screen_active = true;
        self.reset_render_state();
        let sequence = format!("{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}\u{1b}[2J\u{1b}[H\u{1b}[?25l");
        self.base.terminal_mut().write(&sequence);
        self.base.start();
    }

    pub fn stop(&mut self, options: TuiStopOptions) {
        self.close_search();
        if !self.alt_screen_active {
            self.base.stop(options);
            return;
        }
        let sequence = format!(
            "{BEGIN_SYNCHRONIZED_OUTPUT}{DISABLE_MOUSE}{ENABLE_AUTOWRAP}{END_SYNCHRONIZED_OUTPUT}"
        );
        self.base.terminal_mut().write(&sequence);
        self.alt_screen_active = false;
        self.base.stop(options);
    }

    /// Remove expired flashes; returns whether anything changed (the host
    /// then requests a render).
    pub fn expire_flashes(&mut self, now: Instant) -> bool {
        self.flashes.expire(now)
    }

    /// Render one frame (upstream `doRender`).
    pub fn do_render(&mut self) -> Result<(), String> {
        if self.base.is_stopped() || !self.alt_screen_active {
            return Ok(());
        }
        let width = self.base.terminal().columns().max(1);
        let height = self.base.terminal().rows().max(1);

        let mut screen = self.render_frame(width, height);
        if self.refresh_search() {
            screen = self.render_frame(width, height);
        }
        // Zone prefixes are stripped before compositing.
        screen = screen
            .iter()
            .map(|line| line.replace(OSC133_ZONE_PREFIX, ""))
            .collect();
        screen = self.compose_overlays(screen, width, height);
        if screen.len() > height {
            let excess = screen.len() - height;
            screen.drain(..excess);
        }
        screen = self.composite_flashes(screen, width, height);

        let cursor_pos = extract_cursor_position(&mut screen, height);
        screen = apply_line_resets(screen)
            .into_iter()
            .map(|line| {
                if is_image_line(&line) || visible_width(&line) <= width {
                    line
                } else {
                    slice_by_column(&line, 0, width, true)
                }
            })
            .collect();

        let full_redraw = self.previous_screen.is_empty()
            || self.previous_screen_width != width
            || self.previous_screen_height != height;
        let images_need_redraw = screen.iter().enumerate().any(|(row, line)| {
            line != self.previous_screen.get(row).unwrap_or(&String::new())
                && (is_image_line(line)
                    || self
                        .previous_screen
                        .get(row)
                        .is_some_and(|previous| is_image_line(previous)))
        });
        let redraw_images = full_redraw || images_need_redraw;
        // Kitty image upload/caching is not ported (see module divergence).
        // Kitty image upload/caching is not ported: lines pass through and
        // nothing is evicted.
        let prepared = crate::alt_screen::PreparedKittyScreen {
            lines: screen.clone(),
            evicted_image_deletion: String::new(),
        };
        let _ = redraw_images;

        let mut buffer = String::from(BEGIN_SYNCHRONIZED_OUTPUT);
        if full_redraw {
            self.base.note_full_redraw();
            buffer.push_str("\u{1b}[2J");
        } else if images_need_redraw {
            buffer.push_str("\u{1b}[2J");
        }
        buffer.push_str(&prepared.evicted_image_deletion);

        let diff = classify_alt_screen_diff(
            &self.previous_screen,
            self.previous_screen_width,
            self.previous_screen_height,
            &screen,
            width,
            height,
        );
        let _ = AltScreenDiff::FullRedraw;
        for row in rows_to_paint(diff, &self.previous_screen, &screen, height) {
            buffer.push_str(&crate::alt_screen::row_update_escape(
                row,
                prepared.lines.get(row).map(String::as_str).unwrap_or(""),
            ));
        }

        match cursor_pos {
            Some((row, col)) => {
                buffer.push_str(&format!("\u{1b}[{};{}H", row + 1, col.min(width) + 1));
                buffer.push_str(if self.base.show_hardware_cursor() {
                    "\u{1b}[?25h"
                } else {
                    "\u{1b}[?25l"
                });
            }
            None => buffer.push_str("\u{1b}[?25l"),
        }
        buffer.push_str(END_SYNCHRONIZED_OUTPUT);
        self.base.terminal_mut().write(&buffer);

        self.previous_screen = screen;
        self.previous_screen_width = width;
        self.previous_screen_height = height;
        Ok(())
    }

    /// Lay out the implicit scroll view over the registered children.
    fn render_frame(&mut self, width: usize, height: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        {
            // The child renders through the base's component tree.
            let rendered = self.base.render(width);
            lines.extend(rendered);
            self.last_content_lines = lines.clone();
        }
        // Scroll state update + viewport slicing (the layout frame's job).
        let viewport_height = {
            let mut scroll = self.scroll.borrow_mut();
            scroll.update_layout(lines.len(), height);
            scroll.viewport_height()
        };
        let scroll_top = self.scroll.borrow().scroll_top();
        let viewport_height = if viewport_height == 0 {
            height
        } else {
            viewport_height
        };
        let start = scroll_top.min(lines.len().saturating_sub(1).max(0));
        let end = (start + viewport_height).min(lines.len());
        let mut visible: Vec<String> = lines[start..end].to_vec();
        while visible.len() < height {
            visible.push(String::new());
        }
        visible.truncate(height);
        visible
    }

    fn compose_overlays(&mut self, lines: Vec<String>, width: usize, height: usize) -> Vec<String> {
        if !self.base.has_overlay_entries() {
            return lines;
        }
        let ids = self.base.overlay_ids();
        let mut overlays: Vec<(Vec<String>, crate::overlay::ResolvedOverlayLayout)> = Vec::new();
        for id in ids {
            let options = self.base.overlay_options(id).cloned().unwrap_or_default();
            if !options.is_visible(width, height) {
                continue;
            }
            let Some(component) = self.base.overlay_component_mut(id) else {
                continue;
            };
            let overlay_lines = component.render(width);
            overlays.push(prepare_overlay(
                Some(&options),
                overlay_lines,
                width,
                height,
            ));
        }
        composite_overlays(lines, &mut overlays, width, height)
    }

    fn composite_flashes(
        &mut self,
        lines: Vec<String>,
        width: usize,
        height: usize,
    ) -> Vec<String> {
        if self.flashes.is_empty() {
            return lines;
        }
        let flash_lines = self.flashes.render(width);
        if flash_lines.is_empty() {
            return lines;
        }
        let mut result = lines;
        while result.len() < height {
            result.push(String::new());
        }
        // Flashes sit on the last rows of the viewport.
        let start = height.saturating_sub(flash_lines.len());
        for (offset, line) in flash_lines.iter().enumerate() {
            if start + offset < result.len() {
                result[start + offset] = line.clone();
            }
        }
        result
    }
}
