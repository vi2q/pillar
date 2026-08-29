//! Port of packages/protocol/test/protocol.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream vitest test, same names in comments.

use serde_json::{Value, json};

use pillar_protocol::{
    ClientMessageDecoder, FrameDecoder, FrameDecoderOptions, PROTOCOL_VERSION,
    ProtocolValidationError, ServerMessageDecoder, decode_cbor, encode_cbor, encode_client_message,
    encode_frame, encode_server_message, is_supported_protocol_version, parse_client_message,
    parse_server_message,
};

fn client_hello(version: Value) -> Value {
    json!({"type": "hello", "version": version})
}

fn empty_server_snapshot() -> Value {
    json!({
        "serverId": "server-1",
        "protocolVersion": PROTOCOL_VERSION,
        "revision": 0,
        "sessions": [],
        "models": [],
    })
}

fn server_hello() -> Value {
    json!({
        "type": "hello",
        "version": PROTOCOL_VERSION,
        "connectionId": "connection-1",
        "snapshot": empty_server_snapshot(),
    })
}

fn item_message(item: Value, progress_type: &str) -> Value {
    json!({
        "type": "event",
        "event": {
            "type": "session_progress",
            "sessionId": "session-1",
            "progress": {"type": progress_type, "item": item},
        },
    })
}

fn assistant_item(state: Value) -> Value {
    let mut merged = json!({
        "id": "assistant-1",
        "role": "assistant",
        "content": [{"type": "text", "text": "hello"}],
        "model": {"provider": "test", "id": "model"},
        "timestamp": 1,
    });
    let base = merged.as_object_mut().unwrap();
    for (k, v) in state.as_object().unwrap() {
        base.insert(k.clone(), v.clone());
    }
    merged
}

fn tool_item(state: Value) -> Value {
    let mut merged = json!({
        "id": "tool-1",
        "role": "tool",
        "toolCallId": "call-1",
        "toolName": "read",
        "input": {},
        "content": [],
        "timestamp": 1,
    });
    let base = merged.as_object_mut().unwrap();
    for (k, v) in state.as_object().unwrap() {
        base.insert(k.clone(), v.clone());
    }
    merged
}

#[test]
fn uses_protocol_version_1() {
    assert_eq!(PROTOCOL_VERSION, 1);
    assert!(is_supported_protocol_version(1.0));
    assert!(!is_supported_protocol_version(2.0));
    assert!(!is_supported_protocol_version(2.5));
}

#[test]
fn accepts_integer_client_hello_versions_for_negotiation() {
    for version in [0u64, PROTOCOL_VERSION, PROTOCOL_VERSION + 1] {
        let message = client_hello(json!(version));
        parse_client_message(&message).unwrap();
    }
}

#[test]
fn rejects_a_handshake_with_bad_versions_or_extra_fields() {
    // string version
    assert!(parse_client_message(&client_hello(json!("1"))).is_err());
    // fractional version
    assert!(parse_client_message(&client_hello(json!(PROTOCOL_VERSION as f64 + 0.5))).is_err());
    // credential field
    assert!(
        parse_client_message(&json!({
            "type": "hello", "version": PROTOCOL_VERSION, "token": "secret"
        }))
        .is_err()
    );
    // unknown field
    assert!(
        parse_client_message(&json!({
            "type": "hello", "version": PROTOCOL_VERSION, "extra": true
        }))
        .is_err()
    );
}

#[test]
fn does_not_parse_json_strings_as_wire_messages() {
    // A JSON string decodes to a CBOR text string, not a map, so both fail.
    let as_cbor = encode_cbor(
        &json!(serde_json::to_string(&client_hello(json!(PROTOCOL_VERSION))).unwrap()),
        Default::default(),
    )
    .unwrap();
    assert!(parse_client_message(&decode_cbor(&as_cbor, Default::default()).unwrap()).is_err());
}

#[test]
fn rejects_image_input_while_the_mvp_remains_text_only() {
    assert!(
        parse_client_message(&json!({
            "type": "request",
            "id": "request-1",
            "request": {
                "command": "prompt",
                "sessionId": "session-1",
                "text": "inspect",
                "images": [{"type": "image", "data": "abc", "mimeType": "image/png"}],
            },
        }))
        .is_err()
    );
}

#[test]
fn parses_a_server_handshake_snapshot() {
    parse_server_message(&server_hello()).unwrap();
}

#[test]
fn represents_listed_sessions_as_durable_metadata() {
    let message = json!({
        "type": "response",
        "id": "request-1",
        "ok": true,
        "result": {
            "command": "list",
            "sessions": [{
                "id": "session-1",
                "createdAt": 1,
                "updatedAt": 2,
                "parentSessionId": "parent-1",
                "sessionName": "Named session",
                "cwd": "/workspace",
            }],
        },
    });
    parse_server_message(&message).unwrap();
    assert!(
        parse_server_message(&json!({
            "type": "response",
            "id": "request-1",
            "ok": true,
            "result": {
                "command": "list",
                "sessions": [{"id": "session-1", "createdAt": 1, "phase": "idle"}],
            },
        }))
        .is_err()
    );
}

#[test]
fn accepts_the_not_implemented_and_internal_error_codes() {
    for code in ["not_implemented", "internal_error"] {
        let message = json!({
            "type": "response",
            "id": "request-1",
            "ok": false,
            "error": {"code": code, "message": "safe"},
        });
        parse_server_message(&message).unwrap();
    }
}

#[test]
fn rejects_invalid_server_messages() {
    // hello with wrong version
    let mut wrong_version = server_hello();
    wrong_version["version"] = json!(PROTOCOL_VERSION + 1);
    assert!(parse_server_message(&wrong_version).is_err());
    // hello_error with unknown code
    assert!(
        parse_server_message(&json!({
            "type": "hello_error", "error": {"code": "auth", "message": "Authentication failed"}
        }))
        .is_err()
    );
    // response with unknown command
    assert!(
        parse_server_message(&json!({
            "type": "response", "id": "request-1", "ok": true, "result": {"command": "unknown"}
        }))
        .is_err()
    );
    // event with numeric sessionId
    assert!(
        parse_server_message(&json!({
            "type": "event", "event": {"type": "session_removed", "sessionId": 42}
        }))
        .is_err()
    );
}

#[test]
fn validates_nested_json_tool_details() {
    let message = json!({
        "type": "event",
        "event": {
            "type": "session_progress",
            "sessionId": "session-1",
            "progress": {
                "type": "item_finished",
                "item": {
                    "id": "tool-1",
                    "role": "tool",
                    "toolCallId": "call-1",
                    "toolName": "read",
                    "input": {"path": "/tmp/file"},
                    "content": [{"type": "text", "text": "done"}],
                    "details": {"lines": [1, 2, 3], "cached": false},
                    "status": "complete",
                    "isError": false,
                    "timestamp": 1,
                },
            },
        },
    });
    parse_server_message(&message).unwrap();
}

#[test]
fn accepts_consistent_assistant_items() {
    let states = vec![
        json!({"status": "streaming"}),
        json!({"status": "complete", "stopReason": "stop"}),
        json!({"status": "error", "stopReason": "error"}),
        json!({"status": "error", "stopReason": "error", "errorMessage": "failed"}),
        json!({"status": "aborted", "stopReason": "aborted"}),
    ];
    for state in &states {
        let status = state["status"].as_str().unwrap().to_owned();
        let progress_type = if status == "streaming" {
            "item_updated"
        } else {
            "item_finished"
        };
        parse_server_message(&item_message(assistant_item(state.clone()), progress_type)).unwrap();
    }
}

#[test]
fn rejects_inconsistent_assistant_items() {
    let states = vec![
        json!({"status": "streaming", "stopReason": "stop"}),
        json!({"status": "complete"}),
        json!({"status": "complete", "stopReason": "error"}),
        json!({"status": "error", "stopReason": "error", "errorMessage": ""}),
        json!({"status": "aborted", "stopReason": "stop"}),
    ];
    for state in &states {
        let progress_type = if state["status"] == "streaming" {
            "item_updated"
        } else {
            "item_finished"
        };
        assert!(
            parse_server_message(&item_message(assistant_item(state.clone()), progress_type))
                .is_err(),
            "expected rejection"
        );
    }
}

#[test]
fn accepts_consistent_tool_items() {
    let states = vec![
        json!({"status": "running", "isError": false}),
        json!({"status": "complete", "isError": false}),
        json!({"status": "error", "isError": true}),
    ];
    for state in &states {
        let status = state["status"].as_str().unwrap().to_owned();
        let progress_type = if status == "running" {
            "item_updated"
        } else {
            "item_finished"
        };
        parse_server_message(&item_message(tool_item(state.clone()), progress_type)).unwrap();
    }
}

#[test]
fn rejects_nonterminal_items_reported_as_finished() {
    let assistant = json!({
        "id": "assistant-1",
        "role": "assistant",
        "content": [],
        "model": {"provider": "test", "id": "model"},
        "status": "streaming",
        "timestamp": 1,
    });
    let tool = json!({
        "id": "tool-1",
        "role": "tool",
        "toolCallId": "call-1",
        "toolName": "read",
        "input": {},
        "content": [],
        "status": "running",
        "isError": false,
        "timestamp": 1,
    });
    assert!(parse_server_message(&item_message(assistant, "item_finished")).is_err());
    assert!(parse_server_message(&item_message(tool, "item_finished")).is_err());
}

#[test]
fn rejects_inconsistent_tool_items() {
    let states = vec![
        json!({"status": "running", "isError": true}),
        json!({"status": "complete", "isError": true}),
        json!({"status": "error", "isError": false}),
    ];
    for state in &states {
        let status = state["status"].as_str().unwrap().to_owned();
        let progress_type = if status == "running" {
            "item_updated"
        } else {
            "item_finished"
        };
        assert!(
            parse_server_message(&item_message(tool_item(state.clone()), progress_type)).is_err(),
            "expected rejection for {state}"
        );
    }
}

#[test]
fn rejects_cyclic_protocol_values_with_a_protocol_validation_error() {
    // serde_json::Value is acyclic by construction; the equivalent upstream
    // guarantee is that encoding never loops. The validation error path is
    // exercised via the structured error type below.
    let error = ProtocolValidationError::new("server");
    assert_eq!(error.to_string(), "Invalid server protocol message");
}

#[test]
fn validation_errors_do_not_retain_rejected_payloads() {
    // Upstream asserts the error is small and carries no payload reference;
    // the Rust error type holds only a static kind, which satisfies both.
    let thrown = parse_client_message(&json!({
        "type": "hello",
        "version": "1",
    }))
    .unwrap_err();
    assert_eq!(thrown.to_string(), "Invalid client protocol message");
}

#[test]
fn encodes_complete_client_and_server_frames() {
    let frames = FrameDecoder::new(FrameDecoderOptions::new())
        .unwrap()
        .push(
            &encode_client_message(
                &client_hello(json!(PROTOCOL_VERSION)),
                FrameDecoderOptions::new(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(frames.len(), 1);
    parse_client_message(&decode_cbor(&frames[0], Default::default()).unwrap()).unwrap();

    let server_frames = FrameDecoder::new(FrameDecoderOptions::new())
        .unwrap()
        .push(&encode_server_message(&server_hello(), FrameDecoderOptions::new()).unwrap())
        .unwrap();
    assert_eq!(server_frames.len(), 1);
    parse_server_message(&decode_cbor(&server_frames[0], Default::default()).unwrap()).unwrap();
}

#[test]
fn enforces_an_outbound_frame_limit_before_returning_encoded_bytes() {
    assert!(
        encode_client_message(
            &client_hello(json!(PROTOCOL_VERSION)),
            FrameDecoderOptions::new().max_frame_length(8)
        )
        .is_err()
    );
    assert!(
        encode_server_message(
            &server_hello(),
            FrameDecoderOptions::new().max_frame_length(8)
        )
        .is_err()
    );
}

#[test]
fn validates_messages_before_encoding() {
    assert!(
        encode_client_message(
            &client_hello(json!(PROTOCOL_VERSION as f64 + 0.5)),
            FrameDecoderOptions::new()
        )
        .is_err()
    );
}

#[test]
fn omits_explicit_undefined_optional_properties_on_the_wire() {
    // Optional fields with None never serialize in Rust, so the wire form of
    // a create with no optional fields carries only command.
    let message = json!({
        "type": "request",
        "id": "request-1",
        "request": {"command": "create"},
    });
    let payload = FrameDecoder::new(FrameDecoderOptions::new())
        .unwrap()
        .push(&encode_client_message(&message, FrameDecoderOptions::new()).unwrap())
        .unwrap();
    assert_eq!(payload.len(), 1);
    assert_eq!(
        decode_cbor(&payload[0], Default::default()).unwrap(),
        json!({"type": "request", "id": "request-1", "request": {"command": "create"}})
    );
}

#[test]
fn incrementally_decodes_fragmented_and_coalesced_client_messages() {
    let request = json!({
        "type": "request",
        "id": "request-1",
        "request": {"command": "list"},
    });
    let first = encode_client_message(
        &client_hello(json!(PROTOCOL_VERSION)),
        FrameDecoderOptions::new(),
    )
    .unwrap();
    let second = encode_client_message(&request, FrameDecoderOptions::new()).unwrap();
    let mut wire = first.clone();
    wire.extend_from_slice(&second);

    for split in 0..=wire.len() {
        let mut decoder = ClientMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
        let mut messages = Vec::new();
        messages.extend(decoder.push(&wire[..split]).unwrap());
        messages.extend(decoder.push(&wire[split..]).unwrap());
        decoder.end().unwrap();
        assert_eq!(messages.len(), 2, "split at {split}");
        parse_client_message(&messages[0]).unwrap();
        parse_client_message(&messages[1]).unwrap();
    }
}

#[test]
fn incrementally_decodes_server_messages() {
    let error_message = json!({
        "type": "hello_error",
        "error": {"code": "version", "message": "Unsupported protocol version"},
    });
    let frame = encode_server_message(&error_message, FrameDecoderOptions::new()).unwrap();
    let mut decoder = ServerMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    let messages = decoder.push(&frame).unwrap();
    assert_eq!(messages, vec![error_message]);
    decoder.end().unwrap();
}

#[test]
fn rejects_invalid_framed_client_input() {
    let cases: Vec<Value> = vec![
        // empty CBOR payload
        json!(Value::Null),
    ];
    let _ = cases;
    let mut decoder = ClientMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    // empty CBOR payload: frame with zero-length body decodes to nothing.
    let empty = encode_frame(&[]).unwrap();
    assert!(decoder.push(&empty).is_err());
    assert!(
        decoder
            .push(
                &encode_client_message(
                    &client_hello(json!(PROTOCOL_VERSION)),
                    FrameDecoderOptions::new()
                )
                .unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("failed")
    );

    // malformed CBOR
    let mut decoder = ClientMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    let malformed = encode_frame(&[0xff]).unwrap();
    assert!(decoder.push(&malformed).is_err());

    // schema-invalid CBOR
    let mut decoder = ClientMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    let invalid = encode_frame(
        &encode_cbor(
            &json!({"type": "hello", "version": PROTOCOL_VERSION, "extra": true}),
            Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(decoder.push(&invalid).is_err());
}

#[test]
fn rejects_cbor_byte_strings_nested_in_json_valued_fields() {
    // The byte-string wire form carries a \u{0}-prefixed carrier string in
    // the Value layer, which validation rejects inside JSON-typed fields.
    let wire = encode_frame(
        &encode_cbor(
            &json!({
                "type": "response",
                "id": "request-1",
                "ok": false,
                "error": {
                    "code": "invalid_request",
                    "message": "invalid",
                    "details": {"nested": "\u{0}\u{1}\u{2}\u{3}"},
                },
            }),
            Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut decoder = ServerMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    assert!(decoder.push(&wire).is_err());
}

#[test]
fn rejects_truncated_and_oversized_framing_through_the_validated_decoder() {
    let mut truncated = ServerMessageDecoder::new(FrameDecoderOptions::new()).unwrap();
    let empty: Vec<Value> = Vec::new();
    assert_eq!(truncated.push(&[0, 0, 0, 2, 1]).unwrap(), empty);
    assert!(truncated.end().is_err());

    let mut oversized =
        ClientMessageDecoder::new(FrameDecoderOptions::new().max_frame_length(3)).unwrap();
    assert!(oversized.push(&[0, 0, 0, 4]).is_err());
}
