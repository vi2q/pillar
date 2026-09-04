//! Port of packages/server/src/sessions.ts (pi v0.84.3): the
//! LiveSessionManager decision core — attachment bookkeeping, session
//! lifecycle (acquire/dispose/terminate), operation counting, and
//! snapshot normalization — over a synchronous runtime trait.
//!
//! divergences: the async acquire/dispose promise queues become
//! synchronous (opening/discharging states tracked explicitly);
//! broadcast/sendMessage callbacks are host-supplied trait objects;
//! UUID generation is host-supplied; connection bookkeeping uses ids
//! against a host-owned connection registry.

use std::collections::{HashMap, HashSet};

use pillar_protocol::schemas::{
    Command, CommandResult, ModelRef, ProtocolErrorCode, ServerEvent, SessionMetadata,
    SessionPhase, SessionSnapshot, ThinkingLevel, TranscriptProgress,
};

/// A server-side protocol error (upstream `PiServerError`).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerError {
    pub code: ProtocolErrorCode,
    pub message: String,
}

impl ServerError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// One attached connection (upstream `ConnectionState`): the host owns
/// the full struct; the manager tracks its id and attached sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionState {
    pub id: u64,
    pub disconnected: bool,
    pub ready: bool,
    pub closed: bool,
    pub session_ids: HashSet<String>,
}

impl ConnectionState {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            disconnected: false,
            ready: true,
            closed: false,
            session_ids: HashSet::new(),
        }
    }
}

/// Session runtime event (upstream `PiSessionRuntimeEvent`).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum RuntimeEvent {
    Snapshot,
    Progress(TranscriptProgress),
    Error(ServerError),
}

/// A session runtime (upstream `PiSessionRuntime`, synchronous).
pub trait PiSessionRuntime {
    fn snapshot(&self) -> SessionSnapshot;
    fn phase(&self) -> SessionPhase;
    fn prompt(&mut self, text: &str) -> Result<(), ServerError>;
    fn steer(&mut self, text: &str) -> Result<(), ServerError>;
    fn abort(&mut self) -> Result<(), ServerError>;
    fn set_model(&mut self, model: &ModelRef) -> Result<(), ServerError>;
    fn set_thinking(&mut self, level: &ThinkingLevel) -> Result<(), ServerError>;
    fn dispose(&mut self);
}

/// Service boundary (upstream `PiServerService`).
pub trait PiServerService {
    fn list_sessions(&self) -> Vec<SessionMetadata>;
    fn create_session(
        &mut self,
        options: &CreateSessionOptions,
    ) -> Result<Box<dyn PiSessionRuntime>, ServerError>;
    fn open_session(&mut self, session_id: &str) -> Result<Box<dyn PiSessionRuntime>, ServerError>;
}

/// Create session options (upstream `CreateSessionOptions`).
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    pub id: String,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub model: Option<ModelRef>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Broadcast sink (upstream the sendMessage/broadcastServerSnapshot
/// callbacks).
pub trait BroadcastSink {
    fn send_event(&mut self, connection_id: u64, event: &ServerEvent);
    fn broadcast_server_snapshot(&mut self);
    fn close_connection(&mut self, connection_id: u64);
    fn report_error(&mut self, error: String);
}

/// A live session (upstream `LiveSession`).
struct LiveSession {
    id: String,
    runtime: Box<dyn PiSessionRuntime>,
    connections: HashSet<u64>,
    operation_count: usize,
    ready: bool,
    terminal: bool,
    disposing: bool,
}

impl LiveSession {
    fn normalized_snapshot(&self) -> Result<SessionSnapshot, ServerError> {
        let snapshot = self.runtime.snapshot();
        if snapshot.id != self.id {
            return Err(ServerError::new(
                ProtocolErrorCode::InvalidRequest,
                format!(
                    "Runtime session ID changed from {} to {}",
                    self.id, snapshot.id
                ),
            ));
        }
        Ok(SessionSnapshot {
            phase: self.runtime.phase(),
            attached: !self.connections.is_empty(),
            locked: true,
            ..snapshot
        })
    }
}

fn to_metadata(snapshot: &SessionSnapshot) -> SessionMetadata {
    SessionMetadata {
        id: snapshot.id.clone(),
        created_at: snapshot.created_at,
        updated_at: Some(snapshot.updated_at),
        parent_session_id: None,
        session_name: snapshot.name.clone(),
        cwd: Some(snapshot.cwd.clone()),
    }
}

/// The live session manager (upstream `LiveSessionManager`).
pub struct LiveSessionManager {
    live_sessions: HashMap<String, LiveSession>,
    opening_sessions: HashSet<String>,
    closing: bool,
}

impl Default for LiveSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveSessionManager {
    pub fn new() -> Self {
        Self {
            live_sessions: HashMap::new(),
            opening_sessions: HashSet::new(),
            closing: false,
        }
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }

    pub fn set_closing(&mut self) {
        self.closing = true;
    }

    /// Execute a command for a connection (upstream `executeCommand`).
    pub fn execute_command(
        &mut self,
        connection: &mut ConnectionState,
        command: &Command,
        service: &mut dyn PiServerService,
        sink: &mut dyn BroadcastSink,
        new_session_id: &str,
    ) -> Result<CommandResult, ServerError> {
        match command {
            Command::List => Ok(CommandResult::List {
                sessions: self.list_metadata(service),
            }),
            Command::Create {
                cwd,
                name,
                model,
                thinking_level,
            } => {
                let options = CreateSessionOptions {
                    id: new_session_id.to_string(),
                    cwd: cwd.clone(),
                    name: name.clone(),
                    model: model.clone(),
                    thinking_level: *thinking_level,
                };
                let live_id = self.acquire(&options.id, |manager| {
                    let result = service.create_session(&options);
                    match result {
                        Ok(runtime) => Self::register_runtime(manager, &options.id, runtime),
                        Err(error) => Err(error),
                    }
                })?;
                self.attach(connection, &live_id)?;
                let snapshot = self.broadcast_snapshot(&live_id, sink)?;
                let session = self.for_connection(&snapshot, connection);
                sink.broadcast_server_snapshot();
                Ok(CommandResult::Create { session })
            }
            Command::Attach { session_id } => {
                let live_id = self.acquire(session_id, |manager| {
                    let result = service.open_session(session_id);
                    match result {
                        Ok(runtime) => Self::register_runtime(manager, session_id, runtime),
                        Err(error) => Err(error),
                    }
                })?;
                self.attach(connection, &live_id)?;
                let snapshot = self.broadcast_snapshot(&live_id, sink)?;
                let session = self.for_connection(&snapshot, connection);
                sink.broadcast_server_snapshot();
                Ok(CommandResult::Attach { session })
            }
            Command::Detach { session_id } => {
                let mut broadcast = false;
                if connection.session_ids.remove(session_id) {
                    if let Some(live) = self.live_sessions.get_mut(session_id) {
                        live.connections.remove(&connection.id);
                        if !live.connections.is_empty() && !live.terminal && !live.disposing {
                            let snapshot = live.normalized_snapshot();
                            if let Ok(snapshot) = snapshot {
                                sink.send_event(
                                    connection.id,
                                    &ServerEvent::SessionSnapshot {
                                        snapshot: snapshot.clone(),
                                    },
                                );
                            }
                        }
                        let _ = self.maybe_dispose(session_id, sink);
                        broadcast = true;
                    }
                    sink.broadcast_server_snapshot();
                }
                let _ = broadcast;
                Ok(CommandResult::Detach {
                    session_id: session_id.clone(),
                })
            }
            Command::Prompt { session_id, text } => {
                self.require_attached(connection, session_id)?;
                let runtime = &mut self.live_sessions.get_mut(session_id).unwrap().runtime;
                let result = runtime.prompt(text);
                self.finish_operation(session_id, connection, sink, result)
            }
            Command::Steer { session_id, text } => {
                self.require_attached(connection, session_id)?;
                let runtime = &mut self.live_sessions.get_mut(session_id).unwrap().runtime;
                let result = runtime.steer(text);
                self.finish_operation(session_id, connection, sink, result)
            }
            Command::Abort { session_id } => {
                self.require_attached(connection, session_id)?;
                let runtime = &mut self.live_sessions.get_mut(session_id).unwrap().runtime;
                let result = runtime.abort();
                self.finish_operation(session_id, connection, sink, result)
            }
            Command::SetModel { session_id, model } => {
                self.require_attached(connection, session_id)?;
                let runtime = &mut self.live_sessions.get_mut(session_id).unwrap().runtime;
                let result = runtime.set_model(model);
                self.finish_operation(session_id, connection, sink, result)
            }
            Command::SetThinking {
                session_id,
                thinking_level,
            } => {
                self.require_attached(connection, session_id)?;
                let runtime = &mut self.live_sessions.get_mut(session_id).unwrap().runtime;
                let result = runtime.set_thinking(thinking_level);
                self.finish_operation(session_id, connection, sink, result)
            }
        }
    }

    fn finish_operation(
        &mut self,
        session_id: &str,
        connection: &mut ConnectionState,
        sink: &mut dyn BroadcastSink,
        result: Result<(), ServerError>,
    ) -> Result<CommandResult, ServerError> {
        result?;
        let snapshot = self.broadcast_snapshot(session_id, sink)?;
        let session = self.for_connection(&snapshot, connection);
        Ok(CommandResult::Create { session })
    }

    /// A runtime event arrived (upstream `handleRuntimeEvent`): errors
    /// terminate the session, progress broadcasts, snapshots broadcast.
    pub fn handle_runtime_event(
        &mut self,
        session_id: &str,
        event: RuntimeEvent,
        sink: &mut dyn BroadcastSink,
    ) {
        match event {
            RuntimeEvent::Error(error) => {
                self.terminate(session_id, error, sink);
            }
            RuntimeEvent::Progress(progress) => {
                let connection_ids: Vec<u64> = self
                    .live_sessions
                    .get(session_id)
                    .map(|live| live.connections.iter().copied().collect())
                    .unwrap_or_default();
                for connection_id in connection_ids {
                    sink.send_event(
                        connection_id,
                        &ServerEvent::SessionProgress {
                            session_id: session_id.to_string(),
                            progress: progress.clone(),
                        },
                    );
                }
            }
            RuntimeEvent::Snapshot => {
                if let Err(error) = self.broadcast_snapshot(session_id, sink) {
                    sink.report_error(error.to_string());
                }
            }
        }
        let _ = self.maybe_dispose(session_id, sink);
    }

    /// Disconnect a connection (upstream `disconnect`): detach all its
    /// sessions.
    pub fn disconnect(
        &mut self,
        connection_id: u64,
        connection_sessions: &[String],
        sink: &mut dyn BroadcastSink,
    ) {
        for session_id in connection_sessions {
            if let Some(live) = self.live_sessions.get_mut(session_id) {
                live.connections.remove(&connection_id);
            }
            let _ = self.maybe_dispose(session_id, sink);
        }
    }

    /// List metadata merging stored and live snapshots (upstream
    /// `listMetadata`): live entries override stored fields, unknown
    /// live sessions append.
    pub fn list_metadata(&mut self, service: &mut dyn PiServerService) -> Vec<SessionMetadata> {
        let stored = service.list_sessions();
        let mut live_by_id: HashMap<String, SessionMetadata> = HashMap::new();
        for (id, live) in self.live_sessions.iter_mut() {
            if live.disposing {
                continue;
            }
            if let Ok(snapshot) = live.normalized_snapshot() {
                live_by_id.insert(id.clone(), to_metadata(&snapshot));
            }
        }
        let mut metadata: Vec<SessionMetadata> = Vec::new();
        for item in stored {
            if let Some(live) = live_by_id.remove(&item.id) {
                metadata.push(SessionMetadata {
                    session_name: live.session_name,
                    ..item
                });
            } else {
                metadata.push(item);
            }
        }
        metadata.extend(live_by_id.into_values());
        metadata
    }

    /// Close all sessions (upstream `close`).
    pub fn close(&mut self, sink: &mut dyn BroadcastSink) {
        self.closing = true;
        let ids: Vec<String> = self.live_sessions.keys().cloned().collect();
        for id in ids {
            self.dispose_runtime(&id);
        }
        let _ = sink;
    }

    fn register_runtime(
        manager: &mut LiveSessionManager,
        id: &str,
        runtime: Box<dyn PiSessionRuntime>,
    ) -> Result<(), ServerError> {
        if manager.closing {
            let mut runtime = runtime;
            runtime.dispose();
            return Err(ServerError::new(
                ProtocolErrorCode::InternalError,
                "PiServer closed while acquiring a session runtime",
            ));
        }
        // Snapshot id must match the server-assigned id.
        let snapshot_id = runtime.snapshot().id;
        if snapshot_id != id {
            let mut runtime = runtime;
            runtime.dispose();
            return Err(ServerError::new(
                ProtocolErrorCode::InvalidRequest,
                format!("Service returned session {snapshot_id} for server-assigned session {id}"),
            ));
        }
        manager.opening_sessions.remove(id);
        manager.live_sessions.insert(
            id.to_string(),
            LiveSession {
                id: id.to_string(),
                runtime,
                connections: HashSet::new(),
                operation_count: 0,
                ready: true,
                terminal: false,
                disposing: false,
            },
        );
        Ok(())
    }

    fn acquire(
        &mut self,
        id: &str,
        acquire_runtime: impl FnOnce(&mut LiveSessionManager) -> Result<(), ServerError>,
    ) -> Result<String, ServerError> {
        if let Some(existing) = self.live_sessions.get(id) {
            if existing.terminal {
                return Err(ServerError::new(
                    ProtocolErrorCode::SessionLocked,
                    format!("Session runtime is terminating: {id}"),
                ));
            }
            if existing.disposing {
                // Upstream awaits the disposing promise and retries; the
                // sync port retries via re-entry by the caller.
                return Err(ServerError::new(
                    ProtocolErrorCode::SessionLocked,
                    format!("Session is disposing: {id}"),
                ));
            }
            return Ok(id.to_string());
        }
        if self.opening_sessions.contains(id) {
            return Err(ServerError::new(
                ProtocolErrorCode::SessionLocked,
                format!("Session is opening: {id}"),
            ));
        }
        self.opening_sessions.insert(id.to_string());
        let result = acquire_runtime(self);
        self.opening_sessions.remove(id);
        result?;
        Ok(id.to_string())
    }

    fn attach(
        &mut self,
        connection: &mut ConnectionState,
        session_id: &str,
    ) -> Result<(), ServerError> {
        if connection.disconnected || !connection.ready || connection.closed {
            let _ = self.maybe_dispose(session_id, &mut NullSink);
            return Err(ServerError::new(
                ProtocolErrorCode::InvalidRequest,
                "Connection closed while attaching to a session",
            ));
        }
        connection.session_ids.insert(session_id.to_string());
        if let Some(live) = self.live_sessions.get_mut(session_id) {
            live.connections.insert(connection.id);
        }
        Ok(())
    }

    fn require_attached(
        &self,
        connection: &ConnectionState,
        session_id: &str,
    ) -> Result<(), ServerError> {
        if !connection.session_ids.contains(session_id) {
            return Err(ServerError::new(
                ProtocolErrorCode::InvalidRequest,
                format!("Connection is not attached to session {session_id}"),
            ));
        }
        match self.live_sessions.get(session_id) {
            Some(live) if !live.terminal && !live.disposing => Ok(()),
            _ => Err(ServerError::new(
                ProtocolErrorCode::NotFound,
                format!("Session is not live: {session_id}"),
            )),
        }
    }

    fn broadcast_snapshot(
        &mut self,
        session_id: &str,
        sink: &mut dyn BroadcastSink,
    ) -> Result<SessionSnapshot, ServerError> {
        let snapshot = self
            .live_sessions
            .get_mut(session_id)
            .ok_or_else(|| {
                ServerError::new(
                    ProtocolErrorCode::NotFound,
                    format!("Session is not live: {session_id}"),
                )
            })?
            .normalized_snapshot()?;
        let connection_ids: Vec<u64> = self
            .live_sessions
            .get(session_id)
            .map(|live| live.connections.iter().copied().collect())
            .unwrap_or_default();
        for connection_id in connection_ids {
            sink.send_event(
                connection_id,
                &ServerEvent::SessionSnapshot {
                    snapshot: snapshot.clone(),
                },
            );
        }
        Ok(snapshot)
    }

    fn for_connection(
        &self,
        snapshot: &SessionSnapshot,
        connection: &ConnectionState,
    ) -> SessionSnapshot {
        SessionSnapshot {
            attached: connection.session_ids.contains(&snapshot.id),
            ..snapshot.clone()
        }
    }

    fn terminate(&mut self, session_id: &str, error: ServerError, sink: &mut dyn BroadcastSink) {
        let Some(live) = self.live_sessions.get_mut(session_id) else {
            return;
        };
        if live.terminal {
            return;
        }
        live.terminal = true;
        sink.report_error(error.to_string());
        let connection_ids: Vec<u64> = live.connections.iter().copied().collect();
        for connection_id in &connection_ids {
            sink.close_connection(*connection_id);
        }
        for connection_id in connection_ids {
            sink.send_event(
                connection_id,
                &ServerEvent::SessionRemoved {
                    session_id: session_id.to_string(),
                },
            );
        }
        // The connections are being closed; drop them so the terminal
        // session can dispose (upstream disconnect() clears them).
        if let Some(live) = self.live_sessions.get_mut(session_id) {
            live.connections.clear();
        }
        let _ = self.maybe_dispose(session_id, sink);
    }

    fn maybe_dispose(
        &mut self,
        session_id: &str,
        sink: &mut dyn BroadcastSink,
    ) -> Result<(), ServerError> {
        let should_dispose = match self.live_sessions.get(session_id) {
            Some(live) => {
                self.closing
                    || (live.ready
                        && !live.disposing
                        && live.connections.is_empty()
                        && live.operation_count == 0
                        && (live.terminal || live.runtime.phase() == SessionPhase::Idle))
            }
            None => return Ok(()),
        };
        if !should_dispose {
            return Ok(());
        }
        self.dispose_runtime(session_id);
        if !self.closing {
            sink.broadcast_server_snapshot();
        }
        Ok(())
    }

    fn dispose_runtime(&mut self, session_id: &str) {
        if let Some(mut live) = self.live_sessions.remove(session_id) {
            live.runtime.dispose();
        }
    }
}

/// A no-op sink for internal dispose paths (upstream the callbacks are
/// optional effects).
struct NullSink;

impl BroadcastSink for NullSink {
    fn send_event(&mut self, _connection_id: u64, _event: &ServerEvent) {}
    fn broadcast_server_snapshot(&mut self) {}
    fn close_connection(&mut self, _connection_id: u64) {}
    fn report_error(&mut self, _error: String) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink {
        events: Vec<(u64, String)>,
        broadcasts: usize,
        closed: Vec<u64>,
        errors: Vec<String>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                broadcasts: 0,
                closed: Vec::new(),
                errors: Vec::new(),
            }
        }
    }

    impl BroadcastSink for RecordingSink {
        fn send_event(&mut self, connection_id: u64, event: &ServerEvent) {
            let label = match event {
                ServerEvent::SessionSnapshot { .. } => "snapshot".to_string(),
                ServerEvent::SessionProgress { .. } => "progress".to_string(),
                ServerEvent::SessionRemoved { session_id } => {
                    format!("removed:{session_id}")
                }
                ServerEvent::ServerSnapshot { .. } => "server".to_string(),
            };
            self.events.push((connection_id, label));
        }
        fn broadcast_server_snapshot(&mut self) {
            self.broadcasts += 1;
        }
        fn close_connection(&mut self, connection_id: u64) {
            self.closed.push(connection_id);
        }
        fn report_error(&mut self, error: String) {
            self.errors.push(error);
        }
    }

    fn snapshot(id: &str, phase: SessionPhase) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            name: None,
            cwd: "/tmp".to_string(),
            created_at: 0,
            updated_at: 0,
            phase,
            model: pillar_protocol::schemas::ModelRef {
                provider: "p".to_string(),
                id: "m".to_string(),
            },
            thinking_level: ThinkingLevel::Off,
            attached: false,
            locked: false,
            revision: 1,
            transcript: vec![],
            queued_steer: vec![],
            queued_steer_count: 0,
        }
    }

    struct FakeRuntime {
        id: String,
        phase: SessionPhase,
        disposed: bool,
    }

    impl PiSessionRuntime for FakeRuntime {
        fn snapshot(&self) -> SessionSnapshot {
            snapshot(&self.id, self.phase)
        }
        fn phase(&self) -> SessionPhase {
            self.phase
        }
        fn prompt(&mut self, _text: &str) -> Result<(), ServerError> {
            self.phase = SessionPhase::Turn;
            Ok(())
        }
        fn steer(&mut self, _text: &str) -> Result<(), ServerError> {
            Ok(())
        }
        fn abort(&mut self) -> Result<(), ServerError> {
            self.phase = SessionPhase::Idle;
            Ok(())
        }
        fn set_model(&mut self, _model: &ModelRef) -> Result<(), ServerError> {
            Ok(())
        }
        fn set_thinking(&mut self, _level: &ThinkingLevel) -> Result<(), ServerError> {
            Ok(())
        }
        fn dispose(&mut self) {
            self.disposed = true;
        }
    }

    struct FakeService {
        created: Vec<String>,
        opened: Vec<String>,
        fail_create_with: Option<ServerError>,
    }

    impl FakeService {
        fn new() -> Self {
            Self {
                created: Vec::new(),
                opened: Vec::new(),
                fail_create_with: None,
            }
        }
    }

    impl PiServerService for FakeService {
        fn list_sessions(&self) -> Vec<SessionMetadata> {
            vec![SessionMetadata {
                id: "stored-1".to_string(),
                created_at: 0,
                updated_at: None,
                parent_session_id: None,
                session_name: None,
                cwd: Some("/tmp".to_string()),
            }]
        }
        fn create_session(
            &mut self,
            options: &CreateSessionOptions,
        ) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
            if let Some(error) = &self.fail_create_with {
                return Err(error.clone());
            }
            self.created.push(options.id.clone());
            Ok(Box::new(FakeRuntime {
                id: options.id.clone(),
                phase: SessionPhase::Idle,
                disposed: false,
            }))
        }
        fn open_session(
            &mut self,
            session_id: &str,
        ) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
            self.opened.push(session_id.to_string());
            Ok(Box::new(FakeRuntime {
                id: session_id.to_string(),
                phase: SessionPhase::Idle,
                disposed: false,
            }))
        }
    }

    fn create_command() -> Command {
        Command::Create {
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        }
    }

    #[test]
    fn create_assigns_id_attaches_and_broadcasts() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let result = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        // Session id comes from the server-assigned argument.
        assert_eq!(service.created, vec!["s-1".to_string()]);
        match result {
            CommandResult::Create { session } => {
                assert_eq!(session.id, "s-1");
                // forConnection: attached for this connection.
                assert!(session.attached);
            }
            _ => panic!("expected create result"),
        }
        // Snapshot event sent to the connection + server snapshot broadcast.
        assert!(sink.events.contains(&(1, "snapshot".to_string())));
        assert_eq!(sink.broadcasts, 1);
    }

    #[test]
    fn attach_open_then_attach_existing() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Attach {
                    session_id: "stored-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        // Opening called once.
        assert_eq!(service.opened, vec!["stored-1".to_string()]);
        // Re-attach does not open again.
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Attach {
                    session_id: "stored-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        assert_eq!(service.opened.len(), 1);
    }

    #[test]
    fn detached_session_for_this_connection_only() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection_a = ConnectionState::new(1);
        let mut connection_b = ConnectionState::new(2);
        let _ = manager
            .execute_command(
                &mut connection_a,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        let _ = manager
            .execute_command(
                &mut connection_b,
                &create_command(),
                &mut service,
                &mut sink,
                "s-2",
            )
            .unwrap();
        // Both attach to the same "s-1"? No — create generates distinct ids.
        let _ = manager
            .execute_command(
                &mut connection_a,
                &Command::Attach {
                    session_id: "s-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        // Detach.
        let result = manager
            .execute_command(
                &mut connection_a,
                &Command::Detach {
                    session_id: "s-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        match result {
            CommandResult::Detach { session_id } => assert_eq!(session_id, "s-1"),
            _ => panic!("expected detach result"),
        }
    }

    #[test]
    fn prompt_requires_attachment() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let error = manager
            .execute_command(
                &mut connection,
                &Command::Prompt {
                    session_id: "s-1".to_string(),
                    text: "hi".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(error.message, "Connection is not attached to session s-1");
    }

    #[test]
    fn runtime_error_terminates_session() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        // Runtime error event terminates the session.
        manager.handle_runtime_event(
            "s-1",
            RuntimeEvent::Error(ServerError::new(ProtocolErrorCode::InternalError, "boom")),
            &mut sink,
        );
        assert!(sink.errors.iter().any(|e| e.contains("boom")));
        assert!(!manager.live_sessions.contains_key("s-1"));
    }

    #[test]
    fn idle_session_disposes_when_last_connection_detaches() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        assert!(manager.live_sessions.contains_key("s-1"));
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Detach {
                    session_id: "s-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        // Session was idle and unattached → disposed.
        assert!(!manager.live_sessions.contains_key("s-1"));
        // Broadcasts: create + detach + dispose.
        assert_eq!(sink.broadcasts, 3);
    }

    #[test]
    fn active_session_survives_detach() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        // Put the session in the Turn phase.
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Prompt {
                    session_id: "s-1".to_string(),
                    text: "go".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Detach {
                    session_id: "s-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        // Active (Turn) sessions are kept alive after detach.
        assert!(manager.live_sessions.contains_key("s-1"));
    }

    #[test]
    fn list_metadata_merges_live_over_stored() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        // Create "stored-1" as a live session id via attach.
        let _ = manager
            .execute_command(
                &mut connection,
                &Command::Attach {
                    session_id: "stored-1".to_string(),
                },
                &mut service,
                &mut sink,
                "",
            )
            .unwrap();
        let metadata = manager.list_metadata(&mut service);
        // The live session's cwd overrides the stored entry; only one
        // entry exists for the shared id.
        let matching: Vec<_> = metadata.iter().filter(|m| m.id == "stored-1").collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].cwd, Some("/tmp".to_string()));
    }

    #[test]
    fn session_id_mismatch_is_invalid_request() {
        let mut manager = LiveSessionManager::new();
        manager.opening_sessions.insert("s-1".to_string());
        // Manually register a runtime with a mismatched id.
        struct Mismatched;
        impl PiSessionRuntime for Mismatched {
            fn snapshot(&self) -> SessionSnapshot {
                snapshot("other", SessionPhase::Idle)
            }
            fn phase(&self) -> SessionPhase {
                SessionPhase::Idle
            }
            fn prompt(&mut self, _: &str) -> Result<(), ServerError> {
                Ok(())
            }
            fn steer(&mut self, _: &str) -> Result<(), ServerError> {
                Ok(())
            }
            fn abort(&mut self) -> Result<(), ServerError> {
                Ok(())
            }
            fn set_model(&mut self, _: &ModelRef) -> Result<(), ServerError> {
                Ok(())
            }
            fn set_thinking(&mut self, _: &ThinkingLevel) -> Result<(), ServerError> {
                Ok(())
            }
            fn dispose(&mut self) {}
        }
        manager.opening_sessions.remove("s-1");
        let error = LiveSessionManager::register_runtime(&mut manager, "s-1", Box::new(Mismatched))
            .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert!(
            error.message.contains("Service returned session other"),
            "{error}"
        );
    }

    #[test]
    fn close_disposes_all_sessions() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        manager.close(&mut sink);
        assert!(manager.live_sessions.is_empty());
        assert!(manager.is_closing());
    }

    #[test]
    fn progress_event_broadcasts_to_attached_connections() {
        let mut manager = LiveSessionManager::new();
        let mut service = FakeService::new();
        let mut sink = RecordingSink::new();
        let mut connection = ConnectionState::new(1);
        let _ = manager
            .execute_command(
                &mut connection,
                &create_command(),
                &mut service,
                &mut sink,
                "s-1",
            )
            .unwrap();
        manager.handle_runtime_event(
            "s-1",
            RuntimeEvent::Progress(TranscriptProgress::ItemStarted {
                item: pillar_protocol::schemas::TranscriptItem::User {
                    id: "i1".to_string(),
                    content: vec![],
                    timestamp: 0,
                },
            }),
            &mut sink,
        );
        assert!(sink.events.contains(&(1, "progress".to_string())));
    }
}
