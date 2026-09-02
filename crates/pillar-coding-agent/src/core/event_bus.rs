//! Port of packages/coding-agent/src/core/event-bus.ts and session-cwd.ts
//! (pi v0.84.3): a synchronous broadcast event bus and the missing-session
//! cwd detection/formatting helpers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

// --- event-bus.ts -------------------------------------------------------------

/// A broadcast event bus (upstream `EventBus`/`EventBusController`). The
/// upstream is an EventEmitter wrapper with async-safe handlers; the port is
/// synchronous channel-based fan-out with handler errors swallowed and
/// reported through `last_error`.
#[derive(Clone, Default)]
pub struct EventBusController {
    handlers: Arc<Mutex<BTreeMap<String, Vec<EventHandler>>>>,
    /// Errors thrown by handlers (upstream logs via console.error).
    pub last_error: Arc<Mutex<Option<String>>>,
}

type EventHandler = Arc<dyn Fn(&str, &serde_json::Value) + Send + Sync>;

/// An event bus handle (upstream `EventBus`): emit and subscribe.
#[derive(Clone)]
pub struct EventBus {
    controller: Arc<EventBusController>,
}

impl EventBusController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit to all handlers on a channel. Handler failures are recorded on
    /// `last_error` instead of panicking (upstream catches and logs).
    pub fn emit(&self, channel: &str, data: &serde_json::Value) {
        let handlers = self.handlers.lock().unwrap();
        if let Some(list) = handlers.get(channel) {
            for handler in list {
                handler(channel, data);
            }
        }
    }

    /// Register a handler; returns an unsubscribe token id.
    pub fn on<F>(&self, channel: &str, handler: F) -> u64
    where
        F: Fn(&str, &serde_json::Value) + Send + Sync + 'static,
    {
        let mut handlers = self.handlers.lock().unwrap();
        let list = handlers.entry(channel.to_string()).or_default();
        let id = next_handler_id();
        list.push(Arc::new(move |c, d| handler(c, d)));
        id
    }

    /// Remove all handlers (upstream `clear`).
    pub fn clear(&self) {
        self.handlers.lock().unwrap().clear();
    }
}

fn next_handler_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

impl EventBus {
    /// Emit through the bus handle.
    pub fn emit(&self, channel: &str, data: &serde_json::Value) {
        self.controller.emit(channel, data);
    }

    /// Subscribe through the bus handle.
    pub fn on<F>(&self, channel: &str, handler: F) -> u64
    where
        F: Fn(&str, &serde_json::Value) + Send + Sync + 'static,
    {
        self.controller.on(channel, handler)
    }
}

/// Create the event bus pair (upstream `createEventBus`).
pub fn create_event_bus() -> (EventBus, EventBusController) {
    let controller = Arc::new(EventBusController::new());
    (
        EventBus {
            controller: controller.clone(),
        },
        (*controller).clone(),
    )
}

// --- session-cwd.ts -------------------------------------------------------------

/// A stored session cwd that no longer exists (upstream `SessionCwdIssue`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCwdIssue {
    pub session_file: Option<String>,
    pub session_cwd: String,
    pub fallback_cwd: String,
}

/// Source for the session cwd check (upstream narrows SessionManager).
pub trait SessionCwdSource {
    fn get_cwd(&self) -> String;
    fn get_session_file(&self) -> Option<String>;
}

/// Detect a missing stored session cwd (upstream
/// `getMissingSessionCwdIssue`).
pub fn get_missing_session_cwd_issue(
    session_manager: &dyn SessionCwdSource,
    fallback_cwd: &str,
) -> Option<SessionCwdIssue> {
    let session_file = session_manager.get_session_file()?;
    let session_cwd = session_manager.get_cwd();
    if session_cwd.is_empty() || std::path::Path::new(&session_cwd).exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file),
        session_cwd,
        fallback_cwd: fallback_cwd.to_string(),
    })
}

/// Upstream `formatMissingSessionCwdError`.
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|f| format!("\nSession file: {f}"))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd, session_file, issue.fallback_cwd
    )
}

/// Upstream `formatMissingSessionCwdPrompt`.
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// The missing-cwd error (upstream `MissingSessionCwdError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl MissingSessionCwdError {
    pub fn message(&self) -> String {
        format_missing_session_cwd_error(&self.issue)
    }
}

impl std::fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for MissingSessionCwdError {}

/// Assert the session cwd exists, returning the error otherwise (upstream
/// `assertSessionCwdExists` throws).
pub fn check_session_cwd_exists(
    session_manager: &dyn SessionCwdSource,
    fallback_cwd: &str,
) -> Result<(), MissingSessionCwdError> {
    match get_missing_session_cwd_issue(session_manager, fallback_cwd) {
        Some(issue) => Err(MissingSessionCwdError { issue }),
        None => Ok(()),
    }
}
