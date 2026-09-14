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
use crate::keybindings::with_global_keybindings;
use crate::layout::{LayoutNode, ScrollbarGeometry, get_scrollbar_geometry, render_layout_frame};
use crate::loaders::{AltScreenFlashContainer, ScrollView, ScrollViewOptions};
use crate::overlay::{
    apply_line_resets, composite_overlays, extract_cursor_position, prepare_overlay,
};
use crate::process_terminal::Terminal;
use crate::stack_layout::slice_by_column;
use crate::terminal_image::is_image_line;
use crate::text_utils::visible_width;
use crate::tui::{InputListenerResult, TuiBase, TuiMode, TuiStopOptions};

const ENTER_ALT_SCREEN: &str = "\u{1b}[?1049h";
const BEGIN_SYNCHRONIZED_OUTPUT: &str = "\u{1b}[?2026h";
const END_SYNCHRONIZED_OUTPUT: &str = "\u{1b}[?2026l";
const DISABLE_AUTOWRAP: &str = "\u{1b}[?7l";
const ENABLE_AUTOWRAP: &str = "\u{1b}[?7h";
const DISABLE_MOUSE: &str = "\u{1b}[?1006l\u{1b}[?1004l\u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l";
/// Button-motion mouse tracking (multiplexers lag when every move is sent).
const ENABLE_BUTTON_MOTION_MOUSE: &str = "\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1004h\u{1b}[?1006h";
/// All-motion mouse tracking.
const ENABLE_ALL_MOTION_MOUSE: &str =
    "\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1003h\u{1b}[?1004h\u{1b}[?1006h";
const FOCUS_IN: &str = "\u{1b}[I";
const FOCUS_OUT: &str = "\u{1b}[O";
/// Lines of overlap when scrolling by a page (upstream `PAGE_SCROLL_OVERLAP`).
const PAGE_SCROLL_OVERLAP: usize = 4;
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
    /// The overlay hosting the search input, when one is shown (the overlay
    /// component itself is a later slice).
    pub overlay_id: Option<u64>,
    /// Whether the search overlay currently owns focus.
    pub focused: bool,
    pub query: String,
    pub matches: Vec<AltScreenSearchMatch>,
    pub selected_index: Option<usize>,
    pub selected_key: Option<String>,
    pub anchor_row: usize,
    pub selection_mode: SearchSelectionMode,
}

/// A parsed SGR mouse event (upstream `SgrMouseEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SgrMouseEvent {
    pub button: u32,
    pub x: usize,
    pub y: usize,
    pub release: bool,
}

/// A parsed wheel event (upstream `WheelEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelEvent {
    /// -1 = up, 1 = down.
    pub direction: i64,
    pub x: usize,
    pub y: usize,
}

/// An in-progress scrollbar drag (upstream `ScrollbarDrag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarDrag {
    grab_offset: usize,
}

/// The primary scroll view's hit-test geometry from the last frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollHitSnapshot {
    pub rect: (usize, usize, usize, usize),
    pub scrollbar: Option<ScrollbarGeometry>,
}

/// Parse an SGR mouse event (upstream `parseSgrMouseEvent`).
pub fn parse_sgr_mouse_event(data: &str) -> Option<SgrMouseEvent> {
    let rest = data.strip_prefix("\u{1b}[<")?;
    let rest = rest
        .strip_suffix('m')
        .map(|inner| (inner, true))
        .or_else(|| rest.strip_suffix('M').map(|inner| (inner, false)))?;
    let (inner, release) = rest;
    let mut parts = inner.split(';');
    let button = parts.next()?.parse::<u32>().ok()?;
    let x = parts.next()?.parse::<usize>().ok()?;
    let y = parts.next()?.parse::<usize>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SgrMouseEvent {
        button,
        x: x.saturating_sub(1),
        y: y.saturating_sub(1),
        release,
    })
}

/// Parse a wheel event from SGR or legacy X10 encoding (upstream
/// `parseWheelEvent`).
pub fn parse_wheel_event(data: &str) -> Option<WheelEvent> {
    if let Some(event) = parse_sgr_mouse_event(data) {
        if event.button & 64 == 0 {
            return None;
        }
        let direction = event.button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: event.x,
            y: event.y,
        });
    }
    // Legacy X10 encoding: ESC [ M <button> <x> <y>
    let bytes = data.as_bytes();
    if bytes.len() == 6 && data.starts_with("\u{1b}[M") {
        let button = bytes[3] as i64 - 32;
        if button & 64 == 0 {
            return None;
        }
        let direction = button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: (bytes[4] as i64 - 33).max(0) as usize,
            y: (bytes[5] as i64 - 33).max(0) as usize,
        });
    }
    None
}

/// Whether the sequence is any mouse report (upstream `isMouseSequence`).
pub fn is_mouse_sequence(data: &str) -> bool {
    data.starts_with("\u{1b}[<") || (data.len() == 6 && data.starts_with("\u{1b}[M"))
}

/// Whether mouse tracking should be enabled in the current environment
/// (upstream `shouldEnableMouse`).
pub fn should_enable_mouse() -> bool {
    !(cfg!(target_os = "linux") && std::env::var("WAYLAND_DISPLAY").is_ok())
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
    /// Hit-test geometry of the primary scroll view from the last frame.
    scroll_hit: Option<ScrollHitSnapshot>,
    scrollbar_drag: Option<ScrollbarDrag>,
    scrollbar_hover_active: bool,
    mouse_sequence: String,
    selection_press_active: bool,
    on_right_click_paste: Option<RightClickPasteCallback>,
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
            scroll_hit: None,
            scrollbar_drag: None,
            scrollbar_hover_active: false,
            mouse_sequence: mouse_sequence(),
            selection_press_active: false,
            on_right_click_paste: options.on_right_click_paste,
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

    /// The implicit primary scroll view (hosts configure follow/scrollbar
    /// behaviour here).
    pub fn scroll_view_mut(&mut self) -> std::cell::RefMut<'_, ScrollView> {
        self.scroll.borrow_mut()
    }

    /// The primary scroll view's hit-test geometry from the last frame.
    pub fn scroll_hit(&self) -> Option<ScrollHitSnapshot> {
        self.scroll_hit
    }

    pub fn is_scrollbar_dragging(&self) -> bool {
        self.scrollbar_drag.is_some()
    }

    /// Handle viewport input (upstream `handleViewportInput`): focus events,
    /// wheel, mouse, scrollbar dragging/hover and the viewport keybindings.
    ///
    /// divergence: upstream registers this in its constructor; Rust cannot
    /// self-register a `&mut self` listener, so the host registers one that
    /// delegates here.
    pub fn handle_viewport_input(&mut self, data: &str) -> Option<InputListenerResult> {
        if data == FOCUS_OUT {
            let had_active_selection = self.selection_press_active;
            self.selection_press_active = false;
            self.stop_scrollbar_hover();
            self.stop_scrollbar_drag();
            if had_active_selection {
                self.base.request_render(false);
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        if data == FOCUS_IN {
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }

        if let Some(wheel) = parse_wheel_event(data) {
            if self.should_defer_viewport_input_to_overlay() {
                return None;
            }
            self.route_wheel(wheel);
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }

        if let Some(event) = parse_sgr_mouse_event(data) {
            if self.handle_right_click_paste(&event) {
                return Some(InputListenerResult {
                    consume: true,
                    data: None,
                });
            }
            let handled = self.handle_scrollbar_mouse_event(&event);
            if self.scrollbar_drag.is_none() {
                self.update_scrollbar_hover(event.x, event.y);
            }
            // Text selection handling is the next slice.
            let _ = handled;
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }

        if is_mouse_sequence(data) {
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }

        let matches = |keybinding: &str| {
            with_global_keybindings(|keybindings| keybindings.matches(data, keybinding))
        };
        let is_release = crate::tui::is_key_release(data);

        if matches("tui.altScreen.search") {
            if !is_release {
                self.open_search();
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        let search_focused = self
            .active_search
            .as_ref()
            .is_some_and(|search| search.focused);
        if search_focused {
            if matches("tui.altScreen.searchNext") {
                if !is_release {
                    self.navigate_search(1);
                }
                return Some(InputListenerResult {
                    consume: true,
                    data: None,
                });
            }
            if matches("tui.altScreen.searchPrevious") {
                if !is_release {
                    self.navigate_search(-1);
                }
                return Some(InputListenerResult {
                    consume: true,
                    data: None,
                });
            }
            if matches("tui.altScreen.searchClose") {
                if !is_release {
                    self.close_search();
                }
                return Some(InputListenerResult {
                    consume: true,
                    data: None,
                });
            }
        }

        if self.should_defer_viewport_input_to_overlay() {
            return None;
        }

        let viewport_height = self.scroll.borrow().viewport_height().max(1);
        let page = viewport_height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as i64;
        let half_page = (viewport_height / 2).max(1) as i64;

        let actions: [(&str, i64); 6] = [
            ("tui.altScreen.pageUp", -page),
            ("tui.altScreen.pageDown", page),
            ("tui.altScreen.halfPageUp", -half_page),
            ("tui.altScreen.halfPageDown", half_page),
            ("tui.altScreen.lineUp", -1),
            ("tui.altScreen.lineDown", 1),
        ];
        for (keybinding, delta) in actions {
            if matches(keybinding) {
                if !is_release {
                    self.scroll_by(delta);
                }
                return Some(InputListenerResult {
                    consume: true,
                    data: None,
                });
            }
        }
        if matches("tui.altScreen.previousPrompt") {
            if !is_release {
                self.scroll_to_prompt(-1);
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        if matches("tui.altScreen.nextPrompt") {
            if !is_release {
                self.scroll_to_prompt(1);
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        if matches("tui.altScreen.top") {
            if !is_release {
                self.scroll_to_top();
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        if matches("tui.altScreen.bottom") {
            if !is_release {
                self.scroll_to_bottom();
            }
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        None
    }

    /// Whether an overlay other than the search owns focus (upstream
    /// `shouldDeferViewportInputToOverlay`).
    fn should_defer_viewport_input_to_overlay(&self) -> bool {
        self.base.focused().is_some_and(|focused| {
            self.base.overlay_options(focused).is_some()
                && !self
                    .active_search
                    .as_ref()
                    .is_some_and(|search| search.focused && search.overlay_id == Some(focused))
        })
    }

    /// Route a wheel event to the primary scroll view (upstream `routeWheel`
    /// for the single-scroll-view case).
    pub fn route_wheel(&mut self, event: WheelEvent) {
        let remaining = event.direction * self.wheel_scroll_lines as i64;
        let leftover = self.scroll.borrow_mut().scroll_by(remaining as isize);
        let _ = leftover;
        self.update_scrollbar_hover(event.x, event.y);
        self.base.request_render(false);
    }

    /// Paste on a secondary-button press (upstream `handleRightClickPaste`;
    /// Windows only, and disabled in VS Code's terminal).
    pub fn handle_right_click_paste(&mut self, event: &SgrMouseEvent) -> bool {
        let Some(callback) = self.on_right_click_paste.as_mut() else {
            return false;
        };
        if !cfg!(target_os = "windows")
            || std::env::var("TERM_PROGRAM")
                .map(|program| program.to_lowercase() == "vscode")
                .unwrap_or(false)
            || event.release
            || event.button != 2
        {
            return false;
        }
        callback();
        true
    }

    fn scrollbar_target_at(&self, x: usize, y: usize) -> bool {
        let Some(hit) = self.scroll_hit else {
            return false;
        };
        let Some(geometry) = hit.scrollbar else {
            return false;
        };
        x == geometry.column
            && y >= geometry.thumb_top
            && y < geometry.thumb_top + geometry.thumb_height
            && x >= hit.rect.0
            && x < hit.rect.0 + hit.rect.2
            && y >= hit.rect.1
            && y < hit.rect.1 + hit.rect.3
    }

    fn set_scrollbar_hover(&mut self, hovered: bool) {
        if hovered == self.scrollbar_hover_active {
            return;
        }
        self.scrollbar_hover_active = hovered;
        self.scroll.borrow_mut().set_scrollbar_active(hovered);
    }

    fn update_scrollbar_hover(&mut self, x: usize, y: usize) {
        let hovered = self.scrollbar_target_at(x, y);
        self.set_scrollbar_hover(hovered);
    }

    fn stop_scrollbar_hover(&mut self) {
        self.set_scrollbar_hover(false);
    }

    fn stop_scrollbar_drag(&mut self) {
        self.scrollbar_drag = None;
    }

    /// Handle scrollbar dragging (upstream `handleScrollbarMouseEvent`).
    pub fn handle_scrollbar_mouse_event(&mut self, event: &SgrMouseEvent) -> bool {
        if let Some(drag) = self.scrollbar_drag {
            if event.release {
                self.stop_scrollbar_drag();
                return true;
            }
            let Some(hit) = self.scroll_hit else {
                return true;
            };
            let Some(geometry) = hit.scrollbar else {
                return true;
            };
            let max_thumb_offset = geometry.track_height.saturating_sub(geometry.thumb_height);
            let thumb_offset = event
                .y
                .saturating_sub(geometry.track_top)
                .saturating_sub(drag.grab_offset)
                .min(max_thumb_offset);
            let scroll_top = if max_thumb_offset == 0 {
                0
            } else {
                ((thumb_offset * geometry.max_scroll_top) as f64 / max_thumb_offset as f64).round()
                    as usize
            };
            self.scroll
                .borrow_mut()
                .scroll_to(scroll_top as isize, false);
            self.base.request_render(false);
            return true;
        }

        if event.release || (event.button & 32) != 0 || (event.button & 3) != 0 {
            return false;
        }
        if !self.scrollbar_target_at(event.x, event.y) {
            return false;
        }
        let Some(hit) = self.scroll_hit else {
            return false;
        };
        let Some(geometry) = hit.scrollbar else {
            return false;
        };
        self.selection_press_active = false;
        self.set_scrollbar_hover(true);
        self.scrollbar_drag = Some(ScrollbarDrag {
            grab_offset: event.y.saturating_sub(geometry.thumb_top),
        });
        true
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
            // Without the overlay component the search owns focus directly.
            focused: true,
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
        // Multiplexers lag when every pointer movement is forwarded, so the
        // enable sequence depends on the environment (upstream's check).
        let mouse = if self.mouse_enabled && should_enable_mouse() {
            self.mouse_sequence.as_str()
        } else {
            ""
        };
        let sequence =
            format!("{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse}\u{1b}[2J\u{1b}[H\u{1b}[?25l");
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
            "{BEGIN_SYNCHRONIZED_OUTPUT}{}{ENABLE_AUTOWRAP}{END_SYNCHRONIZED_OUTPUT}",
            if self.mouse_enabled {
                DISABLE_MOUSE
            } else {
                ""
            }
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

    /// Lay out the implicit scroll view over the registered children
    /// (upstream `renderLayoutFrame` + `getScrollViewBox`) and record the
    /// primary scroll view's hit-test geometry.
    fn render_frame(&mut self, width: usize, height: usize) -> Vec<String> {
        let content = self.base.render(width);
        self.last_content_lines = content.clone();

        let mut node = LayoutNode::Scroll {
            child: Box::new(LayoutNode::Leaf(Box::new(move |_width| content.clone()))),
            state: &self.scroll,
        };
        let mut frame = render_layout_frame(&mut node, width, height);
        let lines = std::mem::take(&mut frame.lines);
        let snapshot = ScrollHitSnapshot {
            rect: (
                frame.root.rect.x,
                frame.root.rect.y,
                frame.root.rect.width,
                frame.root.rect.height,
            ),
            scrollbar: get_scrollbar_geometry(&frame.root),
        };
        drop(frame);
        self.scroll_hit = Some(snapshot);
        lines
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

/// The mouse-tracking enable sequence for this environment (upstream the
/// multiplexer check in `beforeTerminalStart`).
pub fn mouse_sequence() -> String {
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let multiplexer = std::env::var("TMUX").is_ok()
        || std::env::var("ZELLIJ").is_ok()
        || std::env::var("STY").is_ok()
        || term.starts_with("tmux")
        || term.starts_with("screen");
    if multiplexer {
        ENABLE_BUTTON_MOTION_MOUSE.to_string()
    } else {
        ENABLE_ALL_MOTION_MOUSE.to_string()
    }
}
