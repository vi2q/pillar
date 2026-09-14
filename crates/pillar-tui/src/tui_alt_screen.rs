//! Port of packages/tui/src/tui-alt-screen.ts (pi v0.84.3): the
//! alternate-screen renderer's frame pipeline, viewport scrolling, search
//! state, mouse handling, and application-owned text selection with
//! copy-on-select.
//!
//! Not ported (recorded in docs/TASKS.md): the kitty image upload/cache and
//! the crash/debug logs.
//!
//! divergences:
//! - upstream stores the rendered `LayoutFrame` between frames and slices the
//!   viewport through it; the port's frame borrows its layout node, so the
//!   implicit viewport is sliced directly from the scroll state
//!   (`update_layout` + `scroll_top`/`viewport_height`) and the derived
//!   state (rendered lines, scroll positions, hit geometry) is kept in
//!   [`ScrollHitSnapshot`]. External layout roots (`setLayoutRoot`) need the
//!   component → `LayoutNode` bridge, which the port does not have yet, so
//!   only the implicit scroll view is supported.
//! - kitty image upload/caching is not ported: images render as-is and the
//!   `evictedImageDeletion` step is a no-op.
//! - the selection auto-scroll interval is host-driven: upstream sets a 50 ms
//!   `setInterval`; the port exposes [`TuiAltScreen::selection_auto_scroll_active`]
//!   and [`TuiAltScreen::auto_scroll_selection`] for the host's timer.
//! - upstream registers its input listener in the constructor; Rust cannot
//!   self-register a `&mut self` listener, so the host registers one that
//!   delegates to [`TuiAltScreen::handle_viewport_input`].
//! - clipboard writes use the injected `copy_selection` callback or a bare
//!   OSC 52 write (upstream's fallback); `utils/clipboard` lives in
//!   pillar-coding-agent, which pillar-tui must not depend on.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::alt_screen::{AltScreenDiff, classify_alt_screen_diff, rows_to_paint};
use crate::alt_screen_search::{
    AltScreenSearchComponent, AltScreenSearchMatch, find_alt_screen_search_matches,
    get_alt_screen_search_match_key,
};
use crate::edit_support::segment_words;
use crate::keybindings::with_global_keybindings;
use crate::layout::{LayoutNode, ScrollbarGeometry, get_scrollbar_geometry, render_layout_frame};
use crate::loaders::{AltScreenFlashContainer, ScrollView, ScrollViewOptions};
use crate::overlay::{
    OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue, apply_line_resets, composite_overlays,
    extract_cursor_position, prepare_overlay,
};
use crate::process_terminal::Terminal;
use crate::stack_layout::{composite_tui_line, slice_by_column};
use crate::terminal_image::is_image_line;
use crate::text_utils::{
    extract_ansi_code, get_grapheme_cell_range, get_osc8_link_at_column, strip_terminal_sequences,
    visible_width,
};
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
/// Double-click window for word/line selection (upstream
/// `DOUBLE_CLICK_INTERVAL_MS`).
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Multi-character tokens kept whole by word selection, mirroring common
/// terminal behavior for paths and kebab-case identifiers (upstream
/// `TERMINAL_WORD_SELECTION_JOINERS`).
const TERMINAL_WORD_SELECTION_JOINERS: [&str; 2] = ["/", "-"];
const OSC133_ZONE_PREFIX: &str = "\u{1b}]133;";
const OSC133_PROMPT_START: &str = "\u{1b}]133;A";

/// Styles a search match in the rendered frame.
pub type SearchMatchStyle = Box<dyn Fn(&str) -> String + Send>;
/// Opens an OSC 8 hyperlink clicked in the viewport.
pub type OpenUrlCallback = Box<dyn Fn(&str) + Send>;
/// Handles a secondary-button press for clipboard paste.
pub type RightClickPasteCallback = Box<dyn Fn() + Send>;
/// Copies selected text to the system clipboard, answering whether it
/// succeeded (upstream `copySelection`). When omitted, a bare OSC 52 write is
/// used.
pub type CopySelectionCallback = Box<dyn Fn(&str) -> bool + Send>;

/// Options for [`TuiAltScreen`] (upstream `TuiAltScreenOptions`).
#[derive(Default)]
pub struct TuiAltScreenOptions {
    /// Logical lines moved per wheel event (upstream `wheelScrollLines`).
    pub wheel_scroll_lines: Option<usize>,
    /// Capture mouse events for viewport scrolling and text selection.
    pub mouse: Option<bool>,
    /// Style a non-current search match.
    pub search_match_style: Option<SearchMatchStyle>,
    /// Style the current search match.
    pub search_current_match_style: Option<SearchMatchStyle>,
    /// Open an OSC 8 hyperlink activated with a primary click.
    pub open_url: Option<OpenUrlCallback>,
    /// Handle a secondary-button press for paste.
    pub on_right_click_paste: Option<RightClickPasteCallback>,
    /// Copy the selection on release (default true).
    pub copy_on_select: Option<bool>,
    /// Copy selected text to the system clipboard (default: OSC 52 write).
    pub copy_selection: Option<CopySelectionCallback>,
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

/// The active viewport search (upstream `ActiveSearch`).
#[derive(Debug, Clone, Default)]
pub struct ActiveSearch {
    /// The overlay hosting the search input, when one is shown.
    pub overlay_id: Option<u64>,
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

/// The granularity a selection snapshots to (upstream `SelectionGranularity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionGranularity {
    #[default]
    Character,
    Word,
    Line,
}

/// A point in the selection space (upstream `SelectionPoint`). When
/// `in_viewport` is false the coordinates are terminal cells (overlay
/// selection); otherwise they are scroll-content row/column coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: usize,
    pub col: usize,
    /// Whether this point lies in the viewport's scroll content.
    pub in_viewport: bool,
    /// Whether this point lies between terminal cells rather than on a cell.
    pub boundary: bool,
}

/// A selection range (upstream `SelectionRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
}

/// The previous click, for double/triple-click detection (upstream
/// `ClickTarget`).
#[derive(Debug, Clone, Copy)]
struct ClickTarget {
    timestamp: Instant,
    count: usize,
    row: usize,
    in_viewport: bool,
    word_start: usize,
    word_end: usize,
}

/// A search highlight span within a rendered row (upstream
/// `SearchHighlightRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchHighlightRange {
    start_col: usize,
    end_col: usize,
    current: bool,
}

/// The primary scroll view's hit-test geometry from the last frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollHitSnapshot {
    pub rect: (usize, usize, usize, usize),
    pub clip: (usize, usize, usize, usize),
    pub scrollbar: Option<ScrollbarGeometry>,
    pub scroll_top: usize,
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
    copy_selection: Option<CopySelectionCallback>,
    search_match_style: SearchMatchStyle,
    search_current_match_style: SearchMatchStyle,
    open_url: Option<OpenUrlCallback>,
    on_right_click_paste: Option<RightClickPasteCallback>,
    /// The scroll content lines of the last frame (search and selection
    /// source).
    last_content_lines: Vec<String>,
    /// Hit-test geometry of the primary scroll view from the last frame.
    scroll_hit: Option<ScrollHitSnapshot>,
    scrollbar_drag: Option<ScrollbarDrag>,
    scrollbar_hover_active: bool,
    mouse_sequence: String,
    selection_press_active: bool,
    selection_anchor: Option<SelectionPoint>,
    selection_focus: Option<SelectionPoint>,
    selection_granularity: SelectionGranularity,
    selection_initial_range: Option<SelectionRange>,
    last_click: Option<ClickTarget>,
    selection_drag_pointer: Option<(usize, usize)>,
    selection_auto_scroll_direction: i8,
    selection_auto_scroll_active: bool,
    selection_dragged: bool,
    pressed_url: Option<String>,
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
            copy_selection: options.copy_selection,
            search_match_style: options
                .search_match_style
                .unwrap_or_else(|| Box::new(|text: &str| format!("\u{1b}[4m{text}\u{1b}[24m"))),
            search_current_match_style: options.search_current_match_style.unwrap_or_else(|| {
                Box::new(|text: &str| format!("\u{1b}[1;7m{text}\u{1b}[22;27m"))
            }),
            open_url: options.open_url,
            on_right_click_paste: options.on_right_click_paste,
            last_content_lines: Vec::new(),
            scroll_hit: None,
            scrollbar_drag: None,
            scrollbar_hover_active: false,
            mouse_sequence: mouse_sequence(),
            selection_press_active: false,
            selection_anchor: None,
            selection_focus: None,
            selection_granularity: SelectionGranularity::Character,
            selection_initial_range: None,
            last_click: None,
            selection_drag_pointer: None,
            selection_auto_scroll_direction: 0,
            selection_auto_scroll_active: false,
            selection_dragged: false,
            pressed_url: None,
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

    /// Whether the viewport has a non-empty active text selection (upstream
    /// `hasActiveSelection`).
    pub fn has_active_selection(&self) -> bool {
        self.get_active_selection_text().is_some()
    }

    /// The active selection's text, if any.
    pub fn active_selection_text(&self) -> Option<String> {
        self.get_active_selection_text()
    }

    /// Copy the active selection using the configured clipboard path
    /// (upstream `copyActiveSelectionToClipboard`).
    pub fn copy_active_selection_to_clipboard(&mut self) -> bool {
        let Some(text) = self.get_active_selection_text() else {
            return false;
        };
        self.copy_text_to_clipboard(&text)
    }

    /// Whether the host should run a 50 ms auto-scroll tick (upstream the
    /// `selectionAutoScrollTimer`).
    pub fn selection_auto_scroll_active(&self) -> bool {
        self.selection_auto_scroll_active
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
            let had_non_empty_active_selection =
                had_active_selection && self.get_selection_bounds().is_some();
            self.selection_press_active = false;
            self.stop_selection_auto_scroll();
            self.stop_scrollbar_hover();
            self.stop_scrollbar_drag();
            self.pressed_url = None;
            self.selection_dragged = false;
            if had_active_selection {
                self.selection_anchor = None;
                self.selection_focus = None;
                self.selection_granularity = SelectionGranularity::Character;
                self.selection_initial_range = None;
                if had_non_empty_active_selection {
                    self.base.request_render(false);
                }
            }
            self.last_click = None;
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
            if !handled {
                self.handle_selection_mouse_event_at(&event, Instant::now());
            }
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
            .and_then(|search| search.overlay_id)
            .is_some_and(|id| self.base.overlay_is_focused(id));
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
        let Some(focused) = self.base.focused() else {
            return false;
        };
        if self.base.overlay_options(focused).is_none() {
            return false;
        }
        let search_overlay_focused = self
            .active_search
            .as_ref()
            .and_then(|search| search.overlay_id)
            .is_some_and(|id| id == focused && self.base.overlay_is_focused(id));
        !search_overlay_focused
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
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.last_click = None;
        self.pressed_url = None;
        self.selection_dragged = false;
        self.stop_selection_auto_scroll();
        self.set_scrollbar_hover(true);
        self.scrollbar_drag = Some(ScrollbarDrag {
            grab_offset: event.y.saturating_sub(geometry.thumb_top),
        });
        true
    }

    // --- text selection --------------------------------------------------

    /// The scroll view under a pointer position, if any (upstream
    /// `getScrollViewsAt(...)[0]`). With no external layout root the implicit
    /// viewport fills the screen, so only overlays can take the pointer out of
    /// it.
    fn scroll_view_at(&self, x: usize, y: usize) -> bool {
        if self.base.has_overlay() {
            return false;
        }
        self.scroll_hit.is_some_and(|hit| {
            let (rx, ry, rw, rh) = hit.rect;
            let (cx, cy, cw, ch) = hit.clip;
            x >= rx.max(cx)
                && x < (rx + rw).min(cx + cw)
                && y >= ry.max(cy)
                && y < (ry + rh).min(cy + ch)
        })
    }

    /// Map a pointer position to scroll-content coordinates (upstream
    /// `getScrollSelectionPoint`).
    fn get_scroll_selection_point(&self, x: usize, y: usize) -> Option<SelectionPoint> {
        let hit = self.scroll_hit?;
        let (rx, ry, rw, rh) = hit.rect;
        let (_, cy, _, ch) = hit.clip;
        if rh == 0 || ch == 0 {
            return None;
        }
        let visible_top = ry.max(cy);
        let visible_bottom = (self.base.terminal().rows().saturating_sub(1))
            .min((ry + rh).saturating_sub(1))
            .min((cy + ch).saturating_sub(1));
        if visible_bottom < visible_top {
            return None;
        }
        let pointer_row = y.clamp(visible_top, visible_bottom);
        let max_content_row = self.last_content_lines.len().saturating_sub(1);
        Some(SelectionPoint {
            row: hit
                .scroll_top
                .saturating_add(pointer_row.saturating_sub(ry))
                .min(max_content_row),
            col: x.saturating_sub(rx).min(rw.saturating_sub(1)),
            in_viewport: true,
            boundary: false,
        })
    }

    /// Convert a mouse event into a selection point (upstream
    /// `getSelectionPoint`).
    fn get_selection_point(&self, event: &SgrMouseEvent, in_viewport: bool) -> SelectionPoint {
        if in_viewport {
            if let Some(point) = self.get_scroll_selection_point(event.x, event.y) {
                return point;
            }
        }
        SelectionPoint {
            row: event.y.min(self.base.terminal().rows().saturating_sub(1)),
            col: event.x.min(self.base.terminal().columns().saturating_sub(1)),
            in_viewport: false,
            boundary: false,
        }
    }

    /// The source line a selection point refers to (upstream
    /// `getSelectionSourceLine`).
    fn get_selection_source_line(&self, point: SelectionPoint) -> String {
        if point.in_viewport {
            return self
                .last_content_lines
                .get(point.row)
                .cloned()
                .unwrap_or_default();
        }
        self.previous_screen
            .get(point.row)
            .cloned()
            .unwrap_or_default()
    }

    /// The word (or joiner-joined token) range containing a point (upstream
    /// `getWordSelection`).
    fn get_word_selection(&self, point: SelectionPoint) -> Option<SelectionRange> {
        let line = strip_terminal_sequences(&self.get_selection_source_line(point));
        struct Segment {
            start: usize,
            end: usize,
            selectable: bool,
            joiner: bool,
        }
        let mut segments: Vec<Segment> = Vec::new();
        let mut start = 0usize;
        for segment in segment_words(&line) {
            let end = start + visible_width(&segment.text);
            let joiner = TERMINAL_WORD_SELECTION_JOINERS.contains(&segment.text.as_str());
            segments.push(Segment {
                start,
                end,
                selectable: segment.word_like || joiner,
                joiner,
            });
            start = end;
        }
        let clicked = segments
            .iter()
            .position(|segment| point.col >= segment.start && point.col < segment.end)?;

        let can_join = |left: &Segment, right: &Segment| {
            left.selectable && right.selectable && (left.joiner || right.joiner)
        };
        let mut selection_start = segments[clicked].start;
        let mut selection_end = segments[clicked].end;
        let mut index = clicked;
        while index > 0 && can_join(&segments[index - 1], &segments[index]) {
            selection_start = segments[index - 1].start;
            index -= 1;
        }
        let mut index = clicked;
        while index < segments.len() - 1 && can_join(&segments[index], &segments[index + 1]) {
            selection_end = segments[index + 1].end;
            index += 1;
        }
        Some(SelectionRange {
            start: SelectionPoint {
                col: selection_start,
                ..point
            },
            end: SelectionPoint {
                col: selection_end,
                boundary: true,
                ..point
            },
        })
    }

    /// The whole line at a point (upstream `getLineSelection`).
    fn get_line_selection(&self, point: SelectionPoint) -> SelectionRange {
        SelectionRange {
            start: SelectionPoint { col: 0, ..point },
            end: SelectionPoint {
                col: visible_width(&self.get_selection_source_line(point)),
                boundary: true,
                ..point
            },
        }
    }

    /// Move the selection's focus, honoring a word/line granularity (upstream
    /// `updateSelectionFocus`).
    fn update_selection_focus(&mut self, point: SelectionPoint) {
        let Some(initial) = self.selection_initial_range else {
            self.selection_focus = Some(point);
            return;
        };
        if self.selection_granularity == SelectionGranularity::Character {
            self.selection_focus = Some(point);
            return;
        }
        let range = match self.selection_granularity {
            SelectionGranularity::Word => self.get_word_selection(point),
            _ => Some(self.get_line_selection(point)),
        };
        let Some(range) = range else {
            return;
        };
        let target_before_initial = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if target_before_initial {
            self.selection_anchor = Some(initial.end);
            self.selection_focus = Some(range.start);
        } else {
            self.selection_anchor = Some(initial.start);
            self.selection_focus = Some(range.end);
        }
    }

    /// Count repeated clicks on the same word (upstream `getClickCount`).
    fn get_click_count(
        &mut self,
        point: SelectionPoint,
        word: Option<SelectionRange>,
        now: Instant,
    ) -> usize {
        let previous = self.last_click;
        let count = match (word, previous) {
            (Some(word), Some(previous))
                if now.saturating_duration_since(previous.timestamp) <= DOUBLE_CLICK_INTERVAL
                    && previous.row == point.row
                    && previous.in_viewport == point.in_viewport
                    && previous.word_start == word.start.col
                    && previous.word_end == word.end.col =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        self.last_click = word.map(|word| ClickTarget {
            timestamp: now,
            count,
            row: point.row,
            in_viewport: point.in_viewport,
            word_start: word.start.col,
            word_end: word.end.col,
        });
        count
    }

    /// Track the drag pointer and arm/disarm auto-scroll (upstream
    /// `updateSelectionAutoScroll`).
    fn update_selection_auto_scroll(&mut self, event: &SgrMouseEvent) {
        if self.selection_anchor.map(|point| point.in_viewport) != Some(true) {
            self.stop_selection_auto_scroll();
            return;
        }
        let Some(hit) = self.scroll_hit else {
            self.stop_selection_auto_scroll();
            return;
        };
        let (_, ry, _, rh) = hit.rect;
        let (_, cy, _, ch) = hit.clip;
        if rh == 0 || ch == 0 {
            self.stop_selection_auto_scroll();
            return;
        }
        let visible_top = ry.max(cy);
        let visible_bottom = (self.base.terminal().rows().saturating_sub(1))
            .min((ry + rh).saturating_sub(1))
            .min((cy + ch).saturating_sub(1));
        self.selection_drag_pointer = Some((event.x, event.y));
        self.selection_auto_scroll_direction = if event.y <= visible_top {
            -1
        } else if event.y >= visible_bottom {
            1
        } else {
            0
        };
        if self.selection_auto_scroll_direction == 0 {
            self.stop_selection_auto_scroll();
            return;
        }
        // Upstream holds a 50 ms interval; the port lets the host drive it.
        self.selection_auto_scroll_active = true;
    }

    /// One auto-scroll tick (upstream the `setInterval` callback).
    pub fn auto_scroll_selection(&mut self) {
        let direction = self.selection_auto_scroll_direction;
        let Some(pointer) = self.selection_drag_pointer else {
            self.stop_selection_auto_scroll();
            return;
        };
        if direction == 0 || self.selection_anchor.map(|point| point.in_viewport) != Some(true) {
            self.stop_selection_auto_scroll();
            return;
        }
        let remaining = self.scroll.borrow_mut().scroll_by(direction as isize);
        if remaining == direction as isize {
            self.stop_selection_auto_scroll();
            return;
        }
        if let Some(point) = self.get_scroll_selection_point(pointer.0, pointer.1) {
            self.update_selection_focus(point);
        }
        self.base.request_render(false);
    }

    /// Disarm auto-scroll (upstream `stopSelectionAutoScroll`).
    pub fn stop_selection_auto_scroll(&mut self) {
        self.selection_auto_scroll_active = false;
        self.selection_auto_scroll_direction = 0;
        self.selection_drag_pointer = None;
    }

    /// Handle a mouse event for text selection (upstream
    /// `handleSelectionMouseEvent`), using the current time for click counting.
    pub fn handle_selection_mouse_event(&mut self, event: &SgrMouseEvent) {
        self.handle_selection_mouse_event_at(event, Instant::now());
    }

    /// [`Self::handle_selection_mouse_event`] with an explicit clock.
    pub fn handle_selection_mouse_event_at(&mut self, event: &SgrMouseEvent, now: Instant) {
        let button = event.button & 3;
        if button != 0 && !(event.release && button == 3) {
            return;
        }
        let anchor_in_viewport = self
            .selection_anchor
            .map(|point| point.in_viewport)
            .unwrap_or(false);
        let point = self.get_selection_point(event, anchor_in_viewport);

        if event.release {
            if !self.selection_press_active {
                return;
            }
            self.selection_press_active = false;
            self.stop_selection_auto_scroll();
            let Some(anchor) = self.selection_anchor else {
                return;
            };
            self.update_selection_focus(point);
            let clicked_url = if !self.selection_dragged
                && anchor.in_viewport == point.in_viewport
                && anchor.row == point.row
                && anchor.col == point.col
            {
                self.pressed_url.clone()
            } else {
                None
            };
            self.pressed_url = None;
            if let Some(url) = clicked_url {
                if let Some(open_url) = self.open_url.as_ref() {
                    self.selection_anchor = None;
                    self.selection_focus = None;
                    // URL activation is best-effort.
                    open_url(&url);
                    self.base.request_render(false);
                    return;
                }
            }
            if self.copy_on_select {
                self.copy_selection_to_clipboard();
            }
            self.base.request_render(false);
            return;
        }

        if (event.button & 32) != 0 {
            if !self.selection_press_active || self.selection_anchor.is_none() {
                return;
            }
            self.selection_dragged = true;
            self.last_click = None;
            self.pressed_url = None;
            self.update_selection_focus(point);
            self.update_selection_auto_scroll(event);
            self.base.request_render(false);
            return;
        }

        self.stop_selection_auto_scroll();
        self.selection_press_active = true;
        let in_viewport = self.scroll_view_at(event.x, event.y);
        let anchor = self.get_selection_point(event, in_viewport);
        let word = self.get_word_selection(anchor);
        let click_count = self.get_click_count(anchor, word, now);
        let range = if click_count == 2 {
            word
        } else if click_count == 3 {
            Some(self.get_line_selection(anchor))
        } else {
            None
        };
        self.selection_granularity = match (range, click_count) {
            (Some(_), 2) => SelectionGranularity::Word,
            (Some(_), _) => SelectionGranularity::Line,
            (None, _) => SelectionGranularity::Character,
        };
        self.selection_initial_range = range;
        self.selection_anchor = Some(range.map(|range| range.start).unwrap_or(anchor));
        self.selection_focus = Some(range.map(|range| range.end).unwrap_or(anchor));
        self.selection_dragged = false;
        self.pressed_url = if range.is_some() {
            None
        } else {
            let row = event.y.min(self.base.terminal().rows().saturating_sub(1));
            let col = event.x.min(self.base.terminal().columns().saturating_sub(1));
            get_osc8_link_at_column(self.previous_screen.get(row).map(String::as_str).unwrap_or(""), col)
        };
        self.base.request_render(false);
    }

    /// The ordered selection bounds, if the selection is non-empty (upstream
    /// `getSelectionBounds`).
    fn get_selection_bounds(&self) -> Option<SelectionRange> {
        let anchor = self.selection_anchor?;
        let focus = self.selection_focus?;
        if anchor.in_viewport != focus.in_viewport {
            return None;
        }
        if anchor.row == focus.row && anchor.col == focus.col {
            return None;
        }
        let anchor_before_focus = anchor.row < focus.row
            || (anchor.row == focus.row && anchor.col < focus.col);
        Some(if anchor_before_focus {
            SelectionRange {
                start: anchor,
                end: focus,
            }
        } else {
            SelectionRange {
                start: focus,
                end: anchor,
            }
        })
    }

    /// The selected columns within one line (upstream `getSelectionColumns`).
    fn get_selection_columns(
        &self,
        line: &str,
        row: usize,
        selection: &SelectionRange,
        min_column: usize,
        max_column: usize,
    ) -> (usize, usize) {
        let line_width = visible_width(line);
        let mut start = min_column;
        let mut end = line_width.min(max_column);
        if row == selection.start.row {
            start = get_grapheme_cell_range(line, selection.start.col)
                .map(|range| range.0)
                .unwrap_or_else(|| selection.start.col.min(line_width));
        }
        if row == selection.end.row {
            end = if selection.end.boundary {
                selection.end.col.min(line_width)
            } else {
                get_grapheme_cell_range(line, selection.end.col)
                    .map(|range| range.1)
                    .unwrap_or_else(|| (selection.end.col + 1).min(line_width))
            };
        }
        (start.max(min_column), end.min(max_column))
    }

    /// The active selection's text (upstream `getActiveSelectionText`).
    fn get_active_selection_text(&self) -> Option<String> {
        let selection = self.get_selection_bounds()?;
        let source_lines: Vec<String> = if selection.start.in_viewport {
            self.last_content_lines.clone()
        } else {
            self.previous_screen.clone()
        };
        let mut lines: Vec<String> = Vec::new();
        for row in selection.start.row..=selection.end.row {
            let line = source_lines.get(row).cloned().unwrap_or_default();
            let (start, end) =
                self.get_selection_columns(&line, row, &selection, 0, visible_width(&line));
            lines.push(
                strip_terminal_sequences(&slice_by_column(
                    &line,
                    start,
                    end.saturating_sub(start),
                    true,
                ))
                .trim_end()
                .to_string(),
            );
        }
        let text = lines.join("\n");
        if text.is_empty() { None } else { Some(text) }
    }

    fn copy_selection_to_clipboard(&mut self) -> bool {
        let Some(text) = self.get_active_selection_text() else {
            return false;
        };
        self.copy_text_to_clipboard(&text)
    }

    /// Copy text through the injected clipboard callback, or a bare OSC 52
    /// write (upstream `copyTextToClipboard`). A bare OSC 52 write can report
    /// success while leaving the clipboard untouched, so only the injected path
    /// is verified.
    fn copy_text_to_clipboard(&mut self, text: &str) -> bool {
        if let Some(callback) = self.copy_selection.as_ref() {
            let ok = callback(text);
            self.flash(if ok { "Copied!" } else { "Copy failed" }, None);
            return ok;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        self.base
            .terminal_mut()
            .write(&format!("\u{1b}]52;c;{encoded}\u{7}"));
        self.flash("Copied!", None);
        true
    }

    // --- highlight compositing -------------------------------------------

    /// Wrap the non-ANSI runs of a line in the search match style (upstream
    /// `applySearchTextHighlight`).
    fn apply_search_text_highlight(&self, text: &str, current: bool) -> String {
        let style: &SearchMatchStyle = if current {
            &self.search_current_match_style
        } else {
            &self.search_match_style
        };
        let chars: Vec<char> = text.chars().collect();
        let mut result = String::new();
        let mut plain_start = 0usize;
        let mut index = 0usize;
        while index < chars.len() {
            let Some(ansi) = extract_ansi_code(&chars, index) else {
                index += 1;
                continue;
            };
            if index > plain_start {
                result.push_str(&style(&chars[plain_start..index].iter().collect::<String>()));
            }
            result.push_str(&ansi.code);
            index += ansi.length;
            plain_start = index;
        }
        if plain_start < chars.len() {
            result.push_str(&style(&chars[plain_start..].iter().collect::<String>()));
        }
        result
    }

    /// Highlight the visible search matches in a frame (upstream
    /// `applySearchHighlights`).
    fn apply_search_highlights(&self, screen: Vec<String>, width: usize) -> Vec<String> {
        let Some(search) = self.active_search.as_ref() else {
            return screen;
        };
        let Some(selected_index) = search.selected_index else {
            return screen;
        };
        if search.matches.is_empty() {
            return screen;
        }
        let Some(hit) = self.scroll_hit else {
            return screen;
        };
        let (rx, ry, rw, rh) = hit.rect;
        let (cx, cy, cw, ch) = hit.clip;
        let scrollbar_column = hit.scrollbar.map(|geometry| geometry.column);
        let min_row = ry.max(cy);
        let max_row = screen.len().min(ry + rh).min(cy + ch);
        let min_column = rx.max(cx);
        let max_column = width
            .min(rx + rw)
            .min(cx + cw)
            .min(scrollbar_column.unwrap_or(usize::MAX));

        let mut ranges_by_row: Vec<(usize, Vec<SearchHighlightRange>)> = Vec::new();
        for (match_index, matched) in search.matches.iter().enumerate() {
            for segment in &matched.segments {
                let row = (ry + segment.row).saturating_sub(hit.scroll_top);
                if row < min_row || row >= max_row {
                    continue;
                }
                let start_col = min_column.max(rx + segment.start_col);
                let end_col = max_column.min(rx + segment.end_col);
                if end_col <= start_col {
                    continue;
                }
                let entry = match ranges_by_row.iter_mut().find(|(row_, _)| *row_ == row) {
                    Some(entry) => entry,
                    None => {
                        ranges_by_row.push((row, Vec::new()));
                        ranges_by_row
                            .last_mut()
                            .expect("just pushed the row entry")
                    }
                };
                entry.1.push(SearchHighlightRange {
                    start_col,
                    end_col,
                    current: match_index == selected_index,
                });
            }
        }

        let mut result = screen;
        for (row, mut ranges) in ranges_by_row {
            let Some(current) = result.get(row).cloned() else {
                continue;
            };
            if is_image_line(&current) {
                continue;
            }
            let line_width = visible_width(&current);
            let mut line = current;
            ranges.sort_by(|a, b| b.start_col.cmp(&a.start_col));
            for range in ranges {
                let start_col = range.start_col.min(line_width);
                let end_col = range.end_col.min(line_width);
                if end_col <= start_col {
                    continue;
                }
                let before = slice_by_column(&line, 0, start_col, true);
                let highlighted = slice_by_column(&line, start_col, end_col - start_col, true);
                let after = slice_by_column(&line, end_col, line_width.saturating_sub(end_col), true);
                line = format!(
                    "{before}{}{after}",
                    self.apply_search_text_highlight(&highlighted, range.current)
                );
            }
            if let Some(slot) = result.get_mut(row) {
                *slot = line;
            }
        }
        result
    }

    /// Inverse-video a selected slice, re-asserting the attribute after every
    /// SGR reset (upstream `applySelectionHighlight`).
    fn apply_selection_highlight(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut result = String::from("\u{1b}[7m");
        let mut index = 0usize;
        while index < chars.len() {
            let Some(ansi) = extract_ansi_code(&chars, index) else {
                result.push(chars[index]);
                index += 1;
                continue;
            };
            result.push_str(&ansi.code);
            if ansi.code.ends_with('m') {
                result.push_str("\u{1b}[7m");
            }
            index += ansi.length;
        }
        format!("{result}\u{1b}[27m")
    }

    /// Apply the active selection to a frame (upstream `applySelection`).
    fn apply_selection(&self, screen: Vec<String>) -> Vec<String> {
        let Some(selection) = self.get_selection_bounds() else {
            return screen;
        };
        let mut min_row = 0usize;
        let mut max_row = screen.len().saturating_sub(1);
        let mut min_column = 0usize;
        let mut max_column = self.base.terminal().columns();
        let mut screen_selection = selection;
        if selection.start.in_viewport {
            let Some(hit) = self.scroll_hit else {
                return screen;
            };
            let (rx, ry, rw, rh) = hit.rect;
            let (cx, cy, cw, ch) = hit.clip;
            min_row = ry.max(cy);
            max_row = screen
                .len()
                .saturating_sub(1)
                .min((ry + rh).saturating_sub(1))
                .min((cy + ch).saturating_sub(1));
            min_column = rx.max(cx);
            max_column = self.base.terminal().columns().min(rx + rw).min(cx + cw);
            let translate = |point: SelectionPoint| SelectionPoint {
                row: (ry + point.row).saturating_sub(hit.scroll_top),
                col: rx + point.col,
                ..point
            };
            screen_selection = SelectionRange {
                start: translate(selection.start),
                end: translate(selection.end),
            };
        }
        screen
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                if row < min_row
                    || row > max_row
                    || row < screen_selection.start.row
                    || row > screen_selection.end.row
                    || is_image_line(&line)
                {
                    return line;
                }
                let line_width = visible_width(&line);
                let columns = self.get_selection_columns(
                    &line,
                    row,
                    &screen_selection,
                    min_column,
                    max_column,
                );
                if columns.1 <= columns.0 {
                    return line;
                }
                let before = slice_by_column(&line, 0, columns.0, true);
                let selected = slice_by_column(&line, columns.0, columns.1 - columns.0, true);
                let after = slice_by_column(
                    &line,
                    columns.1,
                    line_width.saturating_sub(columns.1),
                    true,
                );
                format!("{before}{}{after}", Self::apply_selection_highlight(&selected))
            })
            .collect()
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

    /// The search overlay's typed component, when one is shown. The component
    /// belongs to the overlay stack, so it is reached by downcast (upstream
    /// holds a direct reference).
    pub fn search_component_mut(&mut self) -> Option<&mut AltScreenSearchComponent> {
        let id = self.active_search.as_ref()?.overlay_id?;
        self.base
            .overlay_component_mut(id)?
            .as_any_mut()?
            .downcast_mut::<AltScreenSearchComponent>()
    }

    /// Pick up query keystrokes the overlay's input consumed (upstream the
    /// component's `onQueryChange` callback). The port syncs before each frame
    /// because the callback cannot borrow the alt-screen.
    fn sync_search_query(&mut self) {
        let Some(query) = self.search_component_mut().map(|c| c.query().to_string()) else {
            return;
        };
        self.update_search_query(&query);
    }

    /// Open the viewport search (upstream `openSearch`).
    pub fn open_search(&mut self) {
        if self.active_search.is_some() {
            if let Some(id) = self.active_search.as_ref().and_then(|search| search.overlay_id) {
                self.base.focus_overlay(id);
            }
            return;
        }
        let anchor_row = self.viewport_top();
        let id = self.base.show_overlay(
            Box::new(AltScreenSearchComponent::new()),
            OverlayOptions {
                anchor: Some(OverlayAnchor::TopRight),
                width: Some(SizeValue::Percent(400)),
                min_width: Some(24),
                margin: Some(OverlayMargin {
                    top: 1,
                    right: 1,
                    bottom: 1,
                    left: 1,
                }),
                ..Default::default()
            },
        );
        self.active_search = Some(ActiveSearch {
            overlay_id: Some(id),
            anchor_row,
            ..Default::default()
        });
    }

    pub fn close_search(&mut self) {
        let Some(search) = self.active_search.take() else {
            return;
        };
        if let Some(id) = search.overlay_id {
            self.base.remove_overlay(id);
        }
        self.base.request_render(false);
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
        if let Some(component) = self.search_component_mut() {
            component.set_result(-1, 0);
        }
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
            if let Some(component) = self.search_component_mut() {
                component.set_result(-1, 0);
            }
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
        let result_index = selected_index.map(|index| index as i64).unwrap_or(-1);
        let result_count = search.matches.len();
        if let Some(component) = self.search_component_mut() {
            component.set_result(result_index, result_count);
        }
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
        // Upstream `beforeTerminalStart` drops all transient state.
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.flashes.dispose();
        self.alt_screen_active = true;
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.last_click = None;
        self.pressed_url = None;
        self.selection_dragged = false;
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
        // Upstream `beforeTerminalStop` drops all transient state.
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.flashes.dispose();
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

        // Pick up query keystrokes the search overlay consumed.
        self.sync_search_query();

        let mut screen = self.render_frame(width, height);
        if self.refresh_search() {
            screen = self.render_frame(width, height);
        }
        // Zone prefixes are stripped before compositing.
        screen = screen
            .iter()
            .map(|line| line.replace(OSC133_ZONE_PREFIX, ""))
            .collect();
        screen = self.apply_search_highlights(screen, width);
        screen = self.compose_overlays(screen, width, height);
        if screen.len() > height {
            let excess = screen.len() - height;
            screen.drain(..excess);
        }
        screen = self.apply_selection(screen);
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
            clip: (
                frame.root.clip.x,
                frame.root.clip.y,
                frame.root.clip.width,
                frame.root.clip.height,
            ),
            scrollbar: get_scrollbar_geometry(&frame.root),
            scroll_top: self.scroll.borrow().scroll_top(),
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
            // The overlay renders at its resolved width, not the terminal
            // width (upstream `resolveOverlayLayout(options, 0, ...)`).
            let overlay_width =
                crate::overlay::resolve_overlay_layout(Some(&options), 0, width, height).width;
            let Some(component) = self.base.overlay_component_mut(id) else {
                continue;
            };
            let overlay_lines = component.render(overlay_width);
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
        let flash_lines = self.flashes.render(width);
        // Upstream keeps the last `height` entries and composites them
        // right-aligned into the top rows.
        let flash_lines: Vec<String> = if flash_lines.len() > height {
            flash_lines[flash_lines.len() - height..].to_vec()
        } else {
            flash_lines
        };
        if flash_lines.is_empty() {
            return lines;
        }
        let mut result = lines;
        while result.len() < height {
            result.push(String::new());
        }
        for (row, line) in flash_lines.iter().enumerate() {
            let flash_width = visible_width(line);
            if flash_width == 0 {
                continue;
            }
            let Some(current) = result.get(row).cloned() else {
                continue;
            };
            let composited = composite_tui_line(
                &current,
                line,
                width.saturating_sub(flash_width),
                flash_width,
                width,
            );
            if let Some(slot) = result.get_mut(row) {
                *slot = composited;
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
