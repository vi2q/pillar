//! Port of packages/server/src/testing/client.ts (pi v0.84.3): the
//! wire-level protocol test client — a framed channel, a server
//! message decoder, recorded messages, predicate-based waiters, and
//! request/hello helpers.
//!
//! divergences: promises become synchronous polling — `next`
//! (`wait_for`) checks the recorded buffer and returns a pending
//! marker instead of parking a waiter; the socket channel becomes an
//! in-memory `MockChannel` that captures sent bytes and supports
//! fragmentation plus remote close; `connectUnixTestClient` stays
//! host-side (real sockets are a host concern).

use pillar_protocol::ProtocolValidationError;
use pillar_protocol::codec::{ServerMessageDecoder, encode_client_message};
use pillar_protocol::framing::FrameDecoderOptions;
use pillar_protocol::schemas::ClientMessage;
use serde_json::Value;

/// A test transport (upstream `WireChannel`).
pub trait WireChannel {
    fn send(&mut self, chunk: &[u8]) -> Result<(), String>;
    /// Upstream `sendFragmented`: split a write to exercise partial
    /// frame reassembly.
    fn send_fragmented(&mut self, chunk: &[u8], split_at: usize) -> Result<(), String>;
    fn close(&mut self) -> Result<(), String>;
}

/// In-memory channel capturing writes (the port's stand-in for the
/// socket; the driver routes bytes into the server under test).
#[derive(Default)]
pub struct MockChannel {
    pub sent: Vec<Vec<u8>>,
    pub closed: bool,
    pub remote_closed: bool,
}

impl WireChannel for MockChannel {
    fn send(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.closed || self.remote_closed {
            return Err("channel is closed".to_string());
        }
        self.sent.push(chunk.to_vec());
        Ok(())
    }

    fn send_fragmented(&mut self, chunk: &[u8], split_at: usize) -> Result<(), String> {
        if split_at > chunk.len() {
            return Err("split point beyond chunk".to_string());
        }
        self.send(&chunk[..split_at])?;
        self.send(&chunk[split_at..])
    }

    fn close(&mut self) -> Result<(), String> {
        self.closed = true;
        Ok(())
    }
}

/// Outcome of a [`ProtocolTestClient::wait_for`] scan.
pub enum WaitResult {
    /// A recorded message matched the predicate.
    Found(Value),
    /// No match yet; keep polling after more bytes arrive.
    Pending,
}

/// Wire-level protocol test client (upstream `ProtocolTestClient`).
pub struct ProtocolTestClient {
    pub messages: Vec<Value>,
    /// The wire channel (upstream held in the constructor).
    pub channel: MockChannel,
    channel_closed: bool,
    decoder: ServerMessageDecoder,
    request_sequence: usize,
    pending_send: Option<Vec<u8>>,
    /// Errors raised while decoding; the driver drains them (upstream
    /// fail() rejects waiters).
    pub failures: Vec<String>,
}

impl ProtocolTestClient {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            channel: MockChannel::default(),
            channel_closed: false,
            decoder: ServerMessageDecoder::new(FrameDecoderOptions::default())
                .expect("default frame options are valid"),
            request_sequence: 0,
            pending_send: None,
            failures: Vec::new(),
        }
    }

    pub fn closed(&self) -> bool {
        self.channel_closed
    }

    /// Upstream `receive`: feed inbound bytes through the decoder and
    /// record each message.
    pub fn receive(&mut self, chunk: &[u8]) {
        match self.decoder.push(chunk) {
            Ok(messages) => self.messages.extend(messages),
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Upstream `markClosed`.
    pub fn mark_closed(&mut self) {
        if self.channel_closed {
            return;
        }
        self.channel_closed = true;
        self.fail("Wire connection closed".to_string());
    }

    /// Upstream `fail`: reject outstanding waiters (the port records
    /// the failure for the driver).
    pub fn fail(&mut self, error: String) {
        self.failures.push(error);
    }

    /// Upstream `hello`: send a client hello and wait for the server
    /// hello / hello_error reply.
    pub fn hello(&mut self, version: u64) -> Result<Value, WaitError> {
        let reply = self.wait_for(&|message| {
            message.get("type").and_then(|value| value.as_str()) == Some("hello")
                || message.get("type").and_then(|value| value.as_str()) == Some("hello_error")
        })?;
        self.send_message(&ClientMessage::Hello { version })?;
        Ok(reply)
    }

    /// Upstream `request`: register the expected response, send the
    /// request envelope, and wait for the matching response.
    pub fn request(
        &mut self,
        request: serde_json::Value,
        id: Option<&str>,
    ) -> Result<Value, WaitError> {
        self.request_sequence += 1;
        let id = id
            .map(|value| value.to_string())
            .unwrap_or_else(|| format!("request-{}", self.request_sequence));
        let message = ClientMessage::Request(pillar_protocol::schemas::RequestEnvelope {
            id: id.clone(),
            request: serde_json::from_value(request)
                .map_err(|error| WaitError::Encode(error.to_string()))?,
        });
        let response = self.wait_for(&|message| {
            message.get("type").and_then(|value| value.as_str()) == Some("response")
                && message.get("id").and_then(|value| value.as_str()) == Some(id.as_str())
        })?;
        self.send_message(&message)?;
        Ok(response)
    }

    /// Upstream `sendMessage`.
    pub fn send_message(&mut self, message: &ClientMessage) -> Result<(), WaitError> {
        let value =
            serde_json::to_value(message).map_err(|error| WaitError::Encode(error.to_string()))?;
        let bytes = encode_client_message(&value, FrameDecoderOptions::default())
            .map_err(|error: ProtocolValidationError| WaitError::Encode(error.to_string()))?;
        // The channel is host-owned; callers wire it through
        // send_bytes so the client stays channel-agnostic like the
        // upstream constructor.
        self.pending_send = Some(bytes);
        Ok(())
    }

    /// Bytes queued by the last [`Self::send_message`], for the
    /// driver to push through its channel (upstream the channel is
    /// held directly).
    pub fn take_pending_send(&mut self) -> Option<Vec<u8>> {
        self.pending_send.take()
    }

    /// Upstream `sendBytes`: raw bytes straight to the channel.
    pub fn send_bytes(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.channel.send(chunk)
    }

    /// Upstream `sendFragmentedMessage`.
    pub fn send_fragmented_bytes(&mut self, chunk: &[u8], split_at: usize) -> Result<(), String> {
        self.channel.send_fragmented(chunk, split_at)
    }

    /// Upstream `next`: scan recorded messages for a predicate match.
    pub fn find(&self, predicate: &impl Fn(&Value) -> bool) -> Option<Value> {
        self.messages
            .iter()
            .find(|message| predicate(message))
            .cloned()
    }

    /// Synchronous stand-in for the upstream waiter promise: checks
    /// recorded messages first, then reports pending/closed.
    pub fn wait_for(&self, predicate: &impl Fn(&Value) -> bool) -> Result<Value, WaitError> {
        if let Some(found) = self.find(predicate) {
            return Ok(found);
        }
        if self.channel_closed {
            return Err(WaitError::Closed);
        }
        Err(WaitError::Pending)
    }

    /// Upstream `waitForClose`.
    pub fn wait_for_close(&self) -> Result<(), WaitError> {
        if self.channel_closed {
            Ok(())
        } else {
            Err(WaitError::Pending)
        }
    }
}

impl Default for ProtocolTestClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors surfaced by the synchronous wait helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// No matching message yet; poll again after more input.
    Pending,
    /// The wire closed before a match.
    Closed,
    /// Message encoding failed.
    Encode(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use pillar_protocol::schemas::PROTOCOL_VERSION;

    fn encode_server(value: &Value) -> Vec<u8> {
        pillar_protocol::codec::encode_server_message(value, FrameDecoderOptions::default())
            .unwrap()
    }

    fn hello_frame() -> Vec<u8> {
        encode_server(&serde_json::json!({
            "type": "hello",
            "version": PROTOCOL_VERSION,
            "connectionId": "c1",
            "snapshot": {
                "serverId": "s",
                "protocolVersion": PROTOCOL_VERSION,
                "revision": 0,
                "sessions": [],
                "models": []
            }
        }))
    }

    fn response_frame(id: &str) -> Vec<u8> {
        encode_server(&serde_json::json!({
            "type": "response",
            "id": id,
            "ok": true,
            "result": { "command": "list", "sessions": [] }
        }))
    }

    #[test]
    fn receive_records_messages() {
        let mut client = ProtocolTestClient::new();
        let mut frame = hello_frame();
        frame.extend_from_slice(&response_frame("r1"));
        client.receive(&frame);
        assert_eq!(client.messages.len(), 2);
        assert_eq!(
            client.messages[0].get("type").and_then(|v| v.as_str()),
            Some("hello")
        );
        assert_eq!(
            client.messages[1].get("id").and_then(|v| v.as_str()),
            Some("r1")
        );
        assert!(client.failures.is_empty());
    }

    #[test]
    fn fragmented_frames_reassemble() {
        let mut client = ProtocolTestClient::new();
        let frame = hello_frame();
        let split = 4;
        client.receive(&frame[..split]);
        assert!(client.messages.is_empty());
        client.receive(&frame[split..]);
        assert_eq!(client.messages.len(), 1);
    }

    #[test]
    fn invalid_bytes_fail_decoder() {
        let mut client = ProtocolTestClient::new();
        client.receive(&[0xFF; 4]);
        assert!(!client.failures.is_empty());
    }

    #[test]
    fn mark_closed_is_idempotent_and_records_failure() {
        let mut client = ProtocolTestClient::new();
        client.mark_closed();
        let count = client.failures.len();
        client.mark_closed();
        assert_eq!(client.failures.len(), count);
        assert!(client.closed());
        assert_eq!(client.wait_for_close(), Ok(()));
    }

    #[test]
    fn find_and_wait_for_match_predicates() {
        let mut client = ProtocolTestClient::new();
        client.receive(&response_frame("r9"));
        let hit = client
            .find(&|message| message.get("id").and_then(|v| v.as_str()) == Some("r9"))
            .is_some();
        assert!(hit);
        assert!(client.wait_for(&|_| false) == Err(WaitError::Pending));
    }

    #[test]
    fn wait_for_after_close_reports_closed() {
        let mut client = ProtocolTestClient::new();
        client.mark_closed();
        assert_eq!(client.wait_for(&|_| true), Err(WaitError::Closed));
    }

    #[test]
    fn mock_channel_fragmentation_and_close() {
        let mut channel = MockChannel::default();
        channel.send(b"abc").unwrap();
        channel.send_fragmented(b"defgh", 2).unwrap();
        assert_eq!(channel.sent.len(), 3);
        assert_eq!(channel.sent[1], b"de".to_vec());
        assert_eq!(channel.sent[2], b"fgh".to_vec());
        assert!(channel.send_fragmented(b"x", 5).is_err());
        channel.close().unwrap();
        assert!(channel.send(b"y").is_err());
    }

    #[test]
    fn default_decoder_options_build() {
        assert!(ServerMessageDecoder::new(FrameDecoderOptions::default()).is_ok());
    }
}
