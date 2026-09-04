//! Port of packages/client/src/connection.ts (pi v0.84.3): the client
//! connection state machine over an injected byte transport — hello
//! handshake, negotiation of the first server message, stale-connection
//! guards, and failure propagation.
//!
//! divergences: the transport is a trait the host implements (upstream
//! a ByteTransportFactory returning promises); promise resolvers become
//! a stored handshake slot the host polls; encoding goes through the
//! pillar-protocol codec.

use crate::PiDisconnectedError;
use pillar_protocol::codec::{ServerMessageDecoder, encode_client_message};
use pillar_protocol::framing::FrameDecoderOptions;
use pillar_protocol::schemas::{
    ClientMessage, PROTOCOL_VERSION, ProtocolError, ServerMessage, ServerSnapshot,
    parse_server_message,
};

/// Connection state (upstream `ConnectionState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

/// A state change notification (upstream `ConnectionStateChange`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionStateChange {
    pub state: ConnectionState,
    pub error: Option<String>,
}

/// The host-implemented byte transport (upstream `ByteTransport`).
pub trait ByteTransport {
    /// Sends one byte chunk; calls are delivered in invocation order.
    fn send(&mut self, chunk: &[u8]) -> Result<(), String>;
    /// Closes the transport; repeated calls must be harmless.
    fn close(&mut self);
}

/// Transport event handlers the host drives (upstream
/// `ByteTransportHandlers`).
pub trait ByteTransportHandlers {
    fn on_data(&mut self, connection: &mut Connection, chunk: &[u8]);
    fn on_close(&mut self, connection: &mut Connection);
    fn on_error(&mut self, connection: &mut Connection, error: &str);
}

/// Outcome records for the host (upstream the callbacks + promise
/// resolvers).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ConnectionEvent {
    StateChange(ConnectionStateChange),
    Handshake(ServerSnapshot),
    /// A non-handshake server message (Response or Event) decoded and
    /// validated.
    Message(ServerMessage),
    /// The handshake or connection failed.
    Failed(String),
}

/// Connection lifecycle (upstream `ConnectionLifecycle`).
enum Lifecycle {
    Disconnected,
    Connecting {
        id: u64,
        decoder: ServerMessageDecoder,
        handshake: Option<ServerSnapshot>,
        transport: Option<Box<dyn ByteTransport>>,
    },
    Connected {
        id: u64,
        decoder: ServerMessageDecoder,
        /// The transport stays alive here for the host to close via
        /// fail(); never read directly.
        #[allow(dead_code)]
        transport: Box<dyn ByteTransport>,
        /// The resolved handshake snapshot (upstream the resolved
        /// promise), cleared by take_handshake.
        handshake: Option<ServerSnapshot>,
    },
}

/// The client connection state machine (upstream `Connection`).
pub struct Connection {
    max_frame_length: usize,
    lifecycle: Lifecycle,
    sequence: u64,
    /// Events recorded for the host since the last drain.
    pub events: Vec<ConnectionEvent>,
}

impl Connection {
    pub fn new(max_frame_length: Option<usize>) -> Self {
        Self {
            max_frame_length: max_frame_length
                .unwrap_or(pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH),
            lifecycle: Lifecycle::Disconnected,
            sequence: 0,
            events: Vec::new(),
        }
    }

    pub fn state(&self) -> ConnectionState {
        match &self.lifecycle {
            Lifecycle::Disconnected => ConnectionState::Disconnected,
            Lifecycle::Connecting { .. } => ConnectionState::Connecting,
            Lifecycle::Connected { .. } => ConnectionState::Connected,
        }
    }

    pub fn max_frame_length(&self) -> usize {
        self.max_frame_length
    }

    /// The resolved handshake snapshot, if any (upstream the handshake
    /// promise resolution).
    pub fn take_handshake(&mut self) -> Option<ServerSnapshot> {
        match &mut self.lifecycle {
            Lifecycle::Connecting { handshake, .. } => handshake.take(),
            Lifecycle::Connected { handshake, .. } => handshake.take(),
            Lifecycle::Disconnected => None,
        }
    }

    fn is_current(&self, id: u64) -> bool {
        match &self.lifecycle {
            Lifecycle::Disconnected => false,
            Lifecycle::Connecting { id: current, .. }
            | Lifecycle::Connected { id: current, .. } => *current == id,
        }
    }

    fn fail(&mut self, error: &str) {
        let previous = std::mem::replace(&mut self.lifecycle, Lifecycle::Disconnected);
        if let Lifecycle::Connecting {
            handshake,
            transport,
            ..
        } = previous
        {
            let mut transport = transport;
            if let Some(transport) = transport.as_mut() {
                transport.close();
            }
            drop(transport);
            if let Some(_snapshot) = handshake {
                // The handshake is failed via the Failed event below.
            }
        }
        self.events.push(ConnectionEvent::Failed(error.to_string()));
        self.events
            .push(ConnectionEvent::StateChange(ConnectionStateChange {
                state: ConnectionState::Disconnected,
                error: Some(error.to_string()),
            }));
    }

    /// Begin connecting (upstream `connect`): returns the hello frame to
    /// send once the host's transport is open, plus the state change.
    pub fn connect(&mut self) -> Result<Vec<u8>, PiDisconnectedError> {
        if self.state() != ConnectionState::Disconnected {
            return Err(PiDisconnectedError {
                message: format!(
                    "PiClient is already {}",
                    match self.state() {
                        ConnectionState::Connecting => "connecting",
                        ConnectionState::Connected => "connected",
                        ConnectionState::Disconnected => "disconnected",
                    }
                ),
            });
        }
        self.sequence += 1;
        let id = self.sequence;
        let decoder = ServerMessageDecoder::new(FrameDecoderOptions {
            max_frame_length: Some(self.max_frame_length),
        })
        .map_err(|_| PiDisconnectedError {
            message: "invalid frame length".to_string(),
        })?;
        self.lifecycle = Lifecycle::Connecting {
            id,
            decoder,
            handshake: None,
            transport: None,
        };
        self.events
            .push(ConnectionEvent::StateChange(ConnectionStateChange {
                state: ConnectionState::Connecting,
                error: None,
            }));
        // Encode the client hello.
        let hello = serde_json::to_value(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .map_err(|_| PiDisconnectedError {
            message: "hello encode failed".to_string(),
        })?;
        let frame = encode_client_message(
            &hello,
            FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            },
        )
        .map_err(|error| PiDisconnectedError {
            message: error.to_string(),
        })?;
        Ok(frame)
    }

    /// Attach the opened transport and send the hello (upstream
    /// `#openTransport`): returns the hello bytes to write.
    pub fn attach_transport(
        &mut self,
        id: u64,
        transport: Box<dyn ByteTransport>,
    ) -> Option<Vec<u8>> {
        if !self.is_current(id) {
            let mut transport = transport;
            transport.close();
            return None;
        }
        let frame = self.connect_hello_frame();
        match &mut self.lifecycle {
            Lifecycle::Connecting {
                transport: slot, ..
            } => {
                *slot = Some(transport);
            }
            _ => return None,
        }
        frame
    }

    fn connect_hello_frame(&self) -> Option<Vec<u8>> {
        let hello = serde_json::to_value(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .ok()?;
        encode_client_message(
            &hello,
            FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            },
        )
        .ok()
    }

    /// Feed inbound bytes (upstream `#handleData`).
    pub fn handle_data(&mut self, id: u64, chunk: &[u8]) {
        let decoder = match &mut self.lifecycle {
            Lifecycle::Disconnected => return,
            Lifecycle::Connecting {
                id: current,
                decoder,
                transport,
                ..
            } => {
                if *current != id {
                    return;
                }
                if transport.is_none() {
                    self.fail("Received server data before the client hello was sent");
                    return;
                }
                decoder
            }
            Lifecycle::Connected {
                id: current,
                decoder,
                ..
            } => {
                if *current != id {
                    return;
                }
                decoder
            }
        };
        let messages = match decoder.push(chunk) {
            Ok(messages) => messages,
            Err(error) => {
                self.fail(&error.to_string());
                return;
            }
        };
        for value in messages {
            if self.state() == ConnectionState::Disconnected {
                return;
            }
            let message = match parse_server_message(&value) {
                Ok(message) => message,
                Err(error) => {
                    self.fail(&error.to_string());
                    return;
                }
            };
            self.handle_message(id, message);
            if self.state() == ConnectionState::Disconnected {
                return;
            }
        }
    }

    fn handle_message(&mut self, id: u64, message: ServerMessage) {
        match (&mut self.lifecycle, message) {
            (Lifecycle::Connecting { .. }, ServerMessage::HelloError { error }) => {
                let ProtocolError { message, .. } = error;
                self.fail(&message);
            }
            (Lifecycle::Connecting { .. }, ServerMessage::Hello { snapshot, .. }) => {
                // Transition to connected.
                let old = std::mem::replace(&mut self.lifecycle, Lifecycle::Disconnected);
                if let Lifecycle::Connecting {
                    decoder,
                    transport,
                    handshake,
                    ..
                } = old
                {
                    let mut transport = transport;
                    let _ = transport.as_mut().map(|t| t as *mut _);
                    // Store the handshake, keep the transport alive.
                    self.lifecycle = Lifecycle::Connected {
                        id,
                        decoder,
                        transport: transport.unwrap_or_else(unreachable_transport),
                        handshake: Some(snapshot.clone()),
                    };
                    // Record handshake event with the snapshot.
                    self.events
                        .push(ConnectionEvent::Handshake(snapshot.clone()));
                    let _ = handshake;
                    self.events
                        .push(ConnectionEvent::StateChange(ConnectionStateChange {
                            state: ConnectionState::Connected,
                            error: None,
                        }));
                }
            }
            (Lifecycle::Connecting { .. }, _) => {
                self.fail("Expected server hello as first message");
            }
            (Lifecycle::Connected { .. }, ServerMessage::Hello { .. })
            | (Lifecycle::Connected { .. }, ServerMessage::HelloError { .. }) => {
                self.fail("Unexpected handshake message");
            }
            (Lifecycle::Connected { .. }, message) => {
                self.events.push(ConnectionEvent::Message(message));
            }
            (Lifecycle::Disconnected, _) => {}
        }
    }

    /// The transport closed (upstream `#handleClose`).
    pub fn handle_close(&mut self, id: u64) {
        if !self.is_current(id) {
            return;
        }
        // A clean close after handshake completion still disconnects.
        self.fail("Byte transport closed");
    }

    /// A transport error (upstream the onError handler).
    pub fn handle_error(&mut self, id: u64, error: &str) {
        if !self.is_current(id) {
            return;
        }
        self.fail(
            &PiDisconnectedError {
                message: error.to_string(),
            }
            .message,
        );
    }

    /// Disconnect explicitly (upstream `disconnect`).
    pub fn disconnect(&mut self, reason: &str) {
        if self.state() == ConnectionState::Disconnected {
            return;
        }
        self.fail(reason);
    }
}

fn unreachable_transport() -> Box<dyn ByteTransport> {
    struct None;
    impl ByteTransport for None {
        fn send(&mut self, _chunk: &[u8]) -> Result<(), String> {
            Err("no transport".to_string())
        }
        fn close(&mut self) {}
    }
    Box::new(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingTransport {
        sent: Vec<Vec<u8>>,
        closed: bool,
    }

    impl ByteTransport for RecordingTransport {
        fn send(&mut self, chunk: &[u8]) -> Result<(), String> {
            self.sent.push(chunk.to_vec());
            Ok(())
        }

        fn close(&mut self) {
            self.closed = true;
        }
    }

    #[test]
    fn connect_emits_hello_and_connecting_state() {
        let mut connection = Connection::new(None);
        let frame = connection.connect().unwrap();
        assert_eq!(connection.state(), ConnectionState::Connecting);
        assert!(matches!(
            connection.events.first(),
            Some(ConnectionEvent::StateChange(ConnectionStateChange {
                state: ConnectionState::Connecting,
                ..
            }))
        ));
        // The frame is a valid encoded hello.
        assert!(!frame.is_empty());
    }

    #[test]
    fn connect_twice_fails() {
        let mut connection = Connection::new(None);
        let _ = connection.connect().unwrap();
        assert!(connection.connect().is_err());
    }

    #[test]
    fn server_hello_completes_handshake() {
        let mut connection = Connection::new(None);
        let id = 1;
        let _ = connection.connect().unwrap();
        let transport = Box::new(RecordingTransport {
            sent: vec![],
            closed: false,
        });
        let _ = connection.attach_transport(id, transport);
        // Feed a hello snapshot frame.
        let snapshot = ServerSnapshot {
            server_id: "s".to_string(),
            protocol_version: 1,
            revision: 1,
            sessions: vec![],
            models: vec![],
        };
        let message = serde_json::to_value(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            connection_id: "c".to_string(),
            snapshot: snapshot.clone(),
        })
        .unwrap();
        let frame = pillar_protocol::codec::encode_server_message(
            &message,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        connection.handle_data(id, &frame);
        assert_eq!(connection.state(), ConnectionState::Connected);
        assert_eq!(connection.take_handshake(), Some(snapshot));
        // State change to connected recorded.
        assert!(connection.events.iter().any(|event| matches!(
            event,
            ConnectionEvent::StateChange(ConnectionStateChange {
                state: ConnectionState::Connected,
                ..
            })
        )));
    }

    #[test]
    fn hello_error_fails_connection() {
        let mut connection = Connection::new(None);
        let id = 1;
        let _ = connection.connect().unwrap();
        let _ = connection.attach_transport(
            id,
            Box::new(RecordingTransport {
                sent: vec![],
                closed: false,
            }),
        );
        let message = serde_json::to_value(ServerMessage::HelloError {
            error: ProtocolError {
                code: pillar_protocol::schemas::ProtocolErrorCode::Version,
                message: "bad version".to_string(),
                details: None,
            },
        })
        .unwrap();
        let frame = pillar_protocol::codec::encode_server_message(
            &message,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        connection.handle_data(id, &frame);
        assert_eq!(connection.state(), ConnectionState::Disconnected);
        assert!(connection.events.iter().any(
            |event| matches!(event, ConnectionEvent::Failed(text) if text.contains("bad version"))
        ));
    }

    #[test]
    fn non_hello_first_message_fails() {
        let mut connection = Connection::new(None);
        let id = 1;
        let _ = connection.connect().unwrap();
        let _ = connection.attach_transport(
            id,
            Box::new(RecordingTransport {
                sent: vec![],
                closed: false,
            }),
        );
        let message = serde_json::to_value(ServerMessage::Event {
            event: pillar_protocol::schemas::ServerEvent::SessionRemoved {
                session_id: "a".to_string(),
            },
        })
        .unwrap();
        let frame = pillar_protocol::codec::encode_server_message(
            &message,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        connection.handle_data(id, &frame);
        assert_eq!(connection.state(), ConnectionState::Disconnected);
        assert!(connection
            .events
            .iter()
            .any(|event| matches!(event, ConnectionEvent::Failed(text) if text.contains("Expected server hello"))));
    }

    #[test]
    fn stale_connection_data_is_ignored() {
        let mut connection = Connection::new(None);
        let _ = connection.connect().unwrap();
        connection.handle_data(99, b"junk");
        assert_eq!(connection.state(), ConnectionState::Connecting);
        assert!(
            connection
                .events
                .iter()
                .all(|event| !matches!(event, ConnectionEvent::Failed(_)))
        );
    }

    #[test]
    fn transport_close_disconnects() {
        let mut connection = Connection::new(None);
        let id = 1;
        let _ = connection.connect().unwrap();
        let _ = connection.attach_transport(
            id,
            Box::new(RecordingTransport {
                sent: vec![],
                closed: false,
            }),
        );
        connection.handle_close(id);
        assert_eq!(connection.state(), ConnectionState::Disconnected);
        assert!(connection
            .events
            .iter()
            .any(|event| matches!(event, ConnectionEvent::Failed(text) if text.contains("Byte transport closed"))));
    }

    #[test]
    fn explicit_disconnect_is_idempotent() {
        let mut connection = Connection::new(None);
        let _ = connection.connect().unwrap();
        connection.disconnect("bye");
        connection.disconnect("bye");
        assert_eq!(connection.state(), ConnectionState::Disconnected);
        // Only one Failed event — the second disconnect is a no-op.
        assert_eq!(
            connection
                .events
                .iter()
                .filter(|event| matches!(event, ConnectionEvent::Failed(_)))
                .count(),
            1
        );
    }
}
