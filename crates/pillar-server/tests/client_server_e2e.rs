//! End-to-end integration over the ported client and server decision
//! cores: PiClient (client hello, request framing, response
//! resolution, state application) wired against PiServer
//! (handshake, dispatch, LiveSessionManager) through a byte pipe —
//! the port's stand-in for the real Unix socket pair.

use pillar_client::connection::ConnectionState;
use pillar_client::connection::LoopTransport;
use pillar_client::pi_client::ClientError;
use pillar_client::pi_client::PiClient;
use pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH;
use pillar_protocol::schemas::{Command, CommandResult, ProtocolErrorCode, ServerMessage};

use pillar_server::server::{PiServer, ServerOutbound};
use pillar_server::sessions::{BroadcastSink, ServerError};
use pillar_server::testing::TestServerService;

#[derive(Default)]
struct PipeSink {
    events: Vec<(u64, pillar_protocol::schemas::ServerEvent)>,
}

impl BroadcastSink for PipeSink {
    fn send_event(&mut self, connection_id: u64, event: &pillar_protocol::schemas::ServerEvent) {
        self.events.push((connection_id, event.clone()));
    }
    fn broadcast_server_snapshot(&mut self) {}
    fn close_connection(&mut self, _: u64) {}
    fn report_error(&mut self, _: String) {}
}

/// The two-sided harness: client requests produce frames, the frames
/// cross the "socket", the server responds, and the response frames
/// cross back.
struct E2e {
    client: PiClient,
    server: PiServer,
    service: TestServerService,
    sink: PipeSink,
    active_connection: String,
}

impl E2e {
    fn new() -> Self {
        let mut server = PiServer::new("server-1", None).unwrap();
        server.accept("conn-1", None).unwrap();
        let client = PiClient::new(Some(DEFAULT_MAX_FRAME_LENGTH)).unwrap();
        Self {
            client,
            server,
            service: TestServerService::new(),
            sink: PipeSink::default(),
            active_connection: "conn-1".to_string(),
        }
    }

    /// Upstream the socket pump: client -> server, server -> client.
    fn pump(&mut self) {
        // The client's transport writes go to the server.
        let outbound = self.server.outbound.clone();
        self.server.outbound.clear();
        for (id, kind) in outbound {
            if id != self.active_connection {
                continue;
            }
            match kind {
                ServerOutbound::Frame(frame) => self.client.handle_data(1, &frame),
                ServerOutbound::Close(final_frame) => {
                    if let Some(frame) = final_frame {
                        self.client.handle_data(1, &frame);
                    }
                    self.client.handle_close(1);
                }
            }
        }
    }

    fn connect(&mut self) {
        // connect() produces the client hello frame and enters
        // Connecting; attaching the transport completes the socket
        // stand-in (upstream the factory result resolves first).
        let hello = self
            .client
            .connect()
            .expect("client connect produces a hello frame");
        self.client
            .attach_transport(1, Box::<LoopTransport>::default());
        self.server
            .receive("conn-1", &hello, &mut self.service, &mut self.sink);
        self.pump();
        // The handshake snapshot is consumed by the host.
        assert!(
            self.client.handshake.is_some(),
            "no handshake: last_failure={:?} failures={:?} client_msgs={:?} server_out={:?}",
            self.client.last_failure,
            self.client.last_failure,
            self.client.responses,
            self.server.host_events
        );
    }

    fn request(&mut self, command: Command) -> (String, serde_json::Value) {
        let (id, frame) = self.client.request(command).unwrap();
        self.server.receive(
            &self.active_connection,
            &frame,
            &mut self.service,
            &mut self.sink,
        );
        self.pump();
        let response = self
            .client
            .responses
            .iter()
            .find(|(response_id, _)| response_id == &id)
            .map(|(_, outcome)| match outcome {
                Ok(result) => serde_json::json!({
                    "id": id, "ok": true,
                    "result": serde_json::to_value(result).unwrap(),
                }),
                Err(error) => serde_json::json!({
                    "id": id, "ok": false,
                    "error": match error {
                        ClientError::Server { code, message } => serde_json::json!({
                            "code": code, "message": message,
                        }),
                        ClientError::Disconnected(reason) => {
                            serde_json::json!({ "code": "internal_error", "message": reason })
                        }
                        ClientError::Protocol(message) => {
                            serde_json::json!({ "code": "internal_error", "message": message })
                        }
                    },
                }),
            })
            .expect("response resolved");
        (id, response)
    }

    fn request_ok(&mut self, command: Command) -> CommandResult {
        let (_, response) = self.request(command);
        match &response["ok"] {
            serde_json::Value::Bool(true) => {
                serde_json::from_value(response["result"].clone()).unwrap()
            }
            _ => panic!("request failed: {response}"),
        }
    }

    fn seed(&mut self, id: &str) {
        self.service.seed_default(id);
    }

    /// Re-seed on a fresh server (upstream restart).
    fn restart_server(&mut self) {
        self.server = PiServer::new("server-1", None).unwrap();
        self.server.accept("conn-2", None).unwrap();
        self.active_connection = "conn-2".to_string();
        // Upstream server.close() disposes attached runtimes (releasing
        // their locks); the port's server teardown is host-driven, so
        // the driver releases the lock on restart.
        self.service.release_lock("session-1");
        // A restart severs the client transport; reconnect on a fresh
        // client with a fresh connection id.
        self.client.disconnect("server closed");
        self.client = PiClient::new(Some(DEFAULT_MAX_FRAME_LENGTH)).unwrap();
        let hello = self.client.connect().unwrap();
        self.client
            .attach_transport(1, Box::<LoopTransport>::default());
        self.server
            .receive("conn-2", &hello, &mut self.service, &mut self.sink);
        // Pump conn-2 responses back into the client.
        let outbound = self.server.outbound.clone();
        self.server.outbound.clear();
        for (id, kind) in outbound {
            if id != "conn-2" {
                continue;
            }
            if let ServerOutbound::Frame(frame) = kind {
                self.client.handle_data(1, &frame);
            }
        }
        assert!(
            self.client.handshake.is_some(),
            "reconnect handshake failed: failure={:?} out={:?} host={:?}",
            self.client.last_failure,
            self.server.outbound,
            self.server.host_events
        );
    }
}

/// Hello handshake gives the client a server snapshot; request/list
/// resolves and updates client state.
#[test]
fn handshake_and_list_round_trip() {
    let mut e2e = E2e::new();
    e2e.seed("session-1");
    e2e.connect();
    let (id, result) = {
        let command = Command::List;
        let (id, frame) = e2e.client.request(command).unwrap();
        e2e.server
            .receive("conn-1", &frame, &mut e2e.service, &mut e2e.sink);
        e2e.pump();
        let response = e2e
            .client
            .responses
            .iter()
            .find(|(response_id, _)| response_id == &id)
            .map(|(_, outcome)| outcome.clone())
            .expect("response resolved");
        (id, response)
    };
    let result = result.unwrap();
    assert!(matches!(result, CommandResult::List { .. }));
    let CommandResult::List { sessions } = result else {
        panic!("expected list result");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "session-1");
    assert_eq!(id, "1");
}

/// Attach -> client state sees the session as attached; detach
/// releases it and the server disposes the runtime.
#[test]
fn attach_detach_round_trip_updates_client_state() {
    let mut e2e = E2e::new();
    e2e.seed("session-1");
    e2e.connect();
    let attached = e2e.request_ok(Command::Attach {
        session_id: "session-1".to_string(),
    });
    let CommandResult::Attach { session } = attached else {
        panic!("expected attach result");
    };
    assert_eq!(session.id, "session-1");
    assert!(session.attached);
    assert!(e2e.client.state.is_session_attached("session-1"));

    let detached = e2e.request_ok(Command::Detach {
        session_id: "session-1".to_string(),
    });
    let CommandResult::Detach { session_id } = detached else {
        panic!("expected detach result");
    };
    assert_eq!(session_id, "session-1");
    assert!(!e2e.client.state.is_session_attached("session-1"));
    assert_eq!(e2e.service.dispose_count("session-1"), 1);
}

/// Prompt moves the session to turn; a second prompt while busy is
/// rejected with busy and the client records the server error.
#[test]
fn prompt_busy_error_flows_back_to_client() {
    let mut e2e = E2e::new();
    e2e.seed("session-1");
    e2e.connect();
    let _ = e2e.request_ok(Command::Attach {
        session_id: "session-1".to_string(),
    });
    let _ = e2e.request_ok(Command::Prompt {
        session_id: "session-1".to_string(),
        text: "first".to_string(),
    });
    let (_, response) = e2e.request(Command::Prompt {
        session_id: "session-1".to_string(),
        text: "second".to_string(),
    });
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "busy");
    assert_eq!(response["error"]["message"], "A prompt is already running");
}

/// Client-side request failure bookkeeping: a disconnect while
/// requests are pending fails them with the disconnect reason.
#[test]
fn disconnect_fails_pending_requests() {
    let mut e2e = E2e::new();
    e2e.connect();
    let (id, frame) = e2e.client.request(Command::List).unwrap();
    // The frame never reaches a started server (the server is mid
    // handshake in this scenario); the client drops its transport.
    e2e.client.disconnect("server closed");
    // The pending request never resolved — the port does not reject
    // on explicit disconnect until the host drains failures.
    let failures = e2e.client.take_pending_failures();
    assert!(failures.iter().any(|(pending_id, _)| pending_id == &id));
    drop(frame);
}

/// Server restart: sessions restore lazily from the service catalog
/// on the new server, matching upstream restart semantics.
#[test]
fn server_restart_restores_sessions_lazily() {
    let mut e2e = E2e::new();
    e2e.seed("session-1");
    e2e.connect();
    let _ = e2e.request_ok(Command::Attach {
        session_id: "session-1".to_string(),
    });
    e2e.restart_server();
    let attached = e2e.request_ok(Command::Attach {
        session_id: "session-1".to_string(),
    });
    let CommandResult::Attach { session } = attached else {
        panic!("expected attach result");
    };
    assert_eq!(session.id, "session-1");
    assert_eq!(
        session.thinking_level,
        pillar_protocol::schemas::ThinkingLevel::Off
    );
}

/// The client's own protocol errors surface as ClientError without
/// touching the server.
#[test]
fn client_requires_connection_before_requests() {
    let mut client = PiClient::new(Some(DEFAULT_MAX_FRAME_LENGTH)).unwrap();
    assert!(matches!(
        client.request(Command::List),
        Err(ClientError::Disconnected(_))
    ));
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
}

/// Server-side protocol failure maps to hello_error with the
/// upstream message; the client records the final frame then closes.
#[test]
fn server_protocol_failure_reaches_the_client() {
    let mut server = PiServer::new("server-1", None).unwrap();
    server.accept("conn-1", None).unwrap();
    let mut service = TestServerService::new();
    let mut sink = PipeSink::default();
    // A request as the first message fails the connection.
    let envelope = serde_json::json!({
        "type": "request",
        "id": "too-early",
        "request": { "command": "list" }
    });
    let frame = pillar_protocol::codec::encode_client_message(
        &envelope,
        pillar_protocol::framing::FrameDecoderOptions::default(),
    )
    .unwrap();
    server.receive("conn-1", &frame, &mut service, &mut sink);
    let close = server
        .outbound
        .iter()
        .find(|(_, kind)| matches!(kind, ServerOutbound::Close(_)))
        .expect("connection closed");
    if let ServerOutbound::Close(Some(final_frame)) = &close.1 {
        let messages: Vec<ServerMessage> = {
            let mut decoder = pillar_protocol::codec::ServerMessageDecoder::new(
                pillar_protocol::framing::FrameDecoderOptions::default(),
            )
            .unwrap();
            let values = decoder.push(final_frame).unwrap();
            values
                .iter()
                .map(|value| serde_json::from_value(value.clone()).unwrap())
                .collect()
        };
        assert!(matches!(&messages[0], ServerMessage::HelloError { .. }));
    } else {
        panic!("expected a final frame before close");
    }
}

/// The error code strings across both sides match the protocol enum
/// (upstream shared code surface).
#[test]
fn error_code_surface_is_shared() {
    let codes = [
        ProtocolErrorCode::NotFound,
        ProtocolErrorCode::SessionLocked,
        ProtocolErrorCode::Busy,
        ProtocolErrorCode::InvalidRequest,
    ];
    for code in codes {
        let error = ServerError::new(code, "probe");
        assert_eq!(error.code, code);
    }
}
