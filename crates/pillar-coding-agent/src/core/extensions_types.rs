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



// The UI bridge, the `ctx` facts, the Markdown transform, and the render
// options live in the contract crate (docs/DEVELOPMENT-STRATEGY.md §4): they
// name no session or message model, so the VM can hold them without depending
// on this crate. Re-exported here so existing paths keep working.
pub use pillar_extensions_contract::{
    EntryRenderOptions, ExtensionContextFacts, ExtensionContextFn, ExtensionCustomEvent,
    ExtensionCustomEvents, ExtensionCustomFn, ExtensionCustomSurface, ExtensionMode,
    ExtensionUiAskFn, ExtensionUiFn, ExtensionUiRequest, ExtensionUiSlot, ExtensionUiState,
    MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer, MessageRenderOptions,
    RenderedLines, WorkingIndicatorOptions,
};

// ============================================================================
// UI-facing extension types (upstream types.ts: markdown transforms, custom
// entry renderers, working-indicator options)
// ============================================================================

// The renderer aliases live in the contract crate together with the payloads
// and the theme-style lookup they take: they must not name the session, message,
// or presentation models (TASKS: the remaining stages move the runner too).
pub use pillar_extensions_contract::{
    CustomEntryPayload, CustomMessagePayload, EntryRenderer, MessageRenderer, ThemeStyle,
    ThemeStyleFn,
};

/// The payload a custom-message renderer receives, built from this crate's
/// message model (upstream hands the renderer the `CustomMessage` itself).
pub fn message_render_payload(
    message: &crate::core::messages::CustomMessage,
) -> CustomMessagePayload {
    use crate::core::messages::CustomContent;
    CustomMessagePayload {
        custom_type: message.custom_type.clone(),
        content: serde_json::Value::Array(
            message
                .content
                .iter()
                .map(|content| match content {
                    CustomContent::Text(text) => serde_json::json!({ "type": "text", "text": text }),
                    CustomContent::Image { data, mime_type } => {
                        serde_json::json!({ "type": "image", "data": data, "mimeType": mime_type })
                    }
                })
                .collect(),
        ),
        display: message.display,
        // The model keeps `details` / `timestamp` optional; the payload is the
        // Lua view, where an absent key means "no data" (see `to_json`).
        details: message.details.clone().unwrap_or(serde_json::Value::Null),
        timestamp: message.timestamp as i64,
    }
}

/// The payload a custom-entry renderer receives.
pub fn entry_render_payload(
    entry: &crate::core::session_entries::CustomEntry,
) -> CustomEntryPayload {
    CustomEntryPayload {
        custom_type: entry.custom_type.clone(),
        id: entry.base.id.clone(),
        data: entry.data.clone().unwrap_or(serde_json::Value::Null),
    }
}

/// A [`ThemeStyle`] over the live presentation theme: the adapter side of the
/// renderer contract (`custom_message` / `custom_entry` use it).
pub fn theme_style_fn() -> ThemeStyleFn {
    std::sync::Arc::new(|name: &str, text: &str| {
        crate::modes::interactive::theme::theme()
            .try_fg(name, text)
            .unwrap_or_else(|| text.to_string())
    })
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
