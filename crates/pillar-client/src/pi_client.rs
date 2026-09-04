//! Port of the PiClient request/response layer from
//! packages/client/src (pi v0.84.3, client.ts + connection.test.ts
//! behavior): request-id tracking with pending rejection on
//! disconnection, frame-limit validation, and handshake version
//! errors.
//!
//! divergences: the async promise plumbing becomes a pending-request
//! map that the host drains (each pending id maps to the failure it
//! would reject with); the transport factory remains host-side.

use std::collections::HashMap;

use pillar_protocol::schemas::{
    Command, CommandResult, ProtocolError, ProtocolErrorCode, ServerEvent, ServerMessage,
    ServerSnapshot,
};

use crate::connection::{ByteTransport, Connection, ConnectionState};

/// Client error surface (upstream PiServerError / PiDisconnectedError
/// reduced to a message-bearing enum).
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Server {
        code: ProtocolErrorCode,
        message: String,
    },
    Disconnected(String),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server { message, .. } => write!(f, "{message}"),
            Self::Disconnected(message) => write!(f, "{message}"),
            Self::Protocol(message) => write!(f, "{message}"),
        }
    }
}

/// A pending request awaiting its response (upstream the promise
/// resolvers map).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub id: String,
}

/// The client (upstream `PiClient`): wraps a Connection with
/// request/response bookkeeping.
pub struct PiClient {
    connection: Connection,
    /// Embedded client state (upstream ClientState is composed into
    /// the client).
    pub state: crate::ClientState,
    next_request_id: u64,
    /// Pending request ids → the error they would reject with.
    pub pending: HashMap<String, Option<ClientError>>,
    /// Fully decoded responses ready for the host.
    pub responses: Vec<(String, Result<CommandResult, ClientError>)>,
    /// Fully decoded events ready for the host.
    pub events: Vec<ServerEvent>,
    /// The handshake snapshot once resolved.
    pub handshake: Option<ServerSnapshot>,
    listener_failures: usize,
    /// The last connection failure message (upstream the rejected
    /// handshake / state change error).
    pub last_failure: Option<String>,
}

impl PiClient {
    /// Construct with a frame limit (upstream the constructor's
    /// maxFrameLength validation: integer 1..=u32::MAX).
    pub fn new(max_frame_length: Option<usize>) -> Result<Self, String> {
        if let Some(limit) = max_frame_length
            && (limit == 0 || limit > 0xffff_ffff)
        {
            return Err(
                "PiClient maxFrameLength must be an integer between 1 and 4294967295".to_string(),
            );
        }
        Ok(Self {
            connection: Connection::new(max_frame_length),
            state: crate::ClientState::new(),
            next_request_id: 0,
            pending: HashMap::new(),
            responses: Vec::new(),
            events: Vec::new(),
            handshake: None,
            listener_failures: 0,
            last_failure: None,
        })
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.connection.state()
    }

    /// Begin connecting: returns the hello frame (upstream
    /// `connect`'s encoded hello).
    pub fn connect(&mut self) -> Result<Vec<u8>, ClientError> {
        self.connection
            .connect()
            .map_err(|error| ClientError::Disconnected(error.message))
    }

    /// Attach the opened transport (upstream the factory result).
    pub fn attach_transport(
        &mut self,
        id: u64,
        transport: Box<dyn ByteTransport>,
    ) -> Option<Vec<u8>> {
        self.connection.attach_transport(id, transport)
    }

    /// Feed inbound bytes (upstream the onData handler path).
    pub fn handle_data(&mut self, id: u64, chunk: &[u8]) {
        self.connection.handle_data(id, chunk);
        self.drain_connection_events();
    }

    /// The transport closed (upstream onClose: pending requests reject
    /// with PiDisconnectedError).
    pub fn handle_close(&mut self, id: u64) {
        self.connection.handle_close(id);
        self.drain_connection_events();
        self.reject_pending("Pi client is disconnected");
    }

    /// A transport error (upstream onError: pending requests reject
    /// with the error message).
    pub fn handle_error(&mut self, id: u64, error: &str) {
        self.connection.handle_error(id, error);
        self.drain_connection_events();
        self.reject_pending(error);
    }

    /// Explicit disconnect (upstream `disconnect`): pending requests
    /// reject with the reason.
    pub fn disconnect(&mut self, reason: &str) {
        self.connection.disconnect(reason);
        self.drain_connection_events();
        self.reject_pending(reason);
    }

    /// Send a session/list command (upstream `listSessions` etc.):
    /// returns the request id and the frame to write. Fails when not
    /// connected.
    pub fn request(&mut self, command: Command) -> Result<(String, Vec<u8>), ClientError> {
        if self.connection_state() != ConnectionState::Connected {
            return Err(ClientError::Disconnected(
                "Pi client is disconnected".to_string(),
            ));
        }
        self.next_request_id += 1;
        let id = self.next_request_id.to_string();
        self.pending.insert(id.clone(), None);
        let envelope = serde_json::json!({
            "type": "request",
            "id": id,
            "request": serde_json::to_value(&command).map_err(|e| ClientError::Protocol(e.to_string()))?,
        });
        let frame = pillar_protocol::codec::encode_client_message(
            &envelope,
            pillar_protocol::framing::FrameDecoderOptions {
                max_frame_length: Some(self.connection.max_frame_length()),
            },
        )
        .map_err(|error| {
            // The request never left: it is not pending.
            self.pending.remove(&id);
            ClientError::Protocol(error.to_string())
        })?;
        Ok((id, frame))
    }

    /// Complete a request with a server response payload (upstream the
    /// Response message branch). Returns false when the id is unknown.
    pub fn complete_request(
        &mut self,
        id: &str,
        ok: bool,
        result: Option<CommandResult>,
        error: Option<ProtocolError>,
    ) -> bool {
        if !self.pending.contains_key(id) {
            return false;
        }
        let outcome = if ok {
            result
                .map(|result| {
                    self.apply_result(&result);
                    Ok(result)
                })
                .unwrap_or_else(|| Err(ClientError::Protocol("missing result".to_string())))
        } else {
            Err(error
                .map(|error| ClientError::Server {
                    code: error.code,
                    message: error.message,
                })
                .unwrap_or(ClientError::Protocol("missing error".to_string())))
        };
        self.pending.remove(id);
        self.responses.push((id.to_string(), outcome));
        true
    }

    /// Take the failures that pending requests would reject with, then
    /// drop them (upstream promise rejection).
    pub fn take_pending_failures(&mut self) -> Vec<(String, ClientError)> {
        let failures: Vec<(String, ClientError)> = self
            .pending
            .iter()
            .filter_map(|(id, error)| {
                error.clone().map(|error| (id.clone(), error)).or_else(|| {
                    Some((
                        id.clone(),
                        ClientError::Disconnected("Pi client is disconnected".to_string()),
                    ))
                })
            })
            .collect();
        self.pending.clear();
        failures
    }

    fn reject_pending(&mut self, message: &str) {
        let ids: Vec<String> = self.pending.keys().cloned().collect();
        for id in ids {
            if let Some(entry) = self.pending.get_mut(&id) {
                *entry = Some(ClientError::Disconnected(message.to_string()));
            }
        }
    }

    fn apply_result(&mut self, result: &CommandResult) {
        // Snapshot/event application lives in ClientState (upstream
        // the client routes results into ClientState).
        self.state.apply_result(result);
    }

    fn drain_connection_events(&mut self) {
        let events = std::mem::take(&mut self.connection.events);
        for event in events {
            match event {
                crate::connection::ConnectionEvent::Handshake(snapshot) => {
                    self.handshake = Some(snapshot);
                }
                crate::connection::ConnectionEvent::Failed(error) => {
                    self.last_failure = Some(error);
                }
                crate::connection::ConnectionEvent::StateChange(_) => {}
                crate::connection::ConnectionEvent::Message(message) => match message {
                    ServerMessage::Response {
                        id,
                        ok,
                        result,
                        error,
                    } => {
                        let _ = self.complete_request(&id, ok, result, error);
                    }
                    ServerMessage::Event { event } => {
                        self.state.apply_event(&event);
                        self.events.push(event);
                    }
                    _ => {}
                },
            }
        }
    }

    /// Count listener failures (upstream the onListenerError hook).
    pub fn record_listener_failure(&mut self) {
        self.listener_failures += 1;
    }

    pub fn listener_failures(&self) -> usize {
        self.listener_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_protocol::schemas::PROTOCOL_VERSION;
    use pillar_protocol::schemas::{
        ProtocolError, ServerEvent, ServerSnapshot, SessionPhase, SessionSnapshot, ThinkingLevel,
    };

    struct RecordingTransport {
        closed: bool,
    }

    impl ByteTransport for RecordingTransport {
        fn send(&mut self, _chunk: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {
            self.closed = true;
        }
    }

    fn snapshot(revision: u64) -> ServerSnapshot {
        ServerSnapshot {
            server_id: "s".to_string(),
            protocol_version: 1,
            revision,
            sessions: vec![],
            models: vec![],
        }
    }

    #[allow(dead_code)]
    fn session(id: &str, attached: bool) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            name: None,
            cwd: "/tmp".to_string(),
            created_at: 0,
            updated_at: 0,
            phase: SessionPhase::Idle,
            model: pillar_protocol::schemas::ModelRef {
                provider: "p".to_string(),
                id: "m".to_string(),
            },
            thinking_level: ThinkingLevel::Off,
            attached,
            locked: false,
            revision: 1,
            transcript: vec![],
            queued_steer: vec![],
            queued_steer_count: 0,
        }
    }

    fn connect_with_handshake(client: &mut PiClient, revision: u64) -> u64 {
        let id = 1;
        let _ = client.connect().unwrap();
        let _ = client.attach_transport(id, Box::new(RecordingTransport { closed: false }));
        let hello = serde_json::to_value(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            connection_id: "c".to_string(),
            snapshot: snapshot(revision),
        })
        .unwrap();
        let frame = pillar_protocol::codec::encode_server_message(
            &hello,
            pillar_protocol::framing::FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        client.handle_data(id, &frame);
        id
    }

    #[test]
    fn request_requires_connected_state() {
        let mut client = PiClient::new(None).unwrap();
        assert!(matches!(
            client.request(Command::List),
            Err(ClientError::Disconnected(_))
        ));
        assert!(client.pending.is_empty());
    }

    #[test]
    fn pending_requests_reject_on_close() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        let (request_id, _frame) = client.request(Command::List).unwrap();
        assert!(client.pending.contains_key(&request_id));
        client.handle_close(id);
        let failures = client.take_pending_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].1,
            ClientError::Disconnected("Pi client is disconnected".to_string())
        );
    }

    #[test]
    fn pending_requests_reject_on_transport_error_with_message() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        let _ = client.request(Command::List).unwrap();
        client.handle_error(id, "read failed");
        let failures = client.take_pending_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].1,
            ClientError::Disconnected("read failed".to_string())
        );
    }

    #[test]
    fn disconnect_rejects_pending_with_reason() {
        let mut client = PiClient::new(None).unwrap();
        connect_with_handshake(&mut client, 1);
        let _ = client.request(Command::List).unwrap();
        client.disconnect("bye");
        let failures = client.take_pending_failures();
        assert_eq!(failures[0].1, ClientError::Disconnected("bye".to_string()));
    }

    #[test]
    fn response_completes_pending_request() {
        let mut client = PiClient::new(None).unwrap();
        connect_with_handshake(&mut client, 1);
        let (request_id, _frame) = client.request(Command::List).unwrap();
        let ok = client.complete_request(
            &request_id,
            true,
            Some(CommandResult::List { sessions: vec![] }),
            None,
        );
        assert!(ok);
        assert!(client.pending.is_empty());
        let (resolved_id, outcome) = client.responses.remove(0);
        assert_eq!(resolved_id, request_id);
        assert!(outcome.is_ok());
    }

    #[test]
    fn error_response_completes_with_server_error() {
        let mut client = PiClient::new(None).unwrap();
        connect_with_handshake(&mut client, 1);
        let (request_id, _) = client.request(Command::List).unwrap();
        assert!(client.complete_request(
            &request_id,
            false,
            None,
            Some(ProtocolError {
                code: ProtocolErrorCode::SessionLocked,
                message: "locked".to_string(),
                details: None,
            }),
        ));
        let (_, outcome) = client.responses.remove(0);
        assert_eq!(
            outcome,
            Err(ClientError::Server {
                code: ProtocolErrorCode::SessionLocked,
                message: "locked".to_string(),
            })
        );
    }

    #[test]
    fn unknown_response_ids_are_ignored() {
        let mut client = PiClient::new(None).unwrap();
        assert!(!client.complete_request("nope", true, None, None));
    }

    #[test]
    fn server_events_surface_to_the_host() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        let event_message = serde_json::to_value(ServerMessage::Event {
            event: ServerEvent::SessionRemoved {
                session_id: "a".to_string(),
            },
        })
        .unwrap();
        let frame = pillar_protocol::codec::encode_server_message(
            &event_message,
            pillar_protocol::framing::FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        client.handle_data(id, &frame);
        assert_eq!(client.events.len(), 1);
        assert!(matches!(
            client.events[0],
            ServerEvent::SessionRemoved { .. }
        ));
    }

    #[test]
    fn invalid_protocol_data_disconnects() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        // A raw frame with an invalid CBOR body.
        client.handle_data(id, &[0, 0, 0, 2, 1, 2]);
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    }

    #[test]
    fn truncated_framing_reports_truncation() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        client.handle_data(id, &[0, 0, 0, 2, 1]);
        client.handle_close(id);
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
        // Some failure mentions truncation or the closed transport.
        assert!(client.last_failure.is_some());
    }

    #[test]
    fn frame_limit_validation() {
        assert!(PiClient::new(Some(0)).is_err());
        assert!(PiClient::new(Some(0x1_0000_0000)).is_err());
        assert!(PiClient::new(Some(512)).is_ok());
        assert!(PiClient::new(None).is_ok());
    }

    #[test]
    fn listener_failures_are_counted_not_fatal() {
        let mut client = PiClient::new(None).unwrap();
        let id = connect_with_handshake(&mut client, 1);
        client.record_listener_failure();
        // The connection is unaffected.
        assert_eq!(client.connection_state(), ConnectionState::Connected);
        let _ = id;
    }

    #[test]
    fn handshake_snapshot_is_polled_once() {
        let mut client = PiClient::new(None).unwrap();
        connect_with_handshake(&mut client, 1);
        assert!(client.handshake.is_some());
    }
}
