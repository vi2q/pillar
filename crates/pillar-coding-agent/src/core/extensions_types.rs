//! Port of the session-event payload shapes of
//! packages/coding-agent/src/core/extensions/types.ts (pi v0.84.3):
//! the typed payloads the extension runner dispatches for session
//! lifecycle, tree navigation, compaction, and resource discovery.
//!
//! divergences: AbortSignal-bearing events carry no signal (the port
//! is synchronous; cancellation is the caller's decision on the
//! before-variants); UI/host-bound types (widgets, editors, TUI
//! contexts) are not ported; payloads reference the port's
//! session-entry shapes.

use crate::core::session_entries::{BranchSummaryEntry, SessionEntry};

/// Why a session started (upstream `SessionStartEvent.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    Reload,
    New,
    Resume,
    Fork,
}

/// Why the extension runtime shuts down (upstream
/// `SessionShutdownEvent.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Quit,
    Reload,
    New,
    Resume,
    Fork,
}

/// What triggered compaction (upstream the shared reason union).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    Manual,
    Threshold,
    Overflow,
}

/// Tree navigation preparation (upstream `TreePreparation`).
#[derive(Debug, Clone)]
pub struct TreePreparation {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    pub common_ancestor_id: Option<String>,
    pub entries_to_summarize: Vec<SessionEntry>,
    pub user_wants_summary: bool,
    pub custom_instructions: Option<String>,
    /// True when customInstructions replaces the default prompt.
    pub replace_instructions: Option<bool>,
    pub label: Option<String>,
}

/// Results from a resources_discover handler (upstream
/// `ResourcesDiscoverResult`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourcesDiscoverResult {
    pub skill_paths: Option<Vec<String>>,
    pub prompt_paths: Option<Vec<String>>,
    pub theme_paths: Option<Vec<String>>,
}

/// Typed session lifecycle events (upstream the `SessionEvent`
/// union). Before-variants are cancellable at the call site.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    SessionStart {
        reason: SessionStartReason,
        /// Previously active session file for new/resume/fork.
        previous_session_file: Option<String>,
    },
    SessionInfoChanged {
        /// Current normalized session name; None when cleared.
        name: Option<String>,
    },
    SessionBeforeSwitch {
        reason: SessionStartReason,
        target_session_file: Option<String>,
    },
    SessionBeforeFork {
        entry_id: String,
        /// `before` | `at`.
        position: String,
    },
    SessionBeforeCompact {
        preparation: (),
        branch_entries: Vec<SessionEntry>,
        custom_instructions: Option<String>,
        reason: CompactReason,
        /// True when the aborted turn retries after this compaction.
        will_retry: bool,
    },
    SessionCompact {
        compaction_entry: crate::core::session_entries::CompactionEntry,
        from_extension: bool,
        reason: CompactReason,
        will_retry: bool,
    },
    SessionCompactFailed {
        reason: CompactReason,
        /// Error text when compaction failed for a non-abort reason.
        error_message: Option<String>,
        /// True when compaction was cancelled or aborted.
        aborted: bool,
        will_retry: bool,
        /// True when the failing content came from a
        /// session_before_compact handler.
        from_extension: bool,
    },
    SessionShutdown {
        reason: ShutdownReason,
        /// Destination session file for session replacement.
        target_session_file: Option<String>,
    },
    SessionBeforeTree {
        preparation: TreePreparation,
    },
    SessionTree {
        new_leaf_id: Option<String>,
        old_leaf_id: Option<String>,
        summary_entry: Option<BranchSummaryEntry>,
        from_extension: Option<bool>,
    },
}

impl SessionEvent {
    /// The runner's event type key (upstream `event.type`).
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "session_start",
            Self::SessionInfoChanged { .. } => "session_info_changed",
            Self::SessionBeforeSwitch { .. } => "session_before_switch",
            Self::SessionBeforeFork { .. } => "session_before_fork",
            Self::SessionBeforeCompact { .. } => "session_before_compact",
            Self::SessionCompact { .. } => "session_compact",
            Self::SessionCompactFailed { .. } => "session_compact_failed",
            Self::SessionShutdown { .. } => "session_shutdown",
            Self::SessionBeforeTree { .. } => "session_before_tree",
            Self::SessionTree { .. } => "session_tree",
        }
    }

    /// Whether the runner short-circuits on cancellation (upstream
    /// the session-before family).
    pub fn is_cancellable(&self) -> bool {
        matches!(
            self,
            Self::SessionBeforeSwitch { .. }
                | Self::SessionBeforeFork { .. }
                | Self::SessionBeforeCompact { .. }
                | Self::SessionBeforeTree { .. }
        )
    }
}

/// Resource discovery request (upstream `ResourcesDiscoverEvent`).
#[derive(Debug, Clone)]
pub struct ResourcesDiscoverEvent {
    pub cwd: String,
    /// `startup` | `reload`.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event maps to the runner's string key, and the
    /// cancellable family matches the runner's short-circuit set.
    #[test]
    fn event_types_and_cancellable_family() {
        let events = vec![
            (
                SessionEvent::SessionStart {
                    reason: SessionStartReason::Startup,
                    previous_session_file: None,
                },
                "session_start",
                false,
            ),
            (
                SessionEvent::SessionInfoChanged { name: None },
                "session_info_changed",
                false,
            ),
            (
                SessionEvent::SessionBeforeSwitch {
                    reason: SessionStartReason::New,
                    target_session_file: None,
                },
                "session_before_switch",
                true,
            ),
            (
                SessionEvent::SessionBeforeFork {
                    entry_id: "e1".to_string(),
                    position: "at".to_string(),
                },
                "session_before_fork",
                true,
            ),
            (
                SessionEvent::SessionBeforeCompact {
                    preparation: (),
                    branch_entries: vec![],
                    custom_instructions: None,
                    reason: CompactReason::Manual,
                    will_retry: false,
                },
                "session_before_compact",
                true,
            ),
            (
                SessionEvent::SessionCompact {
                    compaction_entry: crate::core::session_entries::CompactionEntry {
                        base: crate::core::session_entries::SessionEntryBase {
                            id: "c1".to_string(),
                            parent_id: None,
                            timestamp: 1,
                        },
                        summary: "s".to_string(),
                        first_kept_entry_id: "e2".to_string(),
                        tokens_before: 10,
                        details: None,
                        usage: None,
                        from_hook: false,
                    },
                    from_extension: false,
                    reason: CompactReason::Threshold,
                    will_retry: false,
                },
                "session_compact",
                false,
            ),
            (
                SessionEvent::SessionCompactFailed {
                    reason: CompactReason::Overflow,
                    error_message: None,
                    aborted: true,
                    will_retry: false,
                    from_extension: false,
                },
                "session_compact_failed",
                false,
            ),
            (
                SessionEvent::SessionShutdown {
                    reason: ShutdownReason::Quit,
                    target_session_file: None,
                },
                "session_shutdown",
                false,
            ),
            (
                SessionEvent::SessionBeforeTree {
                    preparation: TreePreparation {
                        target_id: "t".to_string(),
                        old_leaf_id: None,
                        common_ancestor_id: None,
                        entries_to_summarize: vec![],
                        user_wants_summary: false,
                        custom_instructions: None,
                        replace_instructions: None,
                        label: None,
                    },
                },
                "session_before_tree",
                true,
            ),
            (
                SessionEvent::SessionTree {
                    new_leaf_id: None,
                    old_leaf_id: None,
                    summary_entry: None,
                    from_extension: None,
                },
                "session_tree",
                false,
            ),
        ];
        for (event, expected_type, cancellable) in events {
            assert_eq!(event.event_type(), expected_type);
            assert_eq!(event.is_cancellable(), cancellable, "{expected_type}");
        }
    }
}

// ============================================================================
// UI-facing extension types (upstream types.ts: markdown transforms, custom
// entry renderers, working-indicator options)
// ============================================================================

/// Which kind of message a Markdown transformer is rendering (upstream
/// `MarkdownTransformContext["messageType"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownMessageType {
    User,
    Assistant,
    AssistantThinking,
}

impl MarkdownMessageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantThinking => "assistant-thinking",
        }
    }
}

/// Context handed to a Markdown transformer (upstream
/// `MarkdownTransformContext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownTransformContext {
    pub message_type: MarkdownMessageType,
    pub is_streaming: bool,
    pub available_width: usize,
}

/// Rewrites Markdown source before rendering (upstream `MarkdownTransformer`).
/// Returning `None` keeps the current source, mirroring upstream's
/// `typeof transformed === "string"` check.
///
/// divergence: upstream passes the transformer list by reference; the port
/// holds it in an `Arc` so components can rebuild their children.
pub type MarkdownTransformer =
    std::sync::Arc<dyn Fn(&str, &MarkdownTransformContext) -> Option<String> + Send + Sync>;

/// Options for rendering a custom session entry (upstream
/// `EntryRenderOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryRenderOptions {
    pub expanded: bool,
}

/// Renders a custom session entry (upstream `EntryRenderer`). The renderer
/// owns the returned component; failures are reported by the caller.
///
/// divergence: upstream returns a live `Component`; the port holds the
/// renderer in an `Arc` so the runner can hand it to the transcript.
pub type EntryRenderer = std::sync::Arc<
    dyn Fn(
            &crate::core::session_entries::CustomEntry,
            &EntryRenderOptions,
            &crate::modes::interactive::theme::Theme,
        ) -> Option<Box<dyn pillar_tui::tui::Component>>
        + Send
        + Sync,
>;

/// Which run mode an extension context describes (upstream
/// `ExtensionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Rpc => "rpc",
            Self::Json => "json",
            Self::Print => "print",
        }
    }
}

/// The host facts an extension context carries (upstream `ExtensionContext`'s
/// `cwd` / `mode` / `hasUI`). The host updates them when the mode changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionContextFacts {
    pub cwd: String,
    pub mode: ExtensionMode,
    /// Whether dialog-capable UI is available (upstream `hasUI`).
    pub has_ui: bool,
}

impl Default for ExtensionContextFacts {
    fn default() -> Self {
        Self {
            cwd: String::new(),
            mode: ExtensionMode::Print,
            has_ui: false,
        }
    }
}

/// One `ctx.ui.*` request: the operation name (snake_case of the upstream
/// method, e.g. `set_status`) plus its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionUiRequest {
    pub op: String,
    pub args: serde_json::Value,
}

/// Host bridge for `ctx.ui` (upstream `ExtensionUIContext`). The port's UI
/// state lives on the pump thread, so a request is only *queued*: the sender
/// never blocks and never touches mode locks (an extension handler runs with
/// the Luau runtime locked, and the transcript's extension renderers lock the
/// same runtime while holding transcript state).
pub type ExtensionUiFn =
    std::sync::Arc<dyn Fn(ExtensionUiRequest) -> Result<(), String> + Send + Sync>;

/// The `ctx.ui` bridge state: the pump-backed sender the interactive run
/// installs plus the requests that arrived before it existed (extensions
/// commonly touch the UI from `session_start`, which fires before the run
/// loop starts).
#[derive(Default)]
pub struct ExtensionUiState {
    /// The pump-backed sender.
    pub bridge: Option<ExtensionUiFn>,
    /// The pump-backed request/answer bridge (dialogs).
    pub ask: Option<ExtensionUiAskFn>,
    /// The pump-backed `ctx.ui.custom` installer.
    pub custom: Option<ExtensionCustomFn>,
    /// Requests queued before [`ExtensionUiState::bridge`] was installed.
    pub pending: Vec<ExtensionUiRequest>,
    /// Custom components queued before [`ExtensionUiState::custom`] was
    /// installed.
    pub pending_custom: Vec<ExtensionCustomSurface>,
}

impl ExtensionUiState {
    /// Hand a request to the bridge, queueing it until one exists (the caller
    /// holds the slot lock; the bridge itself only sends on a channel, so it
    /// never re-enters the slot).
    pub fn dispatch(&mut self, request: ExtensionUiRequest) -> bool {
        match self.bridge.clone() {
            Some(bridge) => bridge(request).is_ok(),
            None => {
                // A pathological extension cannot queue without bound.
                if self.pending.len() < 256 {
                    self.pending.push(request);
                }
                false
            }
        }
    }

    /// Mount one `ctx.ui.custom` surface, queueing it until the interactive
    /// run installs its installer.
    pub fn install_custom(&mut self, surface: ExtensionCustomSurface) -> bool {
        match self.custom.clone() {
            Some(install) => install(surface).is_ok(),
            None => {
                if self.pending_custom.len() < 256 {
                    self.pending_custom.push(surface);
                }
                false
            }
        }
    }
}

/// One event the interactive mode sends to a `ctx.ui.custom` render loop
/// (upstream the component's own `handleInput` / the TUI's resize).
pub enum ExtensionCustomEvent {
    /// A key sequence for the component.
    Input(String),
    /// The width the pump renders at.
    Resize(usize),
    /// The pump closed the component (session shutdown or a cancelled ask).
    Close,
}

/// The shared surface of one `ctx.ui.custom` component: the extension thread
/// renders into `lines` (upstream the factory's returned `Component` renders
/// on the main thread; the port splits the two, so the pump reads the last
/// painted frame).
pub struct ExtensionCustomSurface {
    /// The last frame the extension painted.
    pub lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Bumped on every paint so the pump can repaint without a channel.
    pub revision: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set when the render loop gave up; a queued mount is then skipped.
    pub closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The pump's events.
    pub events: ExtensionCustomEvents,
}

impl std::fmt::Debug for ExtensionCustomSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionCustomSurface")
            .field(
                "revision",
                &self.revision.load(std::sync::atomic::Ordering::SeqCst),
            )
            .field(
                "closed",
                &self.closed.load(std::sync::atomic::Ordering::SeqCst),
            )
            .finish()
    }
}

/// The pump's end of a custom surface. The only owner of the event sender is
/// the pump (the queued mount or the mounted component), so dropping the last
/// handle tells the render loop that the component is gone.
#[derive(Clone)]
pub struct ExtensionCustomEvents(std::sync::Arc<ExtensionCustomEventsInner>);

struct ExtensionCustomEventsInner(std::sync::mpsc::Sender<ExtensionCustomEvent>);

impl Drop for ExtensionCustomEventsInner {
    fn drop(&mut self) {
        let _ = self.0.send(ExtensionCustomEvent::Close);
    }
}

impl ExtensionCustomEvents {
    pub fn new(sender: std::sync::mpsc::Sender<ExtensionCustomEvent>) -> Self {
        Self(std::sync::Arc::new(ExtensionCustomEventsInner(sender)))
    }

    /// Send one event; `false` when the render loop is gone.
    pub fn send(&self, event: ExtensionCustomEvent) -> bool {
        self.0.0.send(event).is_ok()
    }
}

/// Host installer for `ctx.ui.custom` (upstream the mode mounting the
/// factory's component in the editor slot).
pub type ExtensionCustomFn =
    std::sync::Arc<dyn Fn(ExtensionCustomSurface) -> Result<(), String> + Send + Sync>;

/// Host bridge for the `ctx.ui` methods that answer a value (upstream
/// `confirm` / `select` / `input` / `editor`): the caller blocks until the
/// user answers.
pub type ExtensionUiAskFn =
    std::sync::Arc<dyn Fn(ExtensionUiRequest) -> Result<serde_json::Value, String> + Send + Sync>;

/// The slot the interactive run fills with its pump-backed UI bridge
/// (upstream the mode owns `ctx.ui` directly).
pub type ExtensionUiSlot = std::sync::Arc<std::sync::Mutex<ExtensionUiState>>;

/// Host callback answering the extension context facts (upstream the live
/// `ExtensionContext` fields).
pub type ExtensionContextFn = std::sync::Arc<dyn Fn() -> ExtensionContextFacts + Send + Sync>;

/// Working-indicator animation options (upstream `WorkingIndicatorOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingIndicatorOptions {
    /// Animation frames; an empty list hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds.
    pub interval_ms: Option<u64>,
}

/// Options for rendering a custom message (upstream `MessageRenderOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageRenderOptions {
    pub expanded: bool,
    /// Horizontal padding from the `outputPad` setting.
    pub output_pad: usize,
}

/// Renders a custom message (upstream `MessageRenderer`). A renderer that
/// answers `None` falls back to the default message rendering.
pub type MessageRenderer = std::sync::Arc<
    dyn Fn(
            &crate::core::messages::CustomMessage,
            &MessageRenderOptions,
            &crate::modes::interactive::theme::Theme,
        ) -> Option<Box<dyn pillar_tui::tui::Component>>
        + Send
        + Sync,
>;
