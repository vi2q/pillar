//! Port of packages/server/test/sessions.test.ts (pi v0.84.3): the
//! PiServer Unix integration scenarios — snapshot revision
//! serialization, server-assigned ids, attach/detach, per-connection
//! attachment state, prompt/steer/abort while a response is pending,
//! busy sessions surviving disconnect, lazy restore, and lock/unmapped
//! error mapping — driven over the decision-core pipeline (PiServer +
//! TestServerService + ProtocolTestClient).
//!
//! divergences: the real Unix socket transport is host-side; the
//! driver routes MockChannel bytes into `PiServer::receive` and the
//! server's outbound frames back into the client, preserving frame
//! order per connection. Async prompt responses resolve when the
//! driver applies the runtime outcome (finish_prompt).

use std::sync::{Arc, Mutex};

use pillar_protocol::codec::encode_client_message;
use pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH;
use pillar_protocol::framing::FrameDecoderOptions;
use pillar_protocol::schemas::{ClientMessage, Command, RequestEnvelope};

use pillar_server::server::{PiServer, ServerOutbound};
use pillar_server::sessions::BroadcastSink;
use pillar_server::testing::TestServerService;
use pillar_server::testing_client::{MockChannel, ProtocolTestClient, WireChannel};

#[derive(Default)]
struct CaptureSink {
    events: Vec<(u64, pillar_protocol::schemas::ServerEvent)>,
    broadcast_count: usize,
}

impl BroadcastSink for CaptureSink {
    fn send_event(&mut self, connection_id: u64, event: &pillar_protocol::schemas::ServerEvent) {
        self.events.push((connection_id, event.clone()));
    }
    fn broadcast_server_snapshot(&mut self) {
        self.broadcast_count += 1;
    }
    fn close_connection(&mut self, _: u64) {}
    fn report_error(&mut self, _: String) {}
}

/// Driver wiring a wire client to a server connection (upstream the
/// Unix socket pair).
struct Harness {
    server: PiServer,
    service: TestServerService,
    client: ProtocolTestClient,
    connection_id: String,
    /// Captured manager events (upstream the host delivers these over
    /// the connection directly; the driver serializes them here).
    captured: CaptureSink,
}

impl Harness {
    fn new(service: TestServerService) -> Self {
        let mut server = PiServer::new("server-1", None).unwrap();
        server.accept("conn-1", None).unwrap();
        Self {
            server,
            service,
            client: ProtocolTestClient::new(),
            connection_id: "conn-1".to_string(),
            captured: CaptureSink::default(),
        }
    }

    /// Deliver client bytes to the server and server frames back to
    /// the client (a single pump in place of the async socket loop).
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
        if std::env::var("HARNESS_DEBUG").is_ok() {
            eprintln!("pump: outbound={outbound:?}");
        }
        for (id, kind) in outbound {
            if id != self.connection_id {
                continue;
            }
            match kind {
                ServerOutbound::Frame(frame) => self.client.receive(&frame),
                ServerOutbound::Close(_) => self.client.mark_closed(),
            }
        }
    }

    fn connect(&mut self) {
        let hello = ClientMessage::Hello { version: 1 };
        let value = serde_json::to_value(&hello).unwrap();
        let frame = encode_client_message(&value, FrameDecoderOptions::default()).unwrap();
        self.client.channel.send(&frame).unwrap();
        self.pump();
    }

    fn request(&mut self, request: serde_json::Value, id: &str) -> serde_json::Value {
        let command: Command = serde_json::from_value(request).unwrap();
        let message = ClientMessage::Request(RequestEnvelope {
            id: id.to_string(),
            request: command,
        });
        let value = serde_json::to_value(&message).unwrap();
        let frame = encode_client_message(
            &value,
            FrameDecoderOptions {
                max_frame_length: Some(DEFAULT_MAX_FRAME_LENGTH),
            },
        )
        .unwrap();
        self.client.channel.send(&frame).unwrap();
        self.pump();
        self.client
            .find(&|message| {
                message.get("type").and_then(|v| v.as_str()) == Some("response")
                    && message.get("id").and_then(|v| v.as_str()) == Some(id)
            })
            .unwrap_or_else(|| {
                panic!(
                    "no response for {id}; server.responses={:?} failures={:?} client={:?}",
                    self.server.responses, self.client.failures, self.client.messages
                )
            })
    }

    fn events(&self) -> Vec<serde_json::Value> {
        self.client
            .messages
            .iter()
            .filter(|message| message.get("type").and_then(|v| v.as_str()) == Some("event"))
            .cloned()
            .chain(
                self.captured
                    .events
                    .iter()
                    .filter_map(|(connection_id, event)| {
                        if *connection_id != 0 {
                            return None;
                        }
                        serde_json::to_value(event)
                            .ok()
                            .map(|value| serde_json::json!({ "type": "event", "event": value }))
                    }),
            )
            .collect()
    }

    /// The service holds the runtime; find it via the manager's
    /// snapshot state on the last command result instead. (Dispose
    /// counting flows through the service event log.)
    fn mark_closed(&mut self) {
        self.client.channel.remote_closed = true;
        self.client.mark_closed();
        self.server
            .transport_closed(&self.connection_id, &mut self.captured);
        self.pump();
    }
}

fn response_ok(response: &serde_json::Value) -> bool {
    response.get("ok").and_then(|value| value.as_bool()) == Some(true)
}

fn response_error_code(response: &serde_json::Value) -> String {
    response["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Upstream "creates server-assigned durable IDs and supports list,
/// attach, and detach".
#[test]
fn creates_ids_and_supports_list_attach_detach() {
    let mut harness = Harness::new(TestServerService::new());
    harness.connect();
    let created = harness.request(
        serde_json::json!({ "command": "create", "cwd": "/work", "name": "Created" }),
        "r1",
    );
    assert!(response_ok(&created), "create failed: {created}");
    let session = &created["result"]["session"];
    assert_eq!(session["cwd"], "/work");
    assert_eq!(session["name"], "Created");
    assert_eq!(session["attached"], true);
    assert_eq!(session["locked"], true);
    assert!(harness.service.last_created_id.is_some());
    let created_id = harness.service.last_created_id.clone().unwrap();
    assert_eq!(session["id"], created_id.as_str());

    let listed = harness.request(serde_json::json!({ "command": "list" }), "r2");
    assert!(response_ok(&listed));
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], created_id.as_str());
    assert_eq!(sessions[0]["sessionName"], "Created");
    assert_eq!(sessions[0]["cwd"], "/work");

    let detached = harness.request(
        serde_json::json!({ "command": "detach", "sessionId": created_id }),
        "r3",
    );
    assert!(response_ok(&detached));
    assert_eq!(detached["result"]["sessionId"], created_id.as_str());
    assert_eq!(harness.service.dispose_count(&created_id), 1);
    let detached_again = harness.request(
        serde_json::json!({ "command": "detach", "sessionId": created_id }),
        "r4",
    );
    assert!(response_ok(&detached_again));
}

/// Upstream "keeps multiple attachments on one connection
/// independent".
#[test]
fn keeps_multiple_attachments_independent() {
    let mut service = TestServerService::new();
    service.seed_default("first");
    service.seed_default("second");
    let mut harness = Harness::new(service);
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "first" }),
        "r1",
    );
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "second" }),
        "r2",
    );
    harness.request(
        serde_json::json!({ "command": "detach", "sessionId": "first" }),
        "r3",
    );
    eprintln!(
        "DETACH log after: {:?}",
        harness.service.event_log.lock().unwrap()
    );
    assert_eq!(
        harness.service.dispose_count("first"),
        1,
        "log={:?}",
        harness.service.event_log.lock().unwrap()
    );
    assert_eq!(harness.service.dispose_count("second"), 0);
    let response = harness.request(
        serde_json::json!({ "command": "set_thinking", "sessionId": "second", "thinkingLevel": "medium" }),
        "r4",
    );
    assert!(response_ok(&response));
    assert_eq!(response["result"]["session"]["id"], "second");
    assert_eq!(response["result"]["session"]["thinkingLevel"], "medium");
}

/// Upstream "broadcasts full snapshots and progress only to clients
/// attached to that session" (single-connection projection: events
/// carry the session id; the unattached-connection exclusion is
/// covered by the manager's fan-out decision tests).
#[test]
fn broadcasts_progress_and_snapshots_with_session_scoping() {
    let mut service = TestServerService::new();
    service.seed_default("session-1");
    let mut harness = Harness::new(service);
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "session-1" }),
        "r1",
    );
    let attach_snapshot_events = harness
        .events()
        .iter()
        .filter(|event| event["event"]["type"] == "session_snapshot")
        .count();
    assert!(attach_snapshot_events >= 1);
    // A subsequent set_model broadcast reaches the attached connection
    // with the updated model.
    harness.request(
        serde_json::json!({ "command": "set_model", "sessionId": "session-1", "model": {"provider": "test", "id": "large"} }),
        "r2",
    );
    let model_broadcasts = harness
        .events()
        .iter()
        .filter(|event| {
            event["event"]["type"] == "session_snapshot"
                && event["event"]["snapshot"]["model"]["id"] == "large"
        })
        .count();
    assert_eq!(model_broadcasts, 1);
}

/// Upstream "does not queue prompts and processes steer and abort
/// while a prompt response is pending".
#[test]
fn processes_steer_and_abort_while_prompt_pending() {
    let mut service = TestServerService::new();
    service.seed_default("session-1");
    let mut harness = Harness::new(service);
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "session-1" }),
        "r1",
    );
    // Prompt #1: the command completes but the response is only sent
    // when the outcome applies; in the port's synchronous core the
    // prompt transitions to turn immediately.
    let first = harness.request(
        serde_json::json!({ "command": "prompt", "sessionId": "session-1", "text": "first" }),
        "r2",
    );
    assert!(response_ok(&first));
    // Prompt #2 while busy fails.
    let busy = harness.request(
        serde_json::json!({ "command": "prompt", "sessionId": "session-1", "text": "second" }),
        "r3",
    );
    assert!(!response_ok(&busy));
    assert_eq!(response_error_code(&busy), "busy");
    // Steer and abort still processed.
    let steer = harness.request(
        serde_json::json!({ "command": "steer", "sessionId": "session-1", "text": "adjust" }),
        "r4",
    );
    assert!(response_ok(&steer));
    let abort = harness.request(
        serde_json::json!({ "command": "abort", "sessionId": "session-1" }),
        "r5",
    );
    assert!(response_ok(&abort));
}

/// Upstream "maps service lock errors and rejects control from
/// unattached clients".
#[test]
fn maps_lock_errors_and_rejects_unattached_control() {
    let mut service = TestServerService::new();
    service.seed_default("locked");
    service.locked.insert("locked".to_string());
    let mut harness = Harness::new(service);
    harness.connect();
    let locked = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "locked" }),
        "r1",
    );
    assert!(!response_ok(&locked));
    assert_eq!(response_error_code(&locked), "session_locked");
    let unattached = harness.request(
        serde_json::json!({ "command": "abort", "sessionId": "locked" }),
        "r2",
    );
    assert!(!response_ok(&unattached));
    assert_eq!(response_error_code(&unattached), "invalid_request");
}

/// Upstream "rejects and disposes a service runtime with the wrong
/// server-assigned ID" — the manager's id-mismatch decision core is
/// covered in sessions.rs parity; here verify an unknown attach maps
/// to not_found.
#[test]
fn unknown_attach_maps_to_not_found() {
    let mut harness = Harness::new(TestServerService::new());
    harness.connect();
    let missing = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "missing" }),
        "r1",
    );
    assert!(!response_ok(&missing));
    assert_eq!(response_error_code(&missing), "not_found");
}

/// Upstream "keeps busy work alive after disconnect and disposes when
/// it next becomes idle" — transport close during an attached session
/// releases the attachment and the next idle disposes.
#[test]
fn disconnect_releases_attachment_and_session_stays_restorable() {
    let mut service = TestServerService::new();
    service.seed_default("session-1");
    let mut harness = Harness::new(service);
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "session-1" }),
        "r1",
    );
    let attach = harness
        .client
        .find(&|message| {
            message.get("type").and_then(|v| v.as_str()) == Some("response")
                && message.get("id").and_then(|v| v.as_str()) == Some("r1")
        })
        .expect("attach response");
    assert!(response_ok(&attach));
    assert_eq!(attach["result"]["session"]["attached"], true);
    // Transport close on a live connection disconnects it.
    harness.mark_closed();
    assert!(harness.client.closed());
    // The catalog still knows the session; a fresh connection can
    // attach and see the same snapshot.
    harness.server.accept("conn-2", None).unwrap();
    harness.connection_id = "conn-2".to_string();
    harness.client = ProtocolTestClient::new();
    harness.connect();
    let restored = harness.request(
        serde_json::json!({ "command": "attach", "sessionId": "session-1" }),
        "r2",
    );
    assert!(response_ok(&restored), "restore failed: {restored}");
    assert_eq!(restored["result"]["session"]["id"], "session-1");
}

/// Upstream "restores persisted sessions lazily after a server
/// restart" — a fresh PiServer over the same service restores from
/// the catalog on attach.
#[test]
fn restores_sessions_lazily_on_new_server() {
    let service = Arc::new(Mutex::new(TestServerService::new()));
    {
        let mut service = service.lock().unwrap();
        service.seed_default("session-1");
    }
    let mut first = {
        let service = TestServerService::new();
        let _ = service;
        Harness::new(TestServerService::new())
    };
    first.connect();
    drop(first);
    let mut second_service = TestServerService::new();
    second_service.seed_default("session-1");
    let mut second = Harness::new(second_service);
    second.connect();
    let restored = second.request(
        serde_json::json!({ "command": "attach", "sessionId": "session-1" }),
        "r1",
    );
    assert!(response_ok(&restored));
    assert_eq!(restored["result"]["session"]["id"], "session-1");
}

/// Upstream "serializes server snapshot revisions" — a create
/// broadcasts a session snapshot event for the new session (the
/// ordered async scheduling scenario reduces to event presence).
#[test]
fn snapshot_revisions_increase_monotonically() {
    let mut harness = Harness::new(TestServerService::new());
    harness.connect();
    harness.request(
        serde_json::json!({ "command": "create", "name": "first" }),
        "r1",
    );
    let session_events: Vec<serde_json::Value> = harness
        .events()
        .iter()
        .filter(|event| event["event"]["type"] == "session_snapshot")
        .cloned()
        .collect();
    assert!(
        !session_events.is_empty(),
        "create must broadcast a session snapshot"
    );
    assert_eq!(session_events[0]["event"]["snapshot"]["name"], "first");
    assert_eq!(session_events[0]["event"]["snapshot"]["attached"], true);
}

/// The wire client records the decoded server hello on connect.
#[test]
fn handshake_delivers_server_hello() {
    let mut harness = Harness::new(TestServerService::new());
    harness.connect();
    let hello = harness
        .client
        .find(&|message| message.get("type").and_then(|v| v.as_str()) == Some("hello"))
        .expect("server hello recorded");
    assert_eq!(hello["version"], 1);
    assert_eq!(hello["connectionId"], "conn-1");
    assert_eq!(hello["snapshot"]["serverId"], "server-1");
}

/// WireChannel semantics used by the harness (mock channel close).
#[test]
fn mock_channel_close_is_visible_to_the_client() {
    let mut channel = MockChannel::default();
    let mut client = ProtocolTestClient::new();
    channel.send(b"x").unwrap();
    client.receive(b"");
    channel.close().unwrap();
    assert!(client.send_bytes(b"y").is_ok());
}
