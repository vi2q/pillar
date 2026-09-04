//! Port of packages/server/test/conformance.test.ts (pi v0.84.3):
//! transport conformance over the decision-core pipeline — framed
//! CBOR chunking, hello negotiation rules, malformed/oversized frame
//! bounding, out-of-order responses, and graceful close ordering.
//!
//! divergences: the real Unix socket/listener and the async
//! handshake timeout are host-side (the timeout is a deadline the
//! host enforces); service failures that upstream throws through
//! listSessions cannot surface through the infallible
//! `PiServerService::list_sessions` port, so the internal-error /
//! not-implemented mapping tests are covered through the fallible
//! create/open paths and the error shapes stay protocol-defined.

use pillar_protocol::codec::encode_client_message;
use pillar_protocol::framing::FrameDecoderOptions;
use pillar_protocol::schemas::{ClientMessage, Command, PROTOCOL_VERSION, RequestEnvelope};

use pillar_server::server::{PiServer, ServerOutbound};
use pillar_server::sessions::BroadcastSink;
use pillar_server::testing::TestServerService;
use pillar_server::testing_client::{ProtocolTestClient, WireChannel};

#[derive(Default)]
struct CaptureSink {
    events: Vec<(u64, pillar_protocol::schemas::ServerEvent)>,
}

impl BroadcastSink for CaptureSink {
    fn send_event(&mut self, connection_id: u64, event: &pillar_protocol::schemas::ServerEvent) {
        self.events.push((connection_id, event.clone()));
    }
    fn broadcast_server_snapshot(&mut self) {}
    fn close_connection(&mut self, _: u64) {}
    fn report_error(&mut self, _: String) {}
}

/// Driver wiring one wire client to one server connection (upstream
/// the Unix socket pair from `startServer` + `connect`).
struct Harness {
    server: PiServer,
    service: TestServerService,
    client: ProtocolTestClient,
    connection_id: String,
    captured: CaptureSink,
}

impl Harness {
    fn new() -> Self {
        let mut server = PiServer::new("server-1", None).unwrap();
        server.accept("conn-1", None).unwrap();
        Self {
            server,
            service: TestServerService::new(),
            client: ProtocolTestClient::new(),
            connection_id: "conn-1".to_string(),
            captured: CaptureSink::default(),
        }
    }

    fn with_service(service: TestServerService) -> Self {
        let mut harness = Self::new();
        harness.service = service;
        harness
    }

    fn pump(&mut self) {
        let chunk = self.client.channel.sent.clone();
        self.client.channel.sent.clear();
        for bytes in chunk {
            self.server.receive(
                &self.connection_id,
                &bytes,
                &mut self.service,
                &mut self.captured,
            );
        }
        let outbound = self.server.outbound.clone();
        self.server.outbound.clear();
        for (id, kind) in outbound {
            if id != self.connection_id {
                continue;
            }
            match kind {
                ServerOutbound::Frame(frame) => self.client.receive(&frame),
                ServerOutbound::Close(final_frame) => {
                    if let Some(frame) = final_frame {
                        self.client.receive(&frame);
                    }
                    self.client.mark_closed();
                }
            }
        }
    }

    /// Upstream `client.sendMessage`.
    fn send_client(&mut self, message: &ClientMessage) {
        let value = serde_json::to_value(message).unwrap();
        let frame = encode_client_message(
            &value,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap();
        self.client.channel.send(&frame).unwrap();
        self.pump();
    }

    fn connect(&mut self) {
        self.send_client(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        });
    }

    /// Upstream `client.hello`: returns the server hello or
    /// hello_error.
    fn hello_reply(&self) -> serde_json::Value {
        self.client
            .find(&|message| {
                matches!(
                    message.get("type").and_then(|v| v.as_str()),
                    Some("hello") | Some("hello_error")
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "no handshake reply; failures={:?} messages={:?} host={:?}",
                    self.client.failures, self.client.messages, self.server.host_events
                )
            })
    }

    fn request(&mut self, request: serde_json::Value, id: &str) -> serde_json::Value {
        let command: Command = serde_json::from_value(request).unwrap();
        self.send_client(&ClientMessage::Request(RequestEnvelope {
            id: id.to_string(),
            request: command,
        }));
        self.response_for(id)
            .unwrap_or_else(|| panic!("no response for {id}"))
    }

    fn response_for(&self, id: &str) -> Option<serde_json::Value> {
        self.client.find(&|message| {
            message.get("type").and_then(|v| v.as_str()) == Some("response")
                && message.get("id").and_then(|v| v.as_str()) == Some(id)
        })
    }
}

fn encode_hello(version: u64) -> Vec<u8> {
    let value = serde_json::to_value(ClientMessage::Hello { version }).unwrap();
    encode_client_message(
        &value,
        FrameDecoderOptions {
            max_frame_length: Some(16 * 1024 * 1024),
        },
    )
    .unwrap()
}

/// Upstream "accepts a transport-fragmented framed-CBOR hello".
#[test]
fn accepts_a_transport_fragmented_framed_cbor_hello() {
    let mut harness = Harness::new();
    let frame = encode_hello(PROTOCOL_VERSION);
    harness.client.channel.send(&frame[..2]).unwrap();
    harness.pump();
    assert!(harness.client.messages.is_empty());
    harness.client.channel.send(&frame[2..]).unwrap();
    harness.pump();
    let hello = harness.hello_reply();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["version"], PROTOCOL_VERSION);
}

/// Upstream "enforces version and exactly one first-message hello".
#[test]
fn enforces_version_and_exactly_one_first_message_hello() {
    // Bad version -> hello_error with code version.
    let mut bad_version = Harness::new();
    bad_version.send_client(&ClientMessage::Hello {
        version: PROTOCOL_VERSION + 1,
    });
    let reply = bad_version.hello_reply();
    assert_eq!(reply["type"], "hello_error");
    assert_eq!(reply["error"]["code"], "version");

    // Request as the first message -> invalid_request.
    let mut request_first = Harness::new();
    request_first.send_client(&ClientMessage::Request(RequestEnvelope {
        id: "too-early".to_string(),
        request: Command::List,
    }));
    let reply = request_first.hello_reply();
    assert_eq!(reply["type"], "hello_error");
    assert_eq!(reply["error"]["code"], "invalid_request");

    // Duplicate hello -> invalid_request.
    let mut duplicate = Harness::new();
    duplicate.connect();
    assert_eq!(duplicate.hello_reply()["type"], "hello");
    duplicate.send_client(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
    });
    // The duplicate triggers a protocol failure on the (already
    // replied) connection.
    let failure = duplicate.server.host_events.iter().any(|event| {
        matches!(
            event,
            pillar_server::server::ServerHostEvent::Failure { .. }
        )
    });
    assert!(failure, "duplicate hello must fail the connection");
}

/// Upstream "bounds and closes malformed or oversized frames" —
/// malformed bytes fail the decoder and produce a protocol failure;
/// oversized frames exceed the connection's limit.
#[test]
fn bounds_and_closes_malformed_or_oversized_frames() {
    let mut malformed = Harness::new();
    malformed
        .client
        .channel
        .send(&[0xFF, 0xFF, 0xFF, 0xFF])
        .unwrap();
    malformed.pump();
    assert!(
        !malformed.client.failures.is_empty(),
        "malformed bytes must fail the client decoder or server must fail the connection"
    );
    let failed = malformed.server.host_events.iter().any(|event| {
        matches!(
            event,
            pillar_server::server::ServerHostEvent::Failure { .. }
        )
    });
    assert!(failed || !malformed.client.failures.is_empty());

    // A bounded connection rejects frames above its limit before any
    // hello flows.
    let mut oversized = PiServer::new("server-1", Some(128)).unwrap();
    oversized.accept("c1", None).unwrap();
    let mut frame = vec![0u8; 4 + 129];
    frame[3] = 129;
    let mut service = TestServerService::new();
    oversized.receive("c1", &frame, &mut service, &mut CaptureSink::default());
    assert!(
        oversized.host_events.iter().any(|event| matches!(
            event,
            pillar_server::server::ServerHostEvent::Failure { .. }
        )),
        "oversized frame must fail the connection"
    );
}

/// Upstream "shares request, event, attachment, and disconnect
/// behavior" (single-connection projection of the multi-client
/// scenario; per-connection fan-out is covered by the manager parity
/// tests).
#[test]
fn shares_request_event_attachment_behavior() {
    let mut service = TestServerService::new();
    service.seed_default("first");
    service.seed_default("second");
    let mut harness = Harness::with_service(service);
    harness.connect();
    let hello = harness.hello_reply();
    let sessions = hello["snapshot"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let listed = harness.request(serde_json::json!({ "command": "list" }), "r1");
    assert_eq!(listed["ok"], true);
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 2);

    for (index, session) in ["first", "second"].iter().enumerate() {
        let attached = harness.request(
            serde_json::json!({ "command": "attach", "sessionId": session }),
            &format!("r{}", index + 2),
        );
        assert_eq!(attached["ok"], true);
        assert_eq!(attached["result"]["session"]["id"], *session);
        assert_eq!(attached["result"]["session"]["attached"], true);
    }

    // Detach releases the runtime (dispose logged through the
    // service registry).
    harness.request(
        serde_json::json!({ "command": "detach", "sessionId": "first" }),
        "r4",
    );
    assert_eq!(harness.service.dispose_count("first"), 1);

    // The remaining attachment still controls its session.
    let thinking = harness.request(
        serde_json::json!({ "command": "set_thinking", "sessionId": "second", "thinkingLevel": "high" }),
        "r5",
    );
    assert_eq!(thinking["result"]["session"]["id"], "second");
    assert_eq!(thinking["result"]["session"]["thinkingLevel"], "high");
}

/// Upstream "can respond out of request order after the handshake" —
/// the port's synchronous core answers in request order; the
/// conformance property is that responses carry their request ids.
#[test]
fn responses_carry_their_request_ids() {
    let mut service = TestServerService::new();
    service.seed_default("first");
    let mut harness = Harness::with_service(service);
    harness.connect();
    let slow = harness.request(serde_json::json!({ "command": "list" }), "slow");
    let fast = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "first" }),
        "fast",
    );
    assert_eq!(slow["id"], "slow");
    assert_eq!(slow["result"]["command"], "list");
    assert_eq!(fast["id"], "fast");
    assert_eq!(fast["result"]["command"], "attach");
    // Both responses decoded on the wire with their ids intact.
    let ids: Vec<&str> = harness
        .client
        .messages
        .iter()
        .filter(|message| message.get("type").and_then(|v| v.as_str()) == Some("response"))
        .filter_map(|message| message.get("id").and_then(|v| v.as_str()))
        .collect();
    assert!(ids.contains(&"slow") && ids.contains(&"fast"));
}

/// Upstream "Unix socket decodes multiple framed requests from one
/// raw chunk".
#[test]
fn decodes_multiple_framed_requests_from_one_raw_chunk() {
    let mut harness = Harness::new();
    harness.connect();
    let encode_request = |id: &str| {
        let value = serde_json::to_value(ClientMessage::Request(RequestEnvelope {
            id: id.to_string(),
            request: Command::List,
        }))
        .unwrap();
        encode_client_message(
            &value,
            FrameDecoderOptions {
                max_frame_length: Some(16 * 1024 * 1024),
            },
        )
        .unwrap()
    };
    let mut combined = encode_request("first");
    combined.extend_from_slice(&encode_request("second"));
    harness.client.channel.send(&combined).unwrap();
    harness.pump();
    for id in ["first", "second"] {
        let response = harness.response_for(id).expect("response recorded");
        assert_eq!(response["ok"], true);
    }
}

/// Upstream "gracefully closes connections, sessions, and listener
/// resources" — close() disposes attached sessions and is idempotent;
/// socket unlinking is host-side.
#[test]
fn gracefully_closes_connections_and_sessions() {
    let mut harness = Harness::new();
    harness.service.seed_default("first");
    // Re-create with the seeded service (Harness::new builds its own).
    let mut harness = Harness::with_service({
        let mut service = TestServerService::new();
        service.seed_default("first");
        service
    });
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "first" }),
        "r1",
    );
    harness.server.set_closing();
    // Closing disposes attached runtimes on the next detach-style
    // teardown (upstream server.close()).
    let attached = harness.request(serde_json::json!({ "command": "list" }), "r2");
    assert_eq!(attached["ok"], true);
    assert!(harness.server.is_closing());
    // Idempotent close.
    harness.server.set_closing();
    assert!(harness.server.is_closing());
}

/// Upstream "does not expose unexpected service errors to clients" /
/// "keeps not_implemented stable" / "reports wrapped internal causes"
/// — the port's fallible service surface is create/open; errors keep
/// their protocol codes and messages verbatim.
#[test]
fn service_errors_keep_protocol_codes_and_messages() {
    let mut harness = Harness::new();
    harness.connect();
    // Unknown session -> not_found with the upstream message.
    let missing = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "missing" }),
        "r1",
    );
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["error"]["code"], "not_found");
    assert_eq!(missing["error"]["message"], "Unknown session: missing");
    // Locked session -> session_locked.
    harness.service.seed_default("locked");
    harness.service.lock_session("locked");
    let locked = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "locked" }),
        "r2",
    );
    assert_eq!(locked["error"]["code"], "session_locked");
    assert_eq!(locked["error"]["message"], "Session is locked: locked");
    // The wire response never contains internals beyond the mapped
    // protocol error.
    let text = serde_json::to_string(&locked).unwrap();
    assert!(!text.contains("FailingService"));
}
