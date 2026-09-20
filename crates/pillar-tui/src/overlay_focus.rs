//! Port of the overlay stack + focus routing state machine from
//! packages/tui/src/tui.ts (pi v0.84.3 `TuiBase`): show/hide/hide-top/
//! focus/unfocus with pre-focus chains, non-capturing overlays, hidden
//! overlays, and the blocked-focus-restore machinery.
//!
//! divergences: components are identified by u64 ids registered with
//! the host (upstream uses object identity and mutates `focused` on
//! Component objects); render scheduling and terminal wiring stay
//! host-side. Focus is tracked as the current id; the host maps ids to
//! component instances.

use std::collections::HashMap;

/// An overlay stack entry (upstream `OverlayStackEntry`).
#[derive(Debug, Clone)]
pub struct OverlayStackEntry {
    pub component_id: u64,
    pub options: OverlayEntryOptions,
    pub pre_focus: Option<u64>,
    pub hidden: bool,
    pub focus_order: u64,
}

/// The subset of OverlayOptions the focus machine consults.
#[derive(Debug, Clone, Default)]
pub struct OverlayEntryOptions {
    pub non_capturing: bool,
    /// Visibility callback flag computed by the host per render cycle
    /// (upstream calls options.visible(termW, termH)).
    pub visible: bool,
}

/// Overlay handle operations (upstream `OverlayHandle`), recorded for
/// the host to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayOp {
    /// Whether a render is needed after the op.
    None,
    RequestRender,
    HideCursor,
}

/// Focus restore state (upstream `OverlayFocusRestoreState`).
#[derive(Debug, Clone, PartialEq)]
pub enum FocusRestoreState {
    Inactive,
    /// The overlay can regain focus on the next input.
    Eligible {
        overlay_index: usize,
    },
    /// Focus moved to a base component; the overlay resumes when
    /// unblocked.
    Blocked {
        overlay_index: usize,
        blocked_by: Option<u64>,
        resume: Resume,
    },
}

/// Resume behavior for a blocked overlay (upstream
/// `OverlayBlockedFocusResume`).
#[derive(Debug, Clone, PartialEq)]
pub enum Resume {
    RestoreOverlay,
    FocusTarget(Option<u64>),
}

/// The overlay stack + focus state (upstream the private fields of
/// TuiBase).
pub struct OverlayFocusMachine {
    pub stack: Vec<OverlayStackEntry>,
    pub focused: Option<u64>,
    focus_order_counter: u64,
    pub focus_restore: FocusRestoreState,
    /// Component mount graph supplied by the host: id → child ids
    /// (upstream walks Container children).
    pub mount_graph: HashMap<u64, Vec<u64>>,
}

impl OverlayFocusMachine {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            focused: None,
            focus_order_counter: 0,
            focus_restore: FocusRestoreState::Inactive,
            mount_graph: HashMap::new(),
        }
    }

    fn is_overlay_visible(&self, entry: &OverlayStackEntry) -> bool {
        !entry.hidden && entry.options.visible
    }

    fn find_entry(&self, component_id: u64) -> Option<usize> {
        self.stack
            .iter()
            .position(|e| e.component_id == component_id)
    }

    fn topmost_visible(&self) -> Option<usize> {
        let mut topmost: Option<usize> = None;
        for (index, entry) in self.stack.iter().enumerate() {
            if entry.options.non_capturing || !self.is_overlay_visible(entry) {
                continue;
            }
            if topmost.is_none_or(|t| self.stack[index].focus_order > self.stack[t].focus_order) {
                topmost = Some(index);
            }
        }
        topmost
    }

    fn is_component_mounted(&self, component_id: u64) -> bool {
        fn contains(
            graph: &HashMap<u64, Vec<u64>>,
            root: u64,
            target: u64,
            seen: &mut Vec<u64>,
        ) -> bool {
            if root == target {
                return true;
            }
            if seen.contains(&root) {
                return false;
            }
            seen.push(root);
            graph.get(&root).is_some_and(|children| {
                children
                    .iter()
                    .any(|child| contains(graph, *child, target, seen))
            })
        }
        self.mount_graph
            .keys()
            .copied()
            .any(|root| contains(&self.mount_graph, root, component_id, &mut Vec::new()))
    }

    fn is_overlay_focus_ancestor(&self, entry: &OverlayStackEntry, component_id: u64) -> bool {
        let mut visited = Vec::new();
        let mut current = entry.pre_focus;
        while let Some(id) = current {
            if visited.contains(&id) {
                break;
            }
            visited.push(id);
            if id == component_id {
                return true;
            }
            current = self
                .stack
                .iter()
                .find(|overlay| overlay.component_id == id)
                .and_then(|overlay| overlay.pre_focus);
        }
        false
    }

    fn clear_focus_restore(&mut self) {
        self.focus_restore = FocusRestoreState::Inactive;
    }

    fn visible_focus_restore(&self) -> FocusRestoreState {
        match &self.focus_restore {
            FocusRestoreState::Inactive => FocusRestoreState::Inactive,
            FocusRestoreState::Eligible { overlay_index } => match self.stack.get(*overlay_index) {
                Some(entry) if self.is_overlay_visible(entry) => self.focus_restore.clone(),
                _ => FocusRestoreState::Inactive,
            },
            FocusRestoreState::Blocked {
                overlay_index,
                blocked_by,
                resume,
            } => match self.stack.get(*overlay_index) {
                Some(entry) if self.is_overlay_visible(entry) => FocusRestoreState::Blocked {
                    overlay_index: *overlay_index,
                    blocked_by: *blocked_by,
                    resume: resume.clone(),
                },
                _ => FocusRestoreState::Inactive,
            },
        }
    }

    fn resolve_blocked_resume(&mut self, resume: &Resume) -> Option<u64> {
        match resume {
            Resume::RestoreOverlay => {
                let index = match &self.focus_restore {
                    FocusRestoreState::Blocked { overlay_index, .. } => *overlay_index,
                    _ => 0,
                };
                self.stack.get(index).map(|e| e.component_id)
            }
            Resume::FocusTarget(target) => {
                self.clear_focus_restore();
                *target
            }
        }
    }

    /// Set focus (upstream `setFocusInternal`). Returns the resolved
    /// focus id for the host to apply `focused` flags.
    pub fn set_focus(&mut self, component: Option<u64>) -> Option<u64> {
        self.set_focus_internal(component, false)
    }

    fn set_focus_internal(&mut self, component: Option<u64>, clear_restore: bool) -> Option<u64> {
        let previous_focus = self.focused;
        let mut next_focus = component;
        let previous_focused_overlay = previous_focus
            .and_then(|id| self.find_entry(id))
            .filter(|index| self.is_overlay_visible(&self.stack[*index]));
        let next_focus_is_overlay = next_focus.is_some_and(|id| self.find_entry(id).is_some());
        let restore_state = self.visible_focus_restore();

        if let Some(next) = next_focus {
            if !next_focus_is_overlay {
                match &restore_state {
                    FocusRestoreState::Blocked {
                        blocked_by: Some(blocked),
                        resume,
                        ..
                    } if *blocked == previous_focus.unwrap_or(u64::MAX) => {
                        let resume = resume.clone();
                        let restore_overlay_index = self.focus_restore.overlay_index();
                        let Some(restore_overlay_index) = restore_overlay_index else {
                            return next_focus;
                        };
                        let blocked_by = self
                            .stack
                            .get(restore_overlay_index)
                            .map(|e| e.component_id);
                        if matches!(resume, Resume::FocusTarget(_))
                            || blocked_by.is_none_or(|id| !self.is_component_mounted(id))
                        {
                            next_focus = self.resolve_blocked_resume(&resume);
                        } else {
                            self.focus_restore = FocusRestoreState::Blocked {
                                overlay_index: restore_overlay_index,
                                blocked_by: Some(next),
                                resume,
                            };
                        }
                    }
                    FocusRestoreState::Blocked { .. } => {}
                    _ => {
                        if let Some(prev_index) = previous_focused_overlay
                            && restore_state != FocusRestoreState::Inactive
                            && self.focus_restore.overlay_index() == Some(prev_index)
                            && !self.is_overlay_focus_ancestor(&self.stack[prev_index], next)
                        {
                            self.focus_restore = FocusRestoreState::Blocked {
                                overlay_index: prev_index,
                                blocked_by: Some(next),
                                resume: Resume::RestoreOverlay,
                            };
                        }
                    }
                }
            }
        } else {
            match &restore_state {
                FocusRestoreState::Blocked {
                    blocked_by: Some(blocked),
                    resume,
                    ..
                } if *blocked == previous_focus.unwrap_or(u64::MAX) => {
                    let resume = resume.clone();
                    next_focus = self.resolve_blocked_resume(&resume);
                }
                _ => {
                    if clear_restore {
                        self.clear_focus_restore();
                    }
                }
            }
        }

        self.focused = next_focus;

        if let Some(next) = next_focus
            && let Some(index) = self.find_entry(next)
            && self.is_overlay_visible(&self.stack[index])
        {
            self.focus_restore = FocusRestoreState::Eligible {
                overlay_index: index,
            };
        }
        next_focus
    }

    /// Show an overlay (upstream `showOverlay`): pushes the entry and
    /// focuses it unless non-capturing or invisible.
    pub fn show_overlay(&mut self, component_id: u64, options: OverlayEntryOptions) -> OverlayOp {
        self.focus_order_counter += 1;
        let entry = OverlayStackEntry {
            component_id,
            options,
            pre_focus: self.focused,
            hidden: false,
            focus_order: self.focus_order_counter,
        };
        self.stack.push(entry);
        let visible = self.is_overlay_visible(self.stack.last().unwrap());
        if visible && !self.stack.last().unwrap().options.non_capturing {
            self.set_focus(Some(component_id));
        }
        OverlayOp::HideCursor
    }

    /// Remove an overlay (upstream the handle's `hide`).
    pub fn remove_overlay(&mut self, component_id: u64) -> OverlayOp {
        let Some(index) = self.find_entry(component_id) else {
            return OverlayOp::None;
        };
        let pre_focus = self.stack[index].pre_focus;
        self.clear_focus_restore_for_index(index);
        self.retarget_pre_focus(index);
        self.stack.remove(index);
        if self.focused == Some(component_id) {
            let top_visible = self.topmost_visible();
            self.set_focus(
                top_visible
                    .map(|i| self.stack[i].component_id)
                    .or(pre_focus),
            );
        }
        if self.stack.is_empty() {
            return OverlayOp::HideCursor;
        }
        OverlayOp::RequestRender
    }

    /// Hide the topmost overlay (upstream `hideOverlay`).
    pub fn hide_top_overlay(&mut self) -> OverlayOp {
        let Some(overlay) = self.stack.last().cloned() else {
            return OverlayOp::None;
        };
        let index = self.stack.len() - 1;
        self.clear_focus_restore_for_index(index);
        self.retarget_pre_focus(index);
        self.stack.pop();
        if self.focused == Some(overlay.component_id) {
            let top_visible = self.topmost_visible();
            self.set_focus(
                top_visible
                    .map(|index| self.stack[index].component_id)
                    .or(overlay.pre_focus),
            );
        }
        if self.stack.is_empty() {
            OverlayOp::HideCursor
        } else {
            OverlayOp::RequestRender
        }
    }

    /// Temporarily hide/show an overlay (upstream the handle's
    /// `setHidden`).
    pub fn set_hidden(&mut self, component_id: u64, hidden: bool) -> OverlayOp {
        let Some(index) = self.find_entry(component_id) else {
            return OverlayOp::None;
        };
        if self.stack[index].hidden == hidden {
            return OverlayOp::None;
        }
        self.stack[index].hidden = hidden;
        if hidden {
            self.clear_focus_restore_for_index(index);
            if self.focused == Some(component_id) {
                let top_visible = self.topmost_visible();
                let pre_focus = self.stack[index].pre_focus;
                self.set_focus(
                    top_visible
                        .map(|i| self.stack[i].component_id)
                        .or(pre_focus),
                );
            }
        } else if !self.stack[index].options.non_capturing
            && self.is_overlay_visible(&self.stack[index])
        {
            self.focus_order_counter += 1;
            self.stack[index].focus_order = self.focus_order_counter;
            self.set_focus(Some(component_id));
        }
        OverlayOp::RequestRender
    }

    /// Focus an overlay (upstream the handle's `focus`).
    pub fn focus_overlay(&mut self, component_id: u64) -> OverlayOp {
        let Some(index) = self.find_entry(component_id) else {
            return OverlayOp::None;
        };
        if !self.is_overlay_visible(&self.stack[index]) {
            return OverlayOp::None;
        }
        self.focus_order_counter += 1;
        self.stack[index].focus_order = self.focus_order_counter;
        self.set_focus(Some(component_id));
        OverlayOp::RequestRender
    }

    /// Release overlay focus (upstream the handle's `unfocus` without
    /// an explicit target).
    pub fn unfocus_overlay(&mut self, component_id: u64) -> OverlayOp {
        let is_focused = self.focused == Some(component_id);
        let has_pending_restore =
            self.focus_restore.overlay_index() == self.find_entry(component_id);
        if !is_focused && !has_pending_restore {
            return OverlayOp::None;
        }
        if let Some(index) = self.find_entry(component_id) {
            self.clear_focus_restore_for_index(index);
            if is_focused {
                let top_visible = self.topmost_visible();
                let fallback = top_visible
                    .filter(|i| *i != index)
                    .map(|i| self.stack[i].component_id)
                    .or(self.stack[index].pre_focus);
                self.set_focus(fallback);
            }
        }
        OverlayOp::RequestRender
    }

    /// Whether any visible overlay exists (upstream `hasOverlay`).
    /// The topmost overlay id (upstream the last stack entry).
    pub fn topmost_overlay_id(&self) -> Option<u64> {
        self.stack.last().map(|entry| entry.component_id)
    }

    pub fn has_overlay(&self) -> bool {
        self.stack.iter().any(|e| self.is_overlay_visible(e))
    }

    /// Whether the focused component is a visible overlay (upstream
    /// `isOverlayFocused`).
    pub fn is_overlay_focused(&self) -> bool {
        self.focused
            .and_then(|id| self.find_entry(id))
            .is_some_and(|index| self.is_overlay_visible(&self.stack[index]))
    }

    /// Route focus before dispatching input (upstream the
    /// handleTerminalInput focus-restore block). Returns the focus the
    /// host should dispatch to.
    pub fn route_input_focus(&mut self) -> Option<u64> {
        // Redirect when the focused overlay became invisible.
        if let Some(focused) = self.focused
            && let Some(index) = self.find_entry(focused)
            && !self.is_overlay_visible(&self.stack[index])
        {
            let top_visible = self.topmost_visible();
            if let Some(top) = top_visible {
                self.set_focus(Some(self.stack[top].component_id));
            } else {
                let pre_focus = self.stack[index].pre_focus;
                self.set_focus_internal(pre_focus, false);
            }
        }
        if !self.is_overlay_focused() {
            match self.visible_focus_restore() {
                FocusRestoreState::Eligible { overlay_index } => {
                    let id = self.stack[overlay_index].component_id;
                    self.set_focus(Some(id));
                }
                FocusRestoreState::Blocked {
                    blocked_by, resume, ..
                } => {
                    if blocked_by != self.focused {
                        match resume {
                            Resume::RestoreOverlay => {
                                let index = self.focus_restore.overlay_index().unwrap_or(0);
                                let id = self.stack[index].component_id;
                                self.set_focus(Some(id));
                            }
                            Resume::FocusTarget(target) => {
                                self.clear_focus_restore();
                                self.set_focus(target);
                            }
                        }
                    }
                }
                FocusRestoreState::Inactive => {}
            }
        }
        self.focused
    }

    fn clear_focus_restore_for_index(&mut self, index: usize) {
        if self.focus_restore.overlay_index() == Some(index) {
            self.clear_focus_restore();
        }
    }

    fn retarget_pre_focus(&mut self, removed_index: usize) {
        let removed = self.stack[removed_index].clone();
        for overlay in &mut self.stack {
            if overlay.pre_focus == Some(removed.component_id) {
                overlay.pre_focus = removed.pre_focus;
            }
        }
    }
}

trait FocusRestoreExt {
    fn overlay_index(&self) -> Option<usize>;
}

impl FocusRestoreExt for FocusRestoreState {
    fn overlay_index(&self) -> Option<usize> {
        match self {
            FocusRestoreState::Eligible { overlay_index }
            | FocusRestoreState::Blocked { overlay_index, .. } => Some(*overlay_index),
            FocusRestoreState::Inactive => None,
        }
    }
}

impl Default for OverlayFocusMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITOR: u64 = 1;
    const OVERLAY_A: u64 = 10;
    const OVERLAY_B: u64 = 11;
    const PICKER: u64 = 20;

    fn capturing_visible() -> OverlayEntryOptions {
        OverlayEntryOptions {
            non_capturing: false,
            visible: true,
        }
    }

    fn non_capturing_visible() -> OverlayEntryOptions {
        OverlayEntryOptions {
            non_capturing: true,
            visible: true,
        }
    }

    #[test]
    fn capturing_overlay_takes_focus_on_show() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        assert_eq!(machine.focused, Some(OVERLAY_A));
        assert!(machine.has_overlay());
        assert!(machine.is_overlay_focused());
    }

    #[test]
    fn non_capturing_overlay_preserves_focus() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, non_capturing_visible());
        assert_eq!(machine.focused, Some(EDITOR));
        // focus() transfers focus to it.
        machine.focus_overlay(OVERLAY_A);
        assert_eq!(machine.focused, Some(OVERLAY_A));
        // unfocus() restores previous focus.
        machine.unfocus_overlay(OVERLAY_A);
        assert_eq!(machine.focused, Some(EDITOR));
    }

    #[test]
    fn hide_top_overlay_restores_pre_focus() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.hide_top_overlay();
        assert_eq!(machine.focused, Some(EDITOR));
        assert!(!machine.has_overlay());
    }

    #[test]
    fn stacked_overlays_restore_stepwise() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.show_overlay(OVERLAY_B, capturing_visible());
        assert_eq!(machine.focused, Some(OVERLAY_B));
        machine.hide_top_overlay();
        assert_eq!(machine.focused, Some(OVERLAY_A));
        machine.hide_top_overlay();
        assert_eq!(machine.focused, Some(EDITOR));
    }

    #[test]
    fn hiding_focused_overlay_falls_back_to_topmost_visible() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.show_overlay(OVERLAY_B, capturing_visible());
        machine.remove_overlay(OVERLAY_B);
        assert_eq!(machine.focused, Some(OVERLAY_A));
    }

    #[test]
    fn set_hidden_hides_and_restores() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.set_hidden(OVERLAY_A, true);
        assert_eq!(machine.focused, Some(EDITOR));
        assert!(!machine.has_overlay());
        // Showing again refocuses (bumps focus order).
        machine.set_hidden(OVERLAY_A, false);
        assert_eq!(machine.focused, Some(OVERLAY_A));
    }

    #[test]
    fn set_hidden_non_capturing_does_not_autofocus() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, non_capturing_visible());
        machine.set_hidden(OVERLAY_A, true);
        machine.set_hidden(OVERLAY_A, false);
        assert_eq!(machine.focused, Some(EDITOR));
    }

    #[test]
    fn invisible_capturing_overlay_does_not_steal_focus_on_show() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(
            OVERLAY_A,
            OverlayEntryOptions {
                non_capturing: false,
                visible: false,
            },
        );
        assert_eq!(machine.focused, Some(EDITOR));
    }

    #[test]
    fn focused_overlay_becoming_invisible_redirects_input() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        // Overlay becomes invisible (resize / visible() callback).
        machine.stack[0].options.visible = false;
        let focus = machine.route_input_focus();
        // Falls back to the pre-focus (editor).
        assert_eq!(focus, Some(EDITOR));
    }

    #[test]
    fn eligible_restore_refocuses_overlay_repeatedly() {
        // Upstream: with the restore state eligible (overlay was focused,
        // host moved to base), each input routes back to the overlay —
        // the overlay is the pre-focus ancestor of the editor, so the
        // blocked state is not created.
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.set_focus(Some(EDITOR));
        let focus = machine.route_input_focus();
        assert_eq!(focus, Some(OVERLAY_A));
    }

    #[test]
    fn mount_graph_gates_blocked_resume() {
        // With a genuinely blocked state (blocked_by present), routing
        // keeps the focus on the base component while it stays mounted.
        let mut machine = OverlayFocusMachine::new();
        machine.mount_graph.insert(EDITOR, vec![]);
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        // Simulate a blocked state pointing at the editor.
        let index = machine.find_entry(OVERLAY_A).unwrap();
        machine.focus_restore = FocusRestoreState::Blocked {
            overlay_index: index,
            blocked_by: Some(EDITOR),
            resume: Resume::RestoreOverlay,
        };
        machine.focused = Some(EDITOR);
        let focus = machine.route_input_focus();
        assert_eq!(focus, Some(EDITOR));
    }

    #[test]
    fn removed_focused_child_overlay_falls_back_to_editor() {
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.show_overlay(OVERLAY_B, capturing_visible());
        // Remove the focused B: focus goes to A (topmost visible).
        machine.remove_overlay(OVERLAY_B);
        assert_eq!(machine.focused, Some(OVERLAY_A));
        // Then remove A: focus returns to the editor.
        machine.remove_overlay(OVERLAY_A);
        assert_eq!(machine.focused, Some(EDITOR));
    }

    #[test]
    fn picker_overlay_chain_cleanup() {
        // Simulates the sub-overlay pattern: picker opens over an
        // overlay; removing the picker restores the overlay.
        let mut machine = OverlayFocusMachine::new();
        machine.focused = Some(EDITOR);
        machine.show_overlay(OVERLAY_A, capturing_visible());
        machine.show_overlay(PICKER, capturing_visible());
        assert_eq!(machine.focused, Some(PICKER));
        machine.remove_overlay(PICKER);
        assert_eq!(machine.focused, Some(OVERLAY_A));
    }
}
