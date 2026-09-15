//! Port of packages/tui/src/tui.ts (pi v0.84.3): the [`Component`] trait,
//! [`Container`], input plumbing, the overlay/TUI option types, and
//! [`TuiBase`] — everything the screen renderers (`tui-main-screen.ts` /
//! `tui-alt-screen.ts`) build on.
//!
//! The overlay stack + focus routing live in
//! [`crate::overlay_focus::OverlayFocusMachine`] (upstream the private fields
//! of `TuiBase`), and overlay geometry/compositing in [`crate::overlay`];
//! both are re-exported here so this module matches upstream's surface.
//!
//! divergences:
//! - upstream components are JS classes with an optional `handleInput` and a
//!   structural `isFocusable` check; the port uses object-safe traits with
//!   [`Component::as_focusable`] as the type guard, and components are
//!   referenced by [`ComponentId`].
//! - render scheduling is host-driven: [`TuiBase::request_render`] records a
//!   deadline and the host calls [`TuiBase::pump`]; upstream schedules
//!   `process.nextTick`/`setTimeout` itself.
//! - the terminal queries (`queryTerminalBackgroundColor` /
//!   `queryTerminalColorScheme`) pump input synchronously until the reply or
//!   timeout instead of returning a promise.
//! - an overlay's `visible` predicate is evaluated when it is shown (upstream
//!   re-evaluates it every render).

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub use crate::input::CURSOR_MARKER;
pub use crate::overlay::{
    OverlayAnchor, OverlayMargin, OverlayOptions, ResolvedOverlayLayout, SizeValue,
    apply_line_resets, composite_overlays, extract_cursor_position, prepare_overlay,
    resolve_overlay_layout,
};
pub use crate::overlay_focus::OverlayFocusMachine;
pub use crate::process_terminal::Terminal;
pub use crate::stack_layout::composite_tui_line;

use crate::terminal_colors::{
    RgbColor, TerminalColorScheme, is_osc11_background_color_response,
    parse_osc11_background_color, parse_terminal_color_scheme_report,
};
use crate::terminal_image::{CellDimensions, set_cell_dimensions};

/// Minimum interval between throttled frames (upstream
/// `TuiBase.MIN_RENDER_INTERVAL_MS`).
pub const MIN_RENDER_INTERVAL_MS: u64 = 16;

/// Identifies a component registered with the TUI.
pub type ComponentId = u64;
/// Identifies a registered input listener.
pub type ListenerId = u64;

/// The result of an input listener (upstream `TuiInputListenerResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputListenerResult {
    /// Stop dispatching to later listeners and the focused component.
    pub consume: bool,
    /// Replaces the data passed on (upstream `data`).
    pub data: Option<String>,
}

/// An input listener (upstream `TuiInputListener`).
pub type TuiInputListener = Box<dyn FnMut(&str) -> Option<InputListenerResult> + Send>;

/// A terminal colour-scheme listener (upstream
/// `onTerminalColorSchemeChange`).
pub type TerminalColorSchemeListener = Box<dyn FnMut(TerminalColorScheme) + Send>;

/// A renderable UI component (upstream `Component`).
pub trait Component: Send {
    /// Render the component to lines for the given viewport width.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Handle keyboard input while focused (upstream `handleInput`).
    fn handle_input(&mut self, _data: &str) {}

    /// Whether key-release events should be delivered (upstream
    /// `wantsKeyRelease`).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Drop cached rendering state (upstream `invalidate`).
    fn invalidate(&mut self) {}

    /// The focus interface when the component can take focus (upstream the
    /// `isFocusable` type guard).
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        None
    }

    /// Downcast hook for hosts that own a concrete component type (the port
    /// reaches the alt-screen search overlay's query state through it).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// A component that can receive focus and show the hardware cursor (upstream
/// `Focusable`).
pub trait Focusable {
    /// Set by the TUI when focus changes.
    fn set_focused(&mut self, focused: bool);
    /// Whether this component currently has focus (upstream reads the public
    /// `focused` field).
    fn is_focused(&self) -> bool;
}

/// Whether a component can take focus (upstream `isFocusable`).
pub fn is_focusable(component: Option<&mut dyn Component>) -> bool {
    component.is_some_and(|component| component.as_focusable().is_some())
}

/// A container of components (upstream `Container`).
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    /// Remove the child at `index` (upstream `removeChild` by identity).
    pub fn remove_child(&mut self, index: usize) -> Option<Box<dyn Component>> {
        if index >= self.children.len() {
            return None;
        }
        Some(self.children.remove(index))
    }

    /// Insert a child at `index` (upstream `children.splice(index, 0, x)`).
    pub fn insert_child(&mut self, index: usize, component: Box<dyn Component>) {
        let index = index.min(self.children.len());
        self.children.insert(index, component);
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// Mutable children access (upstream code indexes `children[i]`)
    /// for identity checks and index-based splices.
    pub fn children_mut(&mut self) -> &mut [Box<dyn Component>] {
        &mut self.children
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn children(&self) -> &[Box<dyn Component>] {
        &self.children
    }
}

impl Component for Container {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &mut self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        for child in &mut self.children {
            child.handle_input(data);
        }
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

/// How the TUI drives the terminal (upstream `TuiMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TuiMode {
    #[default]
    Regular,
    Fullscreen,
}

impl TuiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TuiMode::Regular => "regular",
            TuiMode::Fullscreen => "fullscreen",
        }
    }
}

/// Options for stopping the TUI (upstream `TuiStopOptions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over the same
    /// terminal.
    pub preserve_screen: bool,
}

/// Options for releasing overlay focus (upstream `OverlayUnfocusOptions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayUnfocusOptions {
    /// Replaces the previous focus target when set (upstream `target`).
    pub target: Option<ComponentId>,
}

/// Whether an input sequence is a Kitty key-release event (upstream
/// `isKeyRelease`).
pub fn is_key_release(data: &str) -> bool {
    // Bracketed paste content is never a release, even when it contains a
    // `:3F`-like pattern (e.g. a bluetooth MAC address).
    if data.contains("\u{1b}[200~") {
        return false;
    }
    [":3u", ":3~", ":3A", ":3B", ":3C", ":3D", ":3H", ":3F"]
        .iter()
        .any(|suffix| data.contains(suffix))
}

/// A pending OSC 11 background query.
struct PendingBackgroundQuery {
    pending_replies: usize,
}

/// The TUI core (upstream `TuiBase`): component registry, focus, the overlay
/// stack, input dispatch, render scheduling, and terminal queries. Screen
/// renderers wrap it and implement the actual frame drawing.
pub struct TuiBase {
    terminal: Box<dyn Terminal>,
    mode: TuiMode,
    next_id: ComponentId,
    roots: Vec<(ComponentId, Box<dyn Component>)>,
    overlay_components: HashMap<ComponentId, Box<dyn Component>>,
    /// Full overlay options, keyed like [`Self::overlay_components`].
    overlay_options: HashMap<ComponentId, OverlayOptions>,
    focus_machine: OverlayFocusMachine,
    focused: Option<ComponentId>,
    input_listeners: Vec<(ListenerId, TuiInputListener)>,
    next_listener_id: ListenerId,
    color_scheme_listeners: Vec<(ListenerId, TerminalColorSchemeListener)>,
    next_color_listener_id: ListenerId,
    color_scheme_notifications: bool,
    pending_background: PendingBackgroundQuery,
    /// The most recent OSC 11 reply (upstream resolves the pending query).
    background_reply: Option<RgbColor>,
    render_pending: bool,
    next_render_at: Option<Instant>,
    last_render_at: Option<Instant>,
    stopped: bool,
    full_redraws: u64,
    show_hardware_cursor: bool,
    clear_on_shrink: bool,
    on_debug: Option<Box<dyn FnMut() + Send>>,
}

impl TuiBase {
    pub fn new(terminal: Box<dyn Terminal>, mode: TuiMode) -> Self {
        Self {
            terminal,
            mode,
            next_id: 1,
            roots: Vec::new(),
            overlay_components: HashMap::new(),
            overlay_options: HashMap::new(),
            focus_machine: OverlayFocusMachine::new(),
            focused: None,
            input_listeners: Vec::new(),
            next_listener_id: 1,
            color_scheme_listeners: Vec::new(),
            next_color_listener_id: 1,
            color_scheme_notifications: false,
            pending_background: PendingBackgroundQuery { pending_replies: 0 },
            background_reply: None,
            render_pending: false,
            next_render_at: None,
            last_render_at: None,
            stopped: true,
            full_redraws: 0,
            show_hardware_cursor: std::env::var("PILLAR_HARDWARE_CURSOR").ok().as_deref()
                == Some("1"),
            clear_on_shrink: std::env::var("PILLAR_CLEAR_ON_SHRINK").ok().as_deref() == Some("1"),
            on_debug: None,
        }
    }

    pub fn mode(&self) -> TuiMode {
        self.mode
    }

    pub fn terminal(&self) -> &dyn Terminal {
        &*self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut dyn Terminal {
        &mut *self.terminal
    }

    pub fn full_redraws(&self) -> u64 {
        self.full_redraws
    }

    pub fn note_full_redraw(&mut self) {
        self.full_redraws += 1;
    }

    pub fn show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
    }

    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.show_hardware_cursor = enabled;
    }

    pub fn clear_on_shrink(&self) -> bool {
        self.clear_on_shrink
    }

    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.clear_on_shrink = enabled;
    }

    pub fn set_on_debug(&mut self, callback: Box<dyn FnMut() + Send>) {
        self.on_debug = Some(callback);
    }

    pub fn has_overlay_entries(&self) -> bool {
        self.focus_machine.has_overlay()
    }

    // --- roots -----------------------------------------------------------

    pub fn add_child(&mut self, component: Box<dyn Component>) -> ComponentId {
        let id = self.next_id;
        self.next_id += 1;
        self.roots.push((id, component));
        id
    }

    pub fn remove_child(&mut self, id: ComponentId) -> Option<Box<dyn Component>> {
        let index = self
            .roots
            .iter()
            .position(|(candidate, _)| *candidate == id)?;
        Some(self.roots.remove(index).1)
    }

    /// Replace a root child in place, keeping its id and render position
    /// (the port's stand-in for upstream's stable `editorContainer`, which
    /// swaps the editor for a selector without moving the slot).
    pub fn replace_child(&mut self, id: ComponentId, component: Box<dyn Component>) -> bool {
        let Some(index) = self
            .roots
            .iter()
            .position(|(candidate, _)| *candidate == id)
        else {
            return false;
        };
        self.roots[index].1 = component;
        true
    }

    pub fn clear(&mut self) {
        self.roots.clear();
    }

    /// The mounted root component ids, in render order (upstream
    /// `getMountedRoots`).
    pub fn root_ids(&self) -> Vec<ComponentId> {
        self.roots.iter().map(|(id, _)| *id).collect()
    }

    pub fn child_count(&self) -> usize {
        self.roots.len()
    }

    pub fn invalidate(&mut self) {
        for (_, component) in &mut self.roots {
            component.invalidate();
        }
        for component in self.overlay_components.values_mut() {
            component.invalidate();
        }
    }

    // --- focus -----------------------------------------------------------

    pub fn focused(&self) -> Option<ComponentId> {
        self.focused
    }

    /// Focus a root component or an overlay (`None` clears focus).
    pub fn set_focus(&mut self, id: Option<ComponentId>) {
        self.focus_machine.set_focus(id);
        self.sync_focus_from_machine();
    }

    /// Mirror the focus machine's current focus onto the components.
    fn sync_focus_from_machine(&mut self) {
        let focus = self.focus_machine.route_input_focus();
        self.apply_focus(focus);
    }

    fn apply_focus(&mut self, id: Option<ComponentId>) {
        if self.focused == id {
            return;
        }
        if let Some(previous) = self.focused {
            if let Some(component) = self.component_mut(previous) {
                if let Some(focusable) = component.as_focusable() {
                    focusable.set_focused(false);
                }
            }
        }
        self.focused = id;
        if let Some(next) = id {
            if let Some(component) = self.component_mut(next) {
                if let Some(focusable) = component.as_focusable() {
                    focusable.set_focused(true);
                }
            }
        }
    }

    fn component_mut(&mut self, id: ComponentId) -> Option<&mut Box<dyn Component>> {
        if let Some(index) = self
            .roots
            .iter()
            .position(|(candidate, _)| *candidate == id)
        {
            return Some(&mut self.roots[index].1);
        }
        self.overlay_components.get_mut(&id)
    }

    // --- overlays --------------------------------------------------------

    /// Show an overlay (upstream `showOverlay`), returning its id.
    pub fn show_overlay(
        &mut self,
        component: Box<dyn Component>,
        options: OverlayOptions,
    ) -> ComponentId {
        let id = self.next_id;
        self.next_id += 1;
        self.overlay_components.insert(id, component);
        let entry_options = crate::overlay_focus::OverlayEntryOptions {
            non_capturing: options.non_capturing,
            visible: options.is_visible(self.terminal.columns(), self.terminal.rows()),
        };
        self.overlay_options.insert(id, options);
        self.focus_machine.show_overlay(id, entry_options);
        self.sync_focus_from_machine();
        self.request_render(false);
        id
    }

    /// Permanently remove an overlay (the handle's `hide`).
    pub fn remove_overlay(&mut self, id: ComponentId) {
        self.focus_machine.remove_overlay(id);
        self.overlay_components.remove(&id);
        self.overlay_options.remove(&id);
        self.sync_focus_from_machine();
        self.request_render(false);
    }

    /// Hide the topmost overlay (upstream `hideOverlay`).
    pub fn hide_overlay(&mut self) {
        if let Some(id) = self.topmost_overlay() {
            self.remove_overlay(id);
        }
    }

    pub fn has_overlay(&self) -> bool {
        self.focus_machine.has_overlay()
    }

    /// Temporarily hide or show an overlay (the handle's `setHidden`).
    pub fn set_overlay_hidden(&mut self, id: ComponentId, hidden: bool) {
        self.focus_machine.set_hidden(id, hidden);
        self.sync_focus_from_machine();
        self.request_render(false);
    }

    /// Focus an overlay and bring it to the front (the handle's `focus`).
    pub fn focus_overlay(&mut self, id: ComponentId) {
        self.focus_machine.focus_overlay(id);
        self.sync_focus_from_machine();
        self.request_render(false);
    }

    /// Release overlay focus (the handle's `unfocus`).
    pub fn unfocus_overlay(&mut self, id: ComponentId, _options: OverlayUnfocusOptions) {
        self.focus_machine.unfocus_overlay(id);
        self.sync_focus_from_machine();
        self.request_render(false);
    }

    pub fn overlay_is_focused(&self, id: ComponentId) -> bool {
        self.focused == Some(id)
    }

    /// The topmost overlay in the focus stack (upstream
    /// `overlayStack[overlayStack.length - 1]`).
    fn topmost_overlay(&self) -> Option<ComponentId> {
        self.focus_machine.topmost_overlay_id()
    }

    /// Overlay ids in stack order (render input).
    pub fn overlay_ids(&self) -> Vec<ComponentId> {
        let mut ids: Vec<ComponentId> = self.overlay_components.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The overlay component for `id`.
    pub fn overlay_component_mut(&mut self, id: ComponentId) -> Option<&mut Box<dyn Component>> {
        self.overlay_components.get_mut(&id)
    }

    /// The overlay options for `id`.
    pub fn overlay_options(&self, id: ComponentId) -> Option<&OverlayOptions> {
        self.overlay_options.get(&id)
    }

    // --- input -----------------------------------------------------------

    pub fn add_input_listener(&mut self, listener: TuiInputListener) -> ListenerId {
        let id = self.next_listener_id;
        self.next_listener_id += 1;
        self.input_listeners.push((id, listener));
        id
    }

    pub fn remove_input_listener(&mut self, id: ListenerId) {
        self.input_listeners
            .retain(|(candidate, _)| *candidate != id);
    }

    /// Dispatch one input sequence (upstream `handleTerminalInput`).
    pub fn handle_terminal_input(&mut self, data: &str) {
        if self.consume_osc11_background_response(data) {
            return;
        }
        if self.consume_terminal_color_scheme_report(data) {
            return;
        }
        if self.consume_cell_size_response(data) {
            return;
        }

        let mut current = data.to_string();
        for (_, listener) in &mut self.input_listeners {
            let Some(result) = listener(&current) else {
                continue;
            };
            if let Some(data) = result.data {
                current = data;
            }
            if result.consume {
                return;
            }
        }

        let Some(focused) = self.focused else {
            return;
        };
        let Some(component) = self.component_mut(focused) else {
            return;
        };
        if is_key_release(&current) && !component.wants_key_release() {
            return;
        }
        component.handle_input(&current);
        // Keyboard input is latency sensitive: render on the next pump
        // without the throttle delay.
        self.request_render(true);
    }

    fn consume_osc11_background_response(&mut self, data: &str) -> bool {
        if self.pending_background.pending_replies == 0 {
            return false;
        }
        if !is_osc11_background_color_response(data) {
            return false;
        }
        self.pending_background.pending_replies -= 1;
        self.background_reply = parse_osc11_background_color(data);
        true
    }

    fn consume_terminal_color_scheme_report(&mut self, data: &str) -> bool {
        let Some(scheme) = parse_terminal_color_scheme_report(data) else {
            return false;
        };
        for (_, listener) in &mut self.color_scheme_listeners {
            listener(scheme);
        }
        true
    }

    fn consume_cell_size_response(&mut self, data: &str) -> bool {
        // ESC [ 6 ; height ; width t
        let Some(rest) = data.strip_prefix("\u{1b}[6;") else {
            return false;
        };
        let Some(rest) = rest.strip_suffix('t') else {
            return false;
        };
        let mut parts = rest.split(';');
        let (Some(height), Some(width)) = (parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(height), Ok(width)) = (height.parse::<usize>(), width.parse::<usize>()) else {
            return false;
        };
        if height == 0 || width == 0 {
            return true;
        }
        set_cell_dimensions(CellDimensions {
            width_px: width as u32,
            height_px: height as u32,
        });
        self.invalidate();
        self.request_render(false);
        true
    }

    // --- colour scheme ---------------------------------------------------

    pub fn on_terminal_color_scheme_change(
        &mut self,
        listener: TerminalColorSchemeListener,
    ) -> ListenerId {
        let id = self.next_color_listener_id;
        self.next_color_listener_id += 1;
        self.color_scheme_listeners.push((id, listener));
        id
    }

    pub fn remove_terminal_color_scheme_listener(&mut self, id: ListenerId) {
        self.color_scheme_listeners
            .retain(|(candidate, _)| *candidate != id);
    }

    pub fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
        if self.color_scheme_notifications == enabled {
            return;
        }
        self.color_scheme_notifications = enabled;
        if !self.stopped {
            self.terminal.write(if enabled {
                "\u{1b}[?2031h"
            } else {
                "\u{1b}[?2031l"
            });
        }
    }

    /// Query the terminal background colour with OSC 11, pumping input until
    /// the reply or `timeout` (upstream `queryTerminalBackgroundColor`).
    pub fn query_terminal_background_color(&mut self, timeout: Duration) -> Option<RgbColor> {
        self.background_reply = None;
        self.pending_background.pending_replies += 1;
        self.terminal.write("\u{1b}]11;?\u{7}");

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(data) = self
                .terminal
                .read_input(remaining.min(Duration::from_millis(20)))
            else {
                continue;
            };
            for sequence in self.feed_and_split(&data) {
                self.handle_terminal_input(&sequence);
            }
            if self.pending_background.pending_replies == 0 {
                return self.background_reply.take();
            }
        }
        self.pending_background.pending_replies = 0;
        None
    }

    /// Query the terminal colour-scheme preference with DSR `CSI ? 996 n`,
    /// pumping input until the `CSI ? 997 ; n n` report or `timeout`
    /// (upstream `queryTerminalColorScheme`).
    pub fn query_terminal_color_scheme(
        &mut self,
        timeout: Duration,
    ) -> Option<TerminalColorScheme> {
        let report: std::sync::Arc<std::sync::Mutex<Option<TerminalColorScheme>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let shared = std::sync::Arc::clone(&report);
        let listener = self.on_terminal_color_scheme_change(Box::new(move |scheme| {
            if let Ok(mut slot) = shared.lock() {
                *slot = Some(scheme);
            }
        }));
        self.terminal.write("\u{1b}[?996n");

        let deadline = Instant::now() + timeout;
        let has_report = |report: &std::sync::Arc<
            std::sync::Mutex<Option<TerminalColorScheme>>,
        >| { report.lock().map(|slot| slot.is_some()).unwrap_or(false) };
        while !has_report(&report) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(data) = self
                .terminal
                .read_input(remaining.min(Duration::from_millis(20)))
            else {
                continue;
            };
            for sequence in self.feed_and_split(&data) {
                self.handle_terminal_input(&sequence);
            }
        }
        let result = report.lock().ok().and_then(|slot| *slot);
        self.remove_terminal_color_scheme_listener(listener);
        result
    }

    /// Split raw input bytes through the terminal's sequence parser when the
    /// terminal provides one.
    fn feed_and_split(&mut self, data: &str) -> Vec<String> {
        let sequences = self.terminal.feed_input_bytes(data, Instant::now());
        if sequences.is_empty() {
            vec![data.to_string()]
        } else {
            sequences
        }
    }

    // --- rendering -------------------------------------------------------

    /// Render the mounted roots (upstream `TuiBase.render`). Overlays are
    /// composited and line resets applied by the screen renderer, exactly as
    /// upstream splits it.
    pub fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for (_, component) in &mut self.roots {
            lines.extend(component.render(width));
        }
        lines
    }

    /// Find and strip [`CURSOR_MARKER`], returning the hardware cursor
    /// position (upstream `extractCursorPosition`).
    pub fn extract_cursor_position(
        &self,
        lines: &mut [String],
        height: usize,
    ) -> Option<(usize, usize)> {
        extract_cursor_position(lines, height)
    }

    pub fn request_render(&mut self, force: bool) {
        if force {
            self.render_pending = true;
            self.next_render_at = Some(Instant::now());
            return;
        }
        if self.render_pending {
            return;
        }
        self.render_pending = true;
        let due = Instant::now() + Duration::from_millis(MIN_RENDER_INTERVAL_MS);
        self.next_render_at = Some(due);
    }

    /// Whether a frame is due at `now` (host-driven replacement for
    /// upstream's timer).
    pub fn render_due(&self, now: Instant) -> bool {
        self.render_pending && self.next_render_at.is_some_and(|due| now >= due)
    }

    /// Take the pending render request, if due (the host then draws a frame).
    pub fn take_render_request(&mut self, now: Instant) -> bool {
        if !self.render_due(now) {
            return false;
        }
        self.render_pending = false;
        self.next_render_at = None;
        self.last_render_at = Some(now);
        true
    }

    pub fn render_now(&mut self) -> bool {
        self.render_pending = false;
        self.next_render_at = None;
        self.last_render_at = Some(Instant::now());
        true
    }

    // --- lifecycle -------------------------------------------------------

    pub fn start(&mut self) {
        self.stopped = false;
        self.terminal.start();
        self.terminal.hide_cursor();
        if self.color_scheme_notifications {
            self.terminal.write("\u{1b}[?2031h");
        }
        self.query_cell_size();
        self.request_render(false);
    }

    pub fn stop(&mut self, _options: TuiStopOptions) {
        self.stopped = true;
        self.render_pending = false;
        self.next_render_at = None;
        if self.color_scheme_notifications {
            self.terminal.write("\u{1b}[?2031l");
        }
        self.terminal.show_cursor();
        self.terminal.stop();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn query_cell_size(&mut self) {
        // Cell size only matters for image rendering.
        if crate::terminal_image::get_capabilities().images.is_none() {
            return;
        }
        // CSI 16 t — reply: CSI 6 ; height ; width t
        self.terminal.write("\u{1b}[16t");
    }
}
