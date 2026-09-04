//! Port of packages/client/src (pi v0.84.3): client-side session state
//! with snapshot revision tracking, attachment bookkeeping, and
//! listener fan-out (state.ts), plus the error taxonomy (errors.ts).
//!
//! divergences: the connection/transport layer (connection.ts, unix
//! socket plumbing, promise resolvers) is host-side; the port exposes
//! the pure client state machine over pillar-protocol types.

use std::collections::{HashMap, HashSet};

use pillar_protocol::schemas::{
    CommandResult, ProtocolError, ProtocolErrorCode, ServerEvent, ServerSnapshot, SessionSnapshot,
};

// ============================================================================
// Error taxonomy (upstream errors.ts)
// ============================================================================

/// A server-reported protocol error (upstream `PiServerError`).
#[derive(Debug, Clone, PartialEq)]
pub struct PiServerError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl PiServerError {
    pub fn from_protocol(error: ProtocolError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            details: error.details,
        }
    }
}

impl std::fmt::Display for PiServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiServerError {}

/// The client is disconnected (upstream `PiDisconnectedError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiDisconnectedError {
    pub message: String,
}

impl Default for PiDisconnectedError {
    fn default() -> Self {
        Self {
            message: "Pi client is disconnected".to_string(),
        }
    }
}

impl std::fmt::Display for PiDisconnectedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiDisconnectedError {}

/// The client is disposed (upstream `PiClientDisposedError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiClientDisposedError;

impl std::fmt::Display for PiClientDisposedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pi client is disposed")
    }
}

impl std::error::Error for PiClientDisposedError {}

/// Session ownership violation (upstream `PiSessionOwnershipError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSessionOwnershipError {
    pub session_id: String,
    pub message: String,
}

impl std::fmt::Display for PiSessionOwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiSessionOwnershipError {}

/// Session not attached (upstream `PiSessionDetachedError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSessionDetachedError {
    pub session_id: String,
    pub message: String,
}

impl PiSessionDetachedError {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            message: format!("Session {session_id} is not attached"),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl std::fmt::Display for PiSessionDetachedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiSessionDetachedError {}

// ============================================================================
// Client state (upstream state.ts)
// ============================================================================

/// A snapshot/event listener handle. Removal is done via
/// [`ClientState::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(u64);

/// Server-snapshot listener (upstream the snapshot listener set).
pub type SnapshotListener = (ListenerId, Box<dyn Fn(&ServerSnapshot) + Send>);
/// Server-event listener (upstream the event listener set).
pub type EventListener = (ListenerId, Box<dyn Fn(&ServerEvent) + Send>);
/// Session-snapshot listener (upstream the mapped session listeners).
pub type SessionSnapshotListener = (ListenerId, Box<dyn Fn(&SessionSnapshot) + Send>);

/// Client-side session state (upstream `ClientState`): snapshots with
/// revision gating, attachment set, and listener fan-out.
pub struct ClientState {
    server_snapshot: Option<ServerSnapshot>,
    session_snapshots: HashMap<String, SessionSnapshot>,
    attached_session_ids: HashSet<String>,
    snapshot_listeners: Vec<SnapshotListener>,
    event_listeners: Vec<EventListener>,
    session_snapshot_listeners: HashMap<String, Vec<SessionSnapshotListener>>,
    session_event_listeners: HashMap<String, Vec<EventListener>>,
    next_listener_id: u64,
    /// Errors thrown by listeners are collected (upstream
    /// onListenerError hook).
    pub listener_errors: Vec<String>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientState {
    pub fn new() -> Self {
        Self {
            server_snapshot: None,
            session_snapshots: HashMap::new(),
            attached_session_ids: HashSet::new(),
            snapshot_listeners: Vec::new(),
            event_listeners: Vec::new(),
            session_snapshot_listeners: HashMap::new(),
            session_event_listeners: HashMap::new(),
            next_listener_id: 1,
            listener_errors: Vec::new(),
        }
    }

    pub fn server_snapshot(&self) -> Option<&ServerSnapshot> {
        self.server_snapshot.as_ref()
    }

    /// Clear snapshots and attachments (upstream `reset`).
    pub fn reset(&mut self) {
        self.server_snapshot = None;
        self.session_snapshots.clear();
        self.attached_session_ids.clear();
    }

    /// Clear the attachment set only (upstream `clearAttachments`).
    pub fn clear_attachments(&mut self) {
        self.attached_session_ids.clear();
    }

    /// Reset + drop all listeners (upstream `dispose`).
    pub fn dispose(&mut self) {
        self.reset();
        self.snapshot_listeners.clear();
        self.event_listeners.clear();
        self.session_snapshot_listeners.clear();
        self.session_event_listeners.clear();
    }

    pub fn get_session_snapshot(&self, session_id: &str) -> Option<&SessionSnapshot> {
        self.session_snapshots.get(session_id)
    }

    pub fn is_session_attached(&self, session_id: &str) -> bool {
        self.attached_session_ids.contains(session_id)
    }

    /// Remove and return a session snapshot (upstream
    /// `forgetSessionSnapshot`).
    pub fn forget_session_snapshot(&mut self, session_id: &str) -> Option<SessionSnapshot> {
        self.session_snapshots.remove(session_id)
    }

    /// Restore a snapshot only when the session is unknown (upstream
    /// `restoreSessionSnapshot`).
    pub fn restore_session_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.session_snapshots
            .entry(snapshot.id.clone())
            .or_insert(snapshot);
    }

    pub fn subscribe(&mut self, listener: Box<dyn Fn(&ServerSnapshot) + Send>) -> ListenerId {
        self.next_listener_id += 1;
        let id = ListenerId(self.next_listener_id);
        self.snapshot_listeners.push((id, listener));
        id
    }

    pub fn on_event(&mut self, listener: Box<dyn Fn(&ServerEvent) + Send>) -> ListenerId {
        self.next_listener_id += 1;
        let id = ListenerId(self.next_listener_id);
        self.event_listeners.push((id, listener));
        id
    }

    pub fn subscribe_session(
        &mut self,
        session_id: &str,
        listener: Box<dyn Fn(&SessionSnapshot) + Send>,
    ) -> ListenerId {
        self.next_listener_id += 1;
        let id = ListenerId(self.next_listener_id);
        self.session_snapshot_listeners
            .entry(session_id.to_string())
            .or_default()
            .push((id, listener));
        id
    }

    pub fn on_session_event(
        &mut self,
        session_id: &str,
        listener: Box<dyn Fn(&ServerEvent) + Send>,
    ) -> ListenerId {
        self.next_listener_id += 1;
        let id = ListenerId(self.next_listener_id);
        self.session_event_listeners
            .entry(session_id.to_string())
            .or_default()
            .push((id, listener));
        id
    }

    /// Remove a listener by handle (upstream the Unsubscribe closures).
    pub fn unsubscribe(&mut self, id: ListenerId) {
        self.snapshot_listeners.retain(|(lid, _)| lid != &id);
        self.event_listeners.retain(|(lid, _)| lid != &id);
        for listeners in self.session_snapshot_listeners.values_mut() {
            listeners.retain(|(lid, _)| lid != &id);
        }
        for listeners in self.session_event_listeners.values_mut() {
            listeners.retain(|(lid, _)| lid != &id);
        }
        self.session_snapshot_listeners.retain(|_, l| !l.is_empty());
        self.session_event_listeners.retain(|_, l| !l.is_empty());
    }

    /// Apply a command result to the state (upstream `applyResult`):
    /// list results are ignored, detach drops the attachment and marks
    /// the snapshot detached, other results update the session.
    pub fn apply_result(&mut self, result: &CommandResult) {
        match result {
            CommandResult::List { .. } => {}
            CommandResult::Detach { session_id } => {
                self.attached_session_ids.remove(session_id);
                if let Some(snapshot) = self.session_snapshots.get(session_id) {
                    let mut detached = snapshot.clone();
                    detached.attached = false;
                    self.apply_session_snapshot(&detached, true);
                }
            }
            CommandResult::Create { session } | CommandResult::Attach { session } => {
                self.apply_session_snapshot(session, false);
            }
            CommandResult::Prompt { session }
            | CommandResult::Steer { session }
            | CommandResult::Abort { session }
            | CommandResult::SetModel { session }
            | CommandResult::SetThinking { session } => {
                self.apply_session_snapshot(session, false);
            }
        }
    }

    /// Apply a server event (upstream `applyEvent`).
    pub fn apply_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::ServerSnapshot { snapshot } => {
                self.apply_server_snapshot(snapshot);
            }
            ServerEvent::SessionSnapshot { snapshot } => {
                self.apply_session_snapshot(snapshot, false);
            }
            ServerEvent::SessionRemoved { session_id } => {
                self.session_snapshots.remove(session_id);
                self.attached_session_ids.remove(session_id);
            }
            ServerEvent::SessionProgress { .. } => {}
        }
        let event_listeners: Vec<_> = self
            .event_listeners
            .iter()
            .map(|(_, l)| l as &(dyn Fn(&ServerEvent) + Send))
            .collect();
        self.notify_events(event_listeners.into_iter(), event);
        if let Some(session_id) = event_session_id(event) {
            let session_listeners: Vec<_> = self
                .session_event_listeners
                .get(&session_id)
                .map(|listeners| {
                    listeners
                        .iter()
                        .map(|(_, l)| l as &(dyn Fn(&ServerEvent) + Send))
                        .collect()
                })
                .unwrap_or_default();
            self.notify_events(session_listeners.into_iter(), event);
        }
    }

    /// Apply a server snapshot, ignoring stale revisions (upstream
    /// `applyServerSnapshot`).
    pub fn apply_server_snapshot(&mut self, snapshot: &ServerSnapshot) {
        if self
            .server_snapshot
            .as_ref()
            .is_some_and(|current| snapshot.revision < current.revision)
        {
            return;
        }
        self.server_snapshot = Some(snapshot.clone());
    }

    fn apply_session_snapshot(&mut self, snapshot: &SessionSnapshot, force: bool) {
        if !force
            && self
                .session_snapshots
                .get(&snapshot.id)
                .is_some_and(|current| snapshot.revision < current.revision)
        {
            return;
        }
        if snapshot.attached {
            self.attached_session_ids.insert(snapshot.id.clone());
        } else {
            self.attached_session_ids.remove(&snapshot.id);
        }
        self.session_snapshots
            .insert(snapshot.id.clone(), snapshot.clone());
        if let Some(listeners) = self.session_snapshot_listeners.get(&snapshot.id) {
            let listeners = listeners
                .iter()
                .map(|(_, l)| l.as_ref())
                .collect::<Vec<_>>();
            for listener in listeners {
                listener(snapshot);
            }
        }
    }

    fn notify_events<'a>(
        &self,
        listeners: impl Iterator<Item = &'a (dyn Fn(&ServerEvent) + Send + 'a)>,
        event: &ServerEvent,
    ) {
        for listener in listeners {
            listener(event);
        }
    }
}

fn event_session_id(event: &ServerEvent) -> Option<String> {
    match event {
        ServerEvent::SessionSnapshot { snapshot } => Some(snapshot.id.clone()),
        ServerEvent::SessionProgress { session_id, .. }
        | ServerEvent::SessionRemoved { session_id } => Some(session_id.clone()),
        ServerEvent::ServerSnapshot { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, revision: u64, attached: bool) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            name: None,
            cwd: "/tmp".to_string(),
            created_at: 0,
            updated_at: 0,
            phase: pillar_protocol::schemas::SessionPhase::Idle,
            model: pillar_protocol::schemas::ModelRef {
                provider: "p".to_string(),
                id: "m".to_string(),
            },
            thinking_level: pillar_protocol::schemas::ThinkingLevel::Off,
            attached,
            locked: false,
            revision,
            transcript: vec![],
            queued_steer: vec![],
            queued_steer_count: 0,
        }
    }

    #[test]
    fn server_snapshot_rejects_stale_revisions() {
        let mut state = ClientState::new();
        let fresh = ServerSnapshot {
            server_id: "s".to_string(),
            protocol_version: 1,
            revision: 5,
            sessions: vec![],
            models: vec![],
        };
        state.apply_server_snapshot(&fresh);
        let mut stale = fresh.clone();
        stale.revision = 3;
        state.apply_server_snapshot(&stale);
        assert_eq!(state.server_snapshot().unwrap().revision, 5);
        // Equal or newer revisions apply.
        stale.revision = 5;
        state.apply_server_snapshot(&stale);
        let mut newer = fresh.clone();
        newer.revision = 6;
        state.apply_server_snapshot(&newer);
        assert_eq!(state.server_snapshot().unwrap().revision, 6);
    }

    #[test]
    fn session_snapshot_rejects_stale_revisions_unless_forced() {
        let mut state = ClientState::new();
        state.apply_session_snapshot(&snapshot("a", 5, true), false);
        state.apply_session_snapshot(&snapshot("a", 3, false), false);
        assert_eq!(state.get_session_snapshot("a").unwrap().revision, 5);
        state.apply_session_snapshot(&snapshot("a", 3, false), true);
        assert_eq!(state.get_session_snapshot("a").unwrap().revision, 3);
        assert!(!state.is_session_attached("a"));
    }

    #[test]
    fn apply_result_tracks_attachments() {
        let mut state = ClientState::new();
        state.apply_result(&CommandResult::Attach {
            session: snapshot("a", 1, true),
        });
        assert!(state.is_session_attached("a"));
        state.apply_result(&CommandResult::Detach {
            session_id: "a".to_string(),
        });
        assert!(!state.is_session_attached("a"));
        // The snapshot is marked detached.
        assert!(!state.get_session_snapshot("a").unwrap().attached);
    }

    #[test]
    fn list_results_are_ignored() {
        let mut state = ClientState::new();
        state.apply_result(&CommandResult::List { sessions: vec![] });
        assert!(state.server_snapshot().is_none());
        assert!(state.session_snapshots.is_empty());
    }

    #[test]
    fn events_update_snapshots_and_attachments() {
        let mut state = ClientState::new();
        state.apply_event(&ServerEvent::SessionSnapshot {
            snapshot: snapshot("a", 1, true),
        });
        assert!(state.is_session_attached("a"));
        state.apply_event(&ServerEvent::SessionRemoved {
            session_id: "a".to_string(),
        });
        assert!(state.get_session_snapshot("a").is_none());
        assert!(!state.is_session_attached("a"));
    }

    #[test]
    fn listeners_receive_events_and_are_removable() {
        let mut state = ClientState::new();
        let received: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink = received.clone();
        let handle = state.on_event(Box::new(move |event| {
            if let ServerEvent::SessionRemoved { session_id } = event {
                sink.lock().unwrap().push(session_id.clone());
            }
        }));
        state.apply_event(&ServerEvent::SessionRemoved {
            session_id: "a".to_string(),
        });
        state.unsubscribe(handle);
        state.apply_event(&ServerEvent::SessionRemoved {
            session_id: "b".to_string(),
        });
        assert_eq!(received.lock().unwrap().as_slice(), ["a"]);
    }

    #[test]
    fn session_listeners_are_scoped_and_cleaned_up() {
        let mut state = ClientState::new();
        let received: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let sink = received.clone();
        let handle = state.subscribe_session(
            "a",
            Box::new(move |snapshot| {
                sink.lock().unwrap().push(snapshot.revision);
            }),
        );
        state.apply_event(&ServerEvent::SessionSnapshot {
            snapshot: snapshot("a", 2, false),
        });
        state.apply_event(&ServerEvent::SessionSnapshot {
            snapshot: snapshot("b", 9, false),
        });
        assert_eq!(received.lock().unwrap().as_slice(), [2]);
        // Removing the last session listener drops the map entry.
        state.unsubscribe(handle);
        state.apply_event(&ServerEvent::SessionSnapshot {
            snapshot: snapshot("a", 3, false),
        });
        assert_eq!(received.lock().unwrap().as_slice(), [2]);
    }

    #[test]
    fn restore_only_fills_unknown_sessions() {
        let mut state = ClientState::new();
        state.apply_session_snapshot(&snapshot("a", 7, false), false);
        // Restore with an older revision is ignored for a known session.
        state.restore_session_snapshot(snapshot("a", 3, true));
        assert_eq!(state.get_session_snapshot("a").unwrap().revision, 7);
        // Unknown sessions are restored.
        state.restore_session_snapshot(snapshot("b", 1, false));
        assert!(state.get_session_snapshot("b").is_some());
    }

    #[test]
    fn reset_and_clear_attachments() {
        let mut state = ClientState::new();
        state.apply_session_snapshot(&snapshot("a", 1, true), false);
        state.reset();
        assert!(state.get_session_snapshot("a").is_none());
        state.apply_session_snapshot(&snapshot("a", 1, true), false);
        state.clear_attachments();
        assert!(!state.is_session_attached("a"));
        assert!(state.get_session_snapshot("a").is_some());
    }

    #[test]
    fn dispose_drops_listeners() {
        let mut state = ClientState::new();
        let received: std::sync::Arc<std::sync::Mutex<usize>> = Default::default();
        let sink = received.clone();
        let _handle = state.subscribe(Box::new(move |_| {
            *sink.lock().unwrap() += 1;
        }));
        state.dispose();
        state.apply_server_snapshot(&ServerSnapshot {
            server_id: "s".to_string(),
            protocol_version: 1,
            revision: 1,
            sessions: vec![],
            models: vec![],
        });
        assert_eq!(*received.lock().unwrap(), 0);
    }

    #[test]
    fn error_taxonomy_messages() {
        let err = PiDisconnectedError::default();
        assert_eq!(err.message, "Pi client is disconnected");
        let detached = PiSessionDetachedError::new("abc");
        assert_eq!(detached.session_id(), "abc");
        assert_eq!(detached.to_string(), "Session abc is not attached");
        let server = PiServerError::from_protocol(ProtocolError {
            code: ProtocolErrorCode::SessionLocked,
            message: "locked".to_string(),
            details: None,
        });
        assert_eq!(server.code, ProtocolErrorCode::SessionLocked);
        assert_eq!(server.to_string(), "locked");
    }
}
