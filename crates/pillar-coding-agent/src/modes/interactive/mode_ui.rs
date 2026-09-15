//! Port of the status-indicator, chat-notice and queued-message parts of
//! packages/coding-agent/src/modes/interactive/interactive-mode.ts (pi
//! v0.84.3): the status container management (`showStatusIndicator` /
//! `clearStatusIndicator` / the working-indicator knobs), the chat notices
//! (`showStatus` / `showError` / `showWarning`), and the pending-messages
//! display with the compaction queue.
//!
//! divergences: the editor escape-handler swapping (`defaultEditor.onEscape`)
//! and the `session.getSteeringMessages()` / `clearQueue()` calls stay at the
//! mode layer (the port's editor is host-dispatched); this module owns the
//! UI state and takes the session queues as arguments. `ui.requestRender` is
//! host-driven and omitted.

use pillar_tui::components::{Spacer, Text, TruncatedText};
use pillar_tui::tui::{Component, Container, TuiMode};

use crate::core::extensions_types::WorkingIndicatorOptions;
use crate::modes::interactive::components::keybinding_hints::key_display_text;
use crate::modes::interactive::components::status_indicator::{
    IdleStatus, RetryStatusIndicator, StatusIndicator, StatusIndicatorKind,
    working_status_indicator,
};
use crate::modes::interactive::theme::theme;
use crate::modes::interactive::transcript::{InteractiveTranscript, Shared};

/// The default working status message (upstream `defaultWorkingMessage`).
pub const DEFAULT_WORKING_MESSAGE: &str = "Working...";

/// The queued-message slot (upstream `compactionQueuedMessages` entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Steer,
    FollowUp,
}

/// The status container management (upstream the
/// `activeStatusIndicator` / `statusContainer` fields and methods).
pub struct StatusUi {
    pub status: Container,
    active: Option<ActiveIndicator>,
    tui_mode: TuiMode,
    clear_on_shrink: bool,
    /// Upstream `workingVisible`.
    pub working_visible: bool,
    /// Upstream `workingMessage` / `workingIndicatorOptions`.
    working_message: Option<String>,
    working_indicator_options: Option<WorkingIndicatorOptions>,
}

/// The active indicator (upstream the `StatusIndicator` base class with
/// the retry subclass carrying the countdown).
enum ActiveIndicator {
    Status(Shared<StatusIndicator>),
    Retry(Shared<RetryStatusIndicator>),
}

impl ActiveIndicator {
    fn kind(&self) -> StatusIndicatorKind {
        match self {
            ActiveIndicator::Status(shared) => shared.lock().kind(),
            ActiveIndicator::Retry(shared) => shared.lock().indicator().kind(),
        }
    }

    fn dispose(&self) {
        match self {
            ActiveIndicator::Status(shared) => shared.lock().dispose(),
            ActiveIndicator::Retry(shared) => shared.lock().dispose(),
        }
    }

    /// Drive the loader/countdown (host pump). `true` means "changed".
    fn tick(&self) -> bool {
        match self {
            ActiveIndicator::Status(shared) => shared.lock().tick(),
            ActiveIndicator::Retry(shared) => shared.lock().tick(),
        }
    }

    /// The plain indicator for working-message updates.
    fn as_status(&self) -> Option<&Shared<StatusIndicator>> {
        match self {
            ActiveIndicator::Status(shared) => Some(shared),
            ActiveIndicator::Retry(_) => None,
        }
    }
}

impl StatusUi {
    pub fn new(tui_mode: TuiMode, clear_on_shrink: bool) -> Self {
        Self {
            status: Container::new(),
            active: None,
            tui_mode,
            clear_on_shrink,
            working_visible: true,
            working_message: None,
            working_indicator_options: None,
        }
    }

    /// The active indicator kind, if any (upstream
    /// `activeStatusIndicator?.kind`).
    pub fn active_kind(&self) -> Option<StatusIndicatorKind> {
        self.active.as_ref().map(ActiveIndicator::kind)
    }

    /// Upstream `showStatusIndicator` (any `StatusIndicator` subclass).
    pub fn show_status_indicator(&mut self, indicator: StatusIndicator) {
        self.install(ActiveIndicator::Status(Shared::new(indicator)));
    }

    /// Upstream `showStatusIndicator(new RetryStatusIndicator(...))`.
    pub fn show_retry_indicator(&mut self, indicator: RetryStatusIndicator) {
        self.install(ActiveIndicator::Retry(Shared::new(indicator)));
    }

    fn install(&mut self, active: ActiveIndicator) {
        if let Some(previous) = &self.active {
            previous.dispose();
        }
        let child: Box<dyn Component> = match &active {
            ActiveIndicator::Status(shared) => Box::new(shared.clone()),
            ActiveIndicator::Retry(shared) => Box::new(shared.clone()),
        };
        self.active = Some(active);
        self.status.clear();
        self.status.add_child(child);
    }

    /// Upstream `clearStatusIndicator`.
    pub fn clear_status_indicator(&mut self, kind: Option<StatusIndicatorKind>) {
        if let Some(kind) = kind {
            if self.active_kind() != Some(kind) {
                return;
            }
        }
        let had_active = self.active.is_some();
        if let Some(active) = &self.active {
            active.dispose();
        }
        self.active = None;
        self.status.clear();
        if had_active && self.tui_mode == TuiMode::Regular && self.clear_on_shrink {
            self.status.add_child(Box::new(IdleStatus));
        }
    }

    /// Upstream `showWorkingStatusIndicator`.
    pub fn show_working_status(&mut self) {
        let message = self
            .working_message
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_string());
        self.show_status_indicator(working_status_indicator(
            &message,
            self.working_indicator_options.clone(),
        ));
    }

    /// Upstream `setWorkingVisible`.
    pub fn set_working_visible(&mut self, visible: bool, is_streaming: bool) {
        self.working_visible = visible;
        if !visible {
            self.clear_status_indicator(Some(StatusIndicatorKind::Working));
            return;
        }
        if is_streaming && self.active_kind() != Some(StatusIndicatorKind::Working) {
            self.show_working_status();
        }
    }

    /// Upstream `setWorkingIndicator`.
    pub fn set_working_indicator(&mut self, options: Option<WorkingIndicatorOptions>) {
        self.working_indicator_options = options.clone();
        if self.active_kind() == Some(StatusIndicatorKind::Working) {
            if let Some(active) = self.active.as_ref().and_then(ActiveIndicator::as_status) {
                active
                    .lock()
                    .loader_mut()
                    .set_indicator(options.map(crate::modes::interactive::components::status_indicator::loader_indicator_options));
            }
        }
    }

    /// Upstream the `workingMessage` setter (extension working-status
    /// updates).
    pub fn set_working_message(&mut self, message: Option<String>) {
        self.working_message = message;
        if self.active_kind() == Some(StatusIndicatorKind::Working) {
            if let Some(active) = self.active.as_ref().and_then(ActiveIndicator::as_status) {
                let message = self
                    .working_message
                    .clone()
                    .unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_string());
                active.lock().set_message(&message);
            }
        }
    }

    /// Drive the active loader/countdown (host pump; upstream ticks inside
    /// the indicator's own interval).
    pub fn tick(&mut self) -> bool {
        match &self.active {
            Some(active) => active.tick(),
            None => false,
        }
    }
}

/// The pending-messages display (upstream the `pendingMessagesContainer`
/// field and `updatePendingMessagesDisplay`) plus the compaction queue
/// (`compactionQueuedMessages`).
pub struct PendingMessagesUi {
    pub container: Container,
    compaction_queue: Vec<(String, QueueMode)>,
}

impl PendingMessagesUi {
    pub fn new() -> Self {
        Self {
            container: Container::new(),
            compaction_queue: Vec::new(),
        }
    }

    /// The session queues merged with the compaction queue (upstream
    /// `getAllQueuedMessages`).
    pub fn all_queued_messages(
        &self,
        session_steering: &[String],
        session_follow_up: &[String],
    ) -> (Vec<String>, Vec<String>) {
        let mut steering: Vec<String> = session_steering.to_vec();
        let mut follow_up: Vec<String> = session_follow_up.to_vec();
        for (text, mode) in &self.compaction_queue {
            match mode {
                QueueMode::Steer => steering.push(text.clone()),
                QueueMode::FollowUp => follow_up.push(text.clone()),
            }
        }
        (steering, follow_up)
    }

    /// Upstream `updatePendingMessagesDisplay`.
    pub fn update_display(&mut self, session_steering: &[String], session_follow_up: &[String]) {
        self.container.clear();
        let (steering, follow_up) = self.all_queued_messages(session_steering, session_follow_up);
        if steering.is_empty() && follow_up.is_empty() {
            return;
        }
        self.container.add_child(Box::new(Spacer::new(1)));
        for message in &steering {
            let text = theme().fg("dim", &format!("Steering: {message}"));
            self.container
                .add_child(Box::new(TruncatedText::new(&text, 1, 0)));
        }
        for message in &follow_up {
            let text = theme().fg("dim", &format!("Follow-up: {message}"));
            self.container
                .add_child(Box::new(TruncatedText::new(&text, 1, 0)));
        }
        let dequeue_hint = key_display_text("app.message.dequeue");
        let hint_text = theme().fg(
            "dim",
            &format!("\u{21b3} {dequeue_hint} to edit all queued messages"),
        );
        self.container
            .add_child(Box::new(TruncatedText::new(&hint_text, 1, 0)));
    }

    /// Upstream `queueCompactionMessage` (the editor history and status
    /// notice stay at the mode layer).
    pub fn queue_compaction_message(&mut self, text: String, mode: QueueMode) {
        self.compaction_queue.push((text, mode));
    }

    /// The compaction queue, cleared (upstream `clearAllQueues`'s
    /// compaction half; the session queue clear stays at the mode layer).
    pub fn take_compaction_queue(&mut self) -> Vec<(String, QueueMode)> {
        std::mem::take(&mut self.compaction_queue)
    }
}

impl Default for PendingMessagesUi {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Chat notices (upstream showStatus / showError / showWarning)
// ============================================================================

/// Whether the chat child at `index` is the tracked status text handle.
fn children_last_is_tracked_text(transcript: &mut InteractiveTranscript, index: usize) -> bool {
    let Some(text) = transcript.last_status_text.clone() else {
        return false;
    };

    transcript
        .chat
        .children_mut()
        .get_mut(index)
        .and_then(|child| child.as_any_mut())
        .and_then(|any| any.downcast_mut::<Shared<Text>>())
        .is_some_and(|candidate| Shared::ptr_eq(candidate, &text))
}

/// Whether the chat child at `index` is the tracked status spacer handle.
fn children_second_last_is_tracked_spacer(
    transcript: &mut InteractiveTranscript,
    index: usize,
) -> bool {
    let Some(spacer) = transcript.last_status_spacer.clone() else {
        return false;
    };
    transcript
        .chat
        .children_mut()
        .get_mut(index)
        .and_then(|child| child.as_any_mut())
        .and_then(|any| any.downcast_mut::<Shared<Spacer>>())
        .is_some_and(|candidate| Shared::ptr_eq(candidate, &spacer))
}

impl InteractiveTranscript {
    /// Upstream `showStatus`: a dim chat line that rewrites in place while
    /// it is still the last pair of children.
    pub fn show_status(&mut self, message: &str) {
        if self.last_status_matches() {
            if let Some(text) = &self.last_status_text {
                text.lock().set_text(&theme().fg("dim", message));
            }
            return;
        }

        let spacer = Shared::new(Spacer::new(1));
        let text = Shared::new(Text::new(&theme().fg("dim", message), 1, 0));
        self.chat.add_child(Box::new(spacer.clone()));
        self.chat.add_child(Box::new(text.clone()));
        self.last_status_spacer = Some(spacer);
        self.last_status_text = Some(text);
    }

    /// Upstream `showError`.
    pub fn show_error(&mut self, message: &str) {
        let text = theme().fg("error", &format!("Error: {message}"));
        self.chat.add_child(Box::new(Spacer::new(1)));
        self.chat
            .add_child(Box::new(Text::new(&text, self.output_pad(), 0)));
    }

    /// Upstream `showWarning`.
    pub fn show_warning(&mut self, message: &str) {
        let text = theme().fg("warning", &format!("Warning: {message}"));
        self.chat.add_child(Box::new(Spacer::new(1)));
        self.chat.add_child(Box::new(Text::new(&text, 1, 0)));
    }

    /// Whether the chat's last two children are still the tracked status
    /// pair (upstream the identity comparison against
    /// `lastStatusSpacer` / `lastStatusText`).
    fn last_status_matches(&mut self) -> bool {
        if self.last_status_spacer.is_none() || self.last_status_text.is_none() {
            return false;
        }
        if self.chat.len() < 2 {
            return false;
        }
        let len = self.chat.len();
        let last_is_text = children_last_is_tracked_text(self, len - 1);
        let second_last_is_spacer = children_second_last_is_tracked_spacer(self, len - 2);
        last_is_text && second_last_is_spacer
    }
}

impl Component for StatusUi {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.status.render(width)
    }

    fn invalidate(&mut self) {
        self.status.invalidate();
    }
}

impl Component for PendingMessagesUi {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
