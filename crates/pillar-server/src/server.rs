//! Port of packages/server/src/server.ts (pi v0.84.3): the PiServer
//! connection state machine — handshake staging (awaitingHello →
//! handshaking → ready), protocol version validation, request
//! dispatch, protocol-failure framing, and connection teardown.
//!
//! divergences: the byte transports/listeners, UUID generation, and
//! handshake timeout timers are host-driven (the timeout becomes a
//! deadline the host enforces); messages accumulate in drainable vecs
//! instead of writing straight through a socket.

use pillar_protocol::codec::ClientMessageDecoder;
use pillar_protocol::codec::encode_server_message;
use pillar_protocol::framing::FrameDecoderOptions;
use pillar_protocol::schemas::{
    ClientMessage, Command, CommandResult, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    RequestEnvelope, ServerMessage, is_supported_protocol_version, parse_client_message,
};

use crate::sessions::{
    BroadcastSink, ConnectionState, LiveSessionManager, PiServerService, ServerError,
};
use crate::snapshots::ServerSnapshotPublisher;

const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

/// Connection stage (upstream `stage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    AwaitingHello,
    Handshaking,
    Ready,
    Closing,
    Closed,
}

/// A server-side connection (upstream `ConnectionState` with its
/// decoder).
/// Server-side durable id allocation (upstream uuidv7()): a
/// time-ordered id without external crates.
fn allocate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("session-{nanos:x}-{counter:04x}")
}

static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct ServerConnection {
    pub id: String,
    pub stage: Stage,
    pub disconnected: bool,
    pub handshake_complete: bool,
    pub session_ids: std::collections::HashSet<String>,
    decoder: ClientMessageDecoder,
    /// The pending hello version for the handshake.
    hello_version: Option<f64>,
    /// Timeout deadline the host should enforce (upstream the
    /// setTimeout).
    pub handshake_deadline_ms: u64,
}

impl ServerConnection {
    pub fn new(id: &str, max_frame_length: Option<usize>) -> Result<Self, String> {
        Ok(Self {
            id: id.to_string(),
            stage: Stage::AwaitingHello,
            disconnected: false,
            handshake_complete: false,
            session_ids: Default::default(),
            decoder: ClientMessageDecoder::new(FrameDecoderOptions {
                max_frame_length: Some(
                    max_frame_length.unwrap_or(pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH),
                ),
            })
            .map_err(|error| error.to_string())?,
            hello_version: None,
            handshake_deadline_ms: DEFAULT_HANDSHAKE_TIMEOUT_MS,
        })
    }
}

/// Outbound frames and state changes the host applies (upstream the
/// direct socket writes).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerOutbound {
    /// A frame to write to the connection's socket.
    Frame(Vec<u8>),
    /// Close the socket, optionally after writing a final frame.
    Close(Option<Vec<u8>>),
}

/// Server events for the host (state transitions and errors).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerHostEvent {
    StateChanged {
        connection_id: String,
        stage: Stage,
    },
    Failure {
        connection_id: String,
        error: ProtocolError,
    },
    Reported(String),
}

/// The server (upstream `PiServer`).
pub struct PiServer {
    pub id: String,
    max_frame_length: usize,
    connections: Vec<ServerConnection>,
    sessions: LiveSessionManager,
    snapshots: ServerSnapshotPublisher,
    closing: bool,
    /// Outbound frames keyed by connection id.
    pub outbound: Vec<(String, ServerOutbound)>,
    /// Host events in order.
    pub host_events: Vec<ServerHostEvent>,
    /// Responses/events decoded for the client layer.
    pub responses: Vec<(String, bool, Option<CommandResult>, Option<ProtocolError>)>,
    pub events: Vec<ServerMessage>,
}

impl PiServer {
    /// Construct with option validation (upstream `resolveOptions`).
    pub fn new(server_id: &str, max_frame_length: Option<usize>) -> Result<Self, String> {
        if server_id.is_empty() {
            return Err("PiServer serverId must not be empty".to_string());
        }
        let limit = max_frame_length.unwrap_or(pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH);
        if limit == 0 || limit > 0xffff_ffff {
            return Err(
                "PiServer maxFrameLength must be an integer between 1 and 4294967295".to_string(),
            );
        }
        Ok(Self {
            id: server_id.to_string(),
            max_frame_length: limit,
            connections: Vec::new(),
            sessions: LiveSessionManager::new(),
            snapshots: ServerSnapshotPublisher::new(server_id),
            closing: false,
            outbound: Vec::new(),
            host_events: Vec::new(),
            responses: Vec::new(),
            events: Vec::new(),
        })
    }

    fn find_connection(&mut self, id: &str) -> Option<&mut ServerConnection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    /// Accept a new connection (upstream `accept`).
    pub fn accept(&mut self, id: &str, max_frame_length: Option<usize>) -> Result<(), String> {
        if self.closing {
            self.outbound
                .push((id.to_string(), ServerOutbound::Close(None)));
            return Ok(());
        }
        let connection =
            ServerConnection::new(id, Some(self.max_frame_length).or(max_frame_length))?;
        self.connections.push(connection);
        Ok(())
    }

    /// Feed inbound bytes from a connection (upstream `receive` +
    /// `dispatchMessage`).
    pub fn receive(
        &mut self,
        connection_id: &str,
        chunk: &[u8],
        service: &mut dyn PiServerService,
        sink: &mut dyn BroadcastSink,
    ) {
        let Some(connection) = self.find_connection(connection_id) else {
            return;
        };
        if connection.disconnected || connection.stage == Stage::Closed {
            return;
        }
        let messages = match connection.decoder.push(chunk) {
            Ok(messages) => messages,
            Err(error) => {
                self.fail_protocol(
                    connection_id,
                    ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: error.to_string(),
                        details: None,
                    },
                    sink,
                );
                return;
            }
        };
        for value in messages {
            let message = match parse_client_message(&value) {
                Ok(message) => message,
                Err(error) => {
                    self.fail_protocol(
                        connection_id,
                        ProtocolError {
                            code: ProtocolErrorCode::InvalidRequest,
                            message: error.to_string(),
                            details: None,
                        },
                        sink,
                    );
                    return;
                }
            };
            self.dispatch_message(connection_id, message, service, sink);
        }
    }

    fn dispatch_message(
        &mut self,
        connection_id: &str,
        message: ClientMessage,
        service: &mut dyn PiServerService,
        sink: &mut dyn BroadcastSink,
    ) {
        let stage = match self.find_connection(connection_id) {
            Some(connection) => connection.stage,
            None => return,
        };
        if stage == Stage::AwaitingHello {
            if !matches!(message, ClientMessage::Hello { .. }) {
                self.fail_protocol(
                    connection_id,
                    ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "The first client message must be hello".to_string(),
                        details: None,
                    },
                    sink,
                );
                return;
            }
            // Move to handshaking and record the hello version.
            let version = match message {
                ClientMessage::Hello { version } => version as f64,
                _ => unreachable!(),
            };
            if let Some(connection) = self.find_connection(connection_id) {
                connection.stage = Stage::Handshaking;
                connection.hello_version = Some(version);
            }
            self.finish_handshake(connection_id, service, sink);
            return;
        }
        if matches!(message, ClientMessage::Hello { .. }) {
            self.fail_protocol(
                connection_id,
                ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    message: "hello may only be sent as the first message".to_string(),
                    details: None,
                },
                sink,
            );
            return;
        }
        if stage == Stage::Ready {
            let ClientMessage::Request(envelope) = message else {
                return;
            };
            self.handle_request(connection_id, &envelope, service, sink);
        }
    }

    fn finish_handshake(
        &mut self,
        connection_id: &str,
        service: &mut dyn PiServerService,
        sink: &mut dyn BroadcastSink,
    ) {
        let version = match self.find_connection(connection_id) {
            Some(connection) => connection.hello_version,
            None => return,
        };
        let Some(version) = version else {
            return;
        };
        if !is_supported_protocol_version(version) {
            self.fail_protocol(
                connection_id,
                ProtocolError {
                    code: ProtocolErrorCode::Version,
                    message: format!(
                        "Unsupported protocol version {version}; expected {PROTOCOL_VERSION}"
                    ),
                    details: None,
                },
                sink,
            );
            return;
        }
        // Build the handshake snapshot (upstream includes the live
        // session catalog).
        let snapshot = self.sessions_snapshot_with(service);
        let hello = serde_json::to_value(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            connection_id: connection_id.to_string(),
            snapshot,
        })
        .ok();
        let Some(hello) = hello else {
            return;
        };
        let frame = encode_server_message(
            &hello,
            FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            },
        )
        .ok();
        let Some(frame) = frame else {
            return;
        };
        self.outbound
            .push((connection_id.to_string(), ServerOutbound::Frame(frame)));
        if let Some(connection) = self.find_connection(connection_id) {
            connection.handshake_complete = true;
            connection.stage = Stage::Ready;
        }
        self.host_events.push(ServerHostEvent::StateChanged {
            connection_id: connection_id.to_string(),
            stage: Stage::Ready,
        });
        let _ = sink;
    }

    fn sessions_snapshot_with(
        &mut self,
        service: &mut dyn PiServerService,
    ) -> pillar_protocol::schemas::ServerSnapshot {
        let sessions = self.sessions.list_metadata(service);
        self.snapshots.get(&sessions, &[])
    }

    fn handle_request(
        &mut self,
        connection_id: &str,
        envelope: &RequestEnvelope,
        service: &mut dyn PiServerService,
        sink: &mut dyn BroadcastSink,
    ) {
        let mut session_ids = match self.find_connection(connection_id) {
            Some(connection) => connection.session_ids.clone(),
            None => return,
        };
        let mut state = ConnectionState {
            id: 0,
            disconnected: false,
            ready: true,
            closed: false,
            session_ids: session_ids.clone(),
        };
        // The host maps ids; execute against the session manager. The
        // server allocates durable ids for create commands (upstream
        // uuidv7 generated inside the server).
        let new_session_id = if matches!(envelope.request, Command::Create { .. }) {
            allocate_session_id()
        } else {
            String::new()
        };
        let result = self.sessions.execute_command(
            &mut state,
            &envelope.request,
            service,
            sink,
            &new_session_id,
        );
        if let Some(connection) = self.find_connection(connection_id) {
            connection.session_ids = state.session_ids;
        }
        let _ = &mut session_ids;
        let response = match result {
            Ok(result) => ServerMessage::Response {
                id: envelope.id.clone(),
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => ServerMessage::Response {
                id: envelope.id.clone(),
                ok: false,
                result: None,
                error: Some(ProtocolError {
                    code: error.code,
                    message: error.message,
                    details: None,
                }),
            },
        };
        if let Ok(value) = serde_json::to_value(&response)
            && let Ok(frame) = encode_server_message(
                &value,
                FrameDecoderOptions {
                    max_frame_length: Some(self.max_frame_length),
                },
            )
        {
            self.outbound
                .push((connection_id.to_string(), ServerOutbound::Frame(frame)));
        }
        if let ServerMessage::Response {
            id,
            ok,
            result,
            error,
        } = &response
        {
            self.responses
                .push((id.clone(), *ok, result.clone(), error.clone()));
        }
    }

    /// The transport closed (upstream `transportClosed`).
    pub fn transport_closed(&mut self, connection_id: &str, sink: &mut dyn BroadcastSink) {
        let Some(connection) = self.find_connection(connection_id) else {
            return;
        };
        if connection.disconnected || connection.stage == Stage::Closing {
            return;
        }
        let _ = connection.decoder.end();
        self.disconnect(connection_id, sink);
    }

    /// Disconnect a connection (upstream `disconnect`).
    pub fn disconnect(&mut self, connection_id: &str, sink: &mut dyn BroadcastSink) {
        let Some(connection) = self.find_connection(connection_id) else {
            return;
        };
        if connection.disconnected {
            return;
        }
        let handshake_complete = connection.handshake_complete;
        let session_ids: Vec<String> = connection.session_ids.iter().cloned().collect();
        connection.disconnected = true;
        connection.stage = Stage::Closed;
        self.sessions.disconnect(0, &session_ids, sink);
        self.connections.retain(|c| c.id != connection_id);
        if !self.closing && handshake_complete {
            // Broadcast a fresh snapshot to remaining connections.
            struct NullService;
            impl PiServerService for NullService {
                fn list_sessions(&self) -> Vec<pillar_protocol::schemas::SessionMetadata> {
                    Vec::new()
                }
                fn create_session(
                    &mut self,
                    _: &crate::sessions::CreateSessionOptions,
                ) -> Result<Box<dyn crate::sessions::PiSessionRuntime>, ServerError>
                {
                    Err(ServerError::new(
                        ProtocolErrorCode::InternalError,
                        "no service",
                    ))
                }
                fn open_session(
                    &mut self,
                    _: &str,
                ) -> Result<Box<dyn crate::sessions::PiSessionRuntime>, ServerError>
                {
                    Err(ServerError::new(
                        ProtocolErrorCode::InternalError,
                        "no service",
                    ))
                }
            }
            let mut null_service = NullService;
            let sessions = self.sessions.list_metadata(&mut null_service);
            struct SinkAdapter<'a>(&'a mut dyn BroadcastSink);
            impl crate::snapshots::SnapshotEventSink for SinkAdapter<'_> {
                fn send_event(
                    &mut self,
                    connection_id: u64,
                    event: &pillar_protocol::schemas::ServerEvent,
                ) {
                    self.0.send_event(connection_id, event);
                }
            }
            let mut adapter = SinkAdapter(sink);
            self.snapshots
                .broadcast(&mut [], &sessions, &[], self.closing, &mut adapter);
        }
        self.host_events.push(ServerHostEvent::StateChanged {
            connection_id: connection_id.to_string(),
            stage: Stage::Closed,
        });
    }

    /// Fail a connection with a protocol error (upstream
    /// `failProtocol`): stage to closing, emit hello_error, close.
    pub fn fail_protocol(
        &mut self,
        connection_id: &str,
        error: ProtocolError,
        sink: &mut dyn BroadcastSink,
    ) {
        let Some(connection) = self.find_connection(connection_id) else {
            return;
        };
        if connection.disconnected
            || connection.stage == Stage::Closing
            || connection.stage == Stage::Closed
        {
            return;
        }
        connection.stage = Stage::Closing;
        self.host_events.push(ServerHostEvent::Failure {
            connection_id: connection_id.to_string(),
            error: error.clone(),
        });
        let final_frame = encode_server_message(
            &serde_json::to_value(ServerMessage::HelloError { error }).unwrap_or_default(),
            FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            },
        )
        .ok();
        self.outbound.push((
            connection_id.to_string(),
            ServerOutbound::Close(final_frame),
        ));
        self.disconnect(connection_id, sink);
    }

    pub fn set_closing(&mut self) {
        self.closing = true;
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }

    pub fn connection_stage(&self, id: &str) -> Option<Stage> {
        self.connections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.stage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{CreateSessionOptions, PiSessionRuntime};
    use pillar_protocol::schemas::Command;

    struct NoService;
    impl PiServerService for NoService {
        fn list_sessions(&self) -> Vec<pillar_protocol::schemas::SessionMetadata> {
            Vec::new()
        }
        fn create_session(
            &mut self,
            _: &CreateSessionOptions,
        ) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
            Err(ServerError::new(
                ProtocolErrorCode::InternalError,
                "no service",
            ))
        }
        fn open_session(&mut self, _: &str) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
            Err(ServerError::new(
                ProtocolErrorCode::InternalError,
                "no service",
            ))
        }
    }

    struct NullSink;
    impl BroadcastSink for NullSink {
        fn send_event(&mut self, _: u64, _: &pillar_protocol::schemas::ServerEvent) {}
        fn broadcast_server_snapshot(&mut self) {}
        fn close_connection(&mut self, _: u64) {}
        fn report_error(&mut self, _: String) {}
    }

    fn hello_frame(version: u64) -> Vec<u8> {
        let message = serde_json::to_value(ClientMessage::Hello { version }).unwrap();
        pillar_protocol::codec::encode_client_message(
            &message,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap()
    }

    fn outbound_frames(server: &PiServer, connection_id: &str) -> Vec<Vec<u8>> {
        server
            .outbound
            .iter()
            .filter(|(id, kind)| id == connection_id && matches!(kind, ServerOutbound::Frame(_)))
            .filter_map(|(_, kind)| match kind {
                ServerOutbound::Frame(frame) => Some(frame.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn constructor_rejects_empty_server_id() {
        assert!(matches!(
            PiServer::new("", None),
            Err(message) if message == "PiServer serverId must not be empty"
        ));
    }

    #[test]
    fn constructor_validates_frame_limit() {
        assert!(matches!(
            PiServer::new("s", Some(0)),
            Err(message) if message == "PiServer maxFrameLength must be an integer between 1 and 4294967295"
        ));
        assert!(matches!(
            PiServer::new("s", Some(0x1_0000_0000)),
            Err(message) if message.contains("maxFrameLength")
        ));
        assert!(PiServer::new("s", Some(512)).is_ok());
    }

    #[test]
    fn accept_while_closing_immediately_closes() {
        let mut server = PiServer::new("s", None).unwrap();
        server.set_closing();
        server.accept("c1", None).unwrap();
        assert!(matches!(
            server.outbound.first(),
            Some((id, ServerOutbound::Close(_))) if id == "c1"
        ));
        assert!(server.connection_stage("c1").is_none());
    }

    #[test]
    fn non_hello_first_message_fails() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let request = serde_json::to_value(ClientMessage::Request(RequestEnvelope {
            id: "r1".to_string(),
            request: Command::List,
        }))
        .unwrap();
        let frame = pillar_protocol::codec::encode_client_message(
            &request,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        let mut service = NoService;
        let mut sink = NullSink;
        server.receive("c1", &frame, &mut service, &mut sink);
        // hello_error with the invalid-request message is sent.
        assert!(server.host_events.iter().any(|event| matches!(
            event,
            ServerHostEvent::Failure { error, .. }
                if error.message == "The first client message must be hello"
        )));
        assert!(matches!(
            server.outbound.last(),
            Some((id, ServerOutbound::Close(_))) if id == "c1"
        ));
    }

    #[test]
    fn valid_hello_completes_handshake_to_ready() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let mut service = NoService;
        let mut sink = NullSink;
        server.receive(
            "c1",
            &hello_frame(PROTOCOL_VERSION),
            &mut service,
            &mut sink,
        );
        assert_eq!(server.connection_stage("c1"), Some(Stage::Ready));
        // A framed server hello went out.
        let frames = outbound_frames(&server, "c1");
        assert_eq!(frames.len(), 1);
        // Decoding it as a server frame yields the hello.
        let mut server_decoder =
            pillar_protocol::codec::ServerMessageDecoder::new(FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            })
            .unwrap();
        let server_messages = server_decoder.push(&frames[0]).unwrap();
        assert_eq!(server_messages.len(), 1);
        let hello = parse_client_message(&server_messages[0]);
        assert!(hello.is_err()); // it's a server message, not a client one
    }

    #[test]
    fn unsupported_version_fails_with_version_error() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let mut service = NoService;
        let mut sink = NullSink;
        server.receive("c1", &hello_frame(999), &mut service, &mut sink);
        assert!(server.host_events.iter().any(|event| matches!(
            event,
            ServerHostEvent::Failure { error, .. }
                if error.code == ProtocolErrorCode::Version
                    && error.message.contains("Unsupported protocol version 999")
        )));
        // The failed connection is closed and removed.
        assert_eq!(server.connection_stage("c1"), None);
    }

    #[test]
    fn second_hello_fails() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let mut service = NoService;
        let mut sink = NullSink;
        server.receive(
            "c1",
            &hello_frame(PROTOCOL_VERSION),
            &mut service,
            &mut sink,
        );
        assert_eq!(server.connection_stage("c1"), Some(Stage::Ready));
        server.receive(
            "c1",
            &hello_frame(PROTOCOL_VERSION),
            &mut service,
            &mut sink,
        );
        assert!(server.host_events.iter().any(|event| matches!(
            event,
            ServerHostEvent::Failure { error, .. }
                if error.message == "hello may only be sent as the first message"
        )));
    }

    #[test]
    fn disconnect_is_idempotent() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let mut sink = NullSink;
        server.disconnect("c1", &mut sink);
        let state_events = server
            .host_events
            .iter()
            .filter(|event| matches!(event, ServerHostEvent::StateChanged { .. }))
            .count();
        server.disconnect("c1", &mut sink);
        let after = server
            .host_events
            .iter()
            .filter(|event| matches!(event, ServerHostEvent::StateChanged { .. }))
            .count();
        assert_eq!(state_events, after);
    }

    #[test]
    fn transport_close_disconnects() {
        let mut server = PiServer::new("s", None).unwrap();
        server.accept("c1", None).unwrap();
        let mut sink = NullSink;
        server.transport_closed("c1", &mut sink);
        assert!(server.host_events.iter().any(|event| matches!(
            event,
            ServerHostEvent::StateChanged {
                stage: Stage::Closed,
                ..
            }
        )));
    }
}
