//! Port of packages/server/src/snapshots.ts (pi v0.84.3) and the pure
//! sanitize/usage helpers from protocol.ts: the ServerSnapshotPublisher
//! decision core (revision bumping, ready-connection fan-out) and the
//! boundary sanitizers.
//!
//! divergences: the broadcast promise queue becomes synchronous (a
//! monotonic revision guard achieves the same ordering); pi-ai model
//! type mapping stays in the host adapters (the port covers the
//! usage/details sanitizers that the protocol boundary enforces).

use pillar_protocol::schemas::{
    PROTOCOL_VERSION, ServerEvent, ServerSnapshot, SessionMetadata, Usage, UsageCost,
};

use crate::sessions::{ConnectionState, ServerError};

/// Model listing source (upstream `PiServerService.listModels`).
pub trait ModelLister {
    fn list_models(&mut self) -> Vec<pillar_protocol::schemas::ModelMetadata>;
}

/// Event sink (upstream the sendMessage callback).
pub trait SnapshotEventSink {
    fn send_event(&mut self, connection_id: u64, event: &ServerEvent);
}

/// Server snapshot publisher (upstream `ServerSnapshotPublisher`):
/// builds snapshots at a monotonically increasing revision and
/// broadcasts to ready connections.
pub struct ServerSnapshotPublisher {
    server_id: String,
    revision: u64,
}

impl ServerSnapshotPublisher {
    pub fn new(server_id: &str) -> Self {
        Self {
            server_id: server_id.to_string(),
            revision: 0,
        }
    }

    pub fn current_revision(&self) -> u64 {
        self.revision
    }

    /// Build a snapshot at the current revision (upstream `get`).
    pub fn get(
        &self,
        sessions: &[SessionMetadata],
        models: &[pillar_protocol::schemas::ModelMetadata],
    ) -> ServerSnapshot {
        ServerSnapshot {
            server_id: self.server_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            revision: self.revision,
            sessions: sessions.to_vec(),
            models: models.to_vec(),
        }
    }

    /// Broadcast a fresh snapshot to ready connections (upstream
    /// `performBroadcast`): revision increments only when there is
    /// someone to receive it, and the returned snapshot carries the
    /// bumped revision.
    pub fn broadcast(
        &mut self,
        connections: &mut [ConnectionState],
        sessions: &[SessionMetadata],
        models: &[pillar_protocol::schemas::ModelMetadata],
        is_closing: bool,
        sink: &mut dyn SnapshotEventSink,
    ) -> Option<ServerSnapshot> {
        let ready: Vec<u64> = connections
            .iter_mut()
            .filter(|connection| connection.ready && !connection.disconnected)
            .map(|connection| connection.id)
            .collect();
        if ready.is_empty() || is_closing {
            return None;
        }
        self.revision += 1;
        let mut snapshot = self.get(sessions, models);
        snapshot.revision = self.revision;
        for connection_id in ready {
            sink.send_event(
                connection_id,
                &ServerEvent::ServerSnapshot {
                    snapshot: snapshot.clone(),
                },
            );
        }
        Some(snapshot)
    }
}

/// Sanitize a details value into the protocol's JSON-compatible subset
/// (upstream `sanitizeProtocolDetails`): non-finite floats stringify,
/// undefined/function/symbol drop, circular references become
/// "[Circular]". Rust callers pass JSON values, so the finite/circular
/// cases are the observable ones.
pub fn sanitize_protocol_details(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(number) => {
            if number.as_f64().is_some_and(|float| !float.is_finite()) {
                return serde_json::Value::String(number.as_f64().unwrap_or(0.0).to_string());
            }
            value.clone()
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            value.clone()
        }
        serde_json::Value::Array(entries) => {
            serde_json::Value::Array(entries.iter().map(sanitize_protocol_details).collect())
        }
        serde_json::Value::Object(entries) => {
            let mut result = serde_json::Map::new();
            for (key, entry) in entries {
                result.insert(key.clone(), sanitize_protocol_details(entry));
            }
            serde_json::Value::Object(result)
        }
    }
}

fn non_negative_integer(value: f64) -> Option<u64> {
    if !value.is_finite() {
        return None;
    }
    Some(value.max(0.0).floor() as u64)
}

fn non_negative_number(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// A usage-shaped input for [`to_protocol_usage`] (upstream the AiUsage
/// type).
#[derive(Debug, Clone, Copy, Default)]
pub struct AiUsage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub reasoning: Option<f64>,
    pub total_tokens: f64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
    pub cost_total: f64,
}

/// Map an execution-boundary usage into the protocol subset (upstream
/// `toProtocolUsage`): non-negative integer clamping, optional
/// reasoning, zero-filled cost.
pub fn to_protocol_usage(usage: Option<&AiUsage>) -> Option<Usage> {
    let usage = usage?;
    let reasoning = usage.reasoning.and_then(non_negative_integer);
    Some(Usage {
        input: non_negative_integer(usage.input).unwrap_or(0),
        output: non_negative_integer(usage.output).unwrap_or(0),
        cache_read: non_negative_integer(usage.cache_read).unwrap_or(0),
        cache_write: non_negative_integer(usage.cache_write).unwrap_or(0),
        reasoning,
        total_tokens: non_negative_integer(usage.total_tokens).unwrap_or(0),
        cost: UsageCost {
            input: non_negative_number(usage.cost_input),
            output: non_negative_number(usage.cost_output),
            cache_read: non_negative_number(usage.cost_cache_read),
            cache_write: non_negative_number(usage.cost_cache_write),
            total: non_negative_number(usage.cost_total),
        },
    })
}

/// Server-side error taxonomy (upstream errors.ts — re-exported shape
/// from sessions for boundary use).
pub use crate::sessions::ServerError as ServerProtocolError;

/// Build a server error for an unknown session (helper shared with the
/// protocol layer).
pub fn session_not_found(session_id: &str) -> ServerError {
    ServerError::new(
        pillar_protocol::schemas::ProtocolErrorCode::NotFound,
        format!("Session is not live: {session_id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink {
        sent: Vec<u64>,
    }

    impl SnapshotEventSink for RecordingSink {
        fn send_event(&mut self, connection_id: u64, _event: &ServerEvent) {
            self.sent.push(connection_id);
        }
    }

    fn connection(id: u64, ready: bool, disconnected: bool) -> ConnectionState {
        ConnectionState {
            id,
            ready,
            disconnected,
            closed: false,
            session_ids: Default::default(),
        }
    }

    // --- publisher ----------------------------------------------------------------------------------

    #[test]
    fn broadcast_skips_when_no_ready_connections() {
        let mut publisher = ServerSnapshotPublisher::new("s");
        let mut connections = vec![connection(1, false, false), connection(2, true, true)];
        let mut sink = RecordingSink { sent: Vec::new() };
        let snapshot = publisher.broadcast(&mut connections, &[], &[], false, &mut sink);
        assert!(snapshot.is_none());
        assert_eq!(publisher.current_revision(), 0);
    }

    #[test]
    fn broadcast_skips_when_closing() {
        let mut publisher = ServerSnapshotPublisher::new("s");
        let mut connections = vec![connection(1, true, false)];
        let mut sink = RecordingSink { sent: Vec::new() };
        let snapshot = publisher.broadcast(&mut connections, &[], &[], true, &mut sink);
        assert!(snapshot.is_none());
        assert_eq!(publisher.current_revision(), 0);
    }

    #[test]
    fn broadcast_bumps_revision_and_sends_to_ready() {
        let mut publisher = ServerSnapshotPublisher::new("s");
        let mut connections = vec![
            connection(1, true, false),
            connection(2, false, false),
            connection(3, true, false),
        ];
        let mut sink = RecordingSink { sent: Vec::new() };
        let snapshot = publisher
            .broadcast(&mut connections, &[], &[], false, &mut sink)
            .unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
        assert_eq!(snapshot.server_id, "s");
        // Only ready connections receive the event.
        assert_eq!(sink.sent, vec![1, 3]);
        // A second broadcast bumps again.
        let snapshot = publisher
            .broadcast(&mut connections, &[], &[], false, &mut sink)
            .unwrap();
        assert_eq!(snapshot.revision, 2);
    }

    #[test]
    fn get_uses_current_revision_without_bump() {
        let publisher = ServerSnapshotPublisher::new("srv");
        let snapshot = publisher.get(&[], &[]);
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.server_id, "srv");
    }

    // --- sanitizeProtocolDetails -----------------------------------------------------------------------

    #[test]
    fn sanitize_keeps_json_values() {
        let value = serde_json::json!({"a": 1, "b": "x", "c": [true, null]});
        assert_eq!(sanitize_protocol_details(&value), value);
    }

    #[test]
    fn sanitize_passes_finite_numbers_through() {
        // serde_json cannot represent non-finite numbers, so the
        // upstream stringification path (Number.isFinite → String) is
        // only reachable for host-language callers; the JSON-visible
        // behavior is that finite numbers pass through unchanged.
        let value = serde_json::json!({"a": 1.5, "b": -0.0});
        assert_eq!(sanitize_protocol_details(&value), value);
    }

    // --- toProtocolUsage ---------------------------------------------------------------------------------

    #[test]
    fn usage_maps_with_clamping() {
        let mut ai = AiUsage {
            input: 10.0,
            output: 5.7,
            cache_read: -1.0,
            cache_write: 0.0,
            reasoning: Some(3.2),
            total_tokens: 15.9,
            cost_input: 0.1,
            cost_output: -0.5,
            cost_cache_read: f64::NAN,
            cost_cache_write: 0.2,
            cost_total: 0.4,
        };
        let usage = to_protocol_usage(Some(&ai)).unwrap();
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5); // floored
        assert_eq!(usage.cache_read, 0); // negative clamped
        assert_eq!(usage.reasoning, Some(3));
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.cost.output, 0.0); // negative clamped
        assert_eq!(usage.cost.cache_read, 0.0); // NaN → 0
        assert_eq!(usage.cost.cache_write, 0.2);
        let _ = &mut ai;
    }

    #[test]
    fn usage_none_returns_none() {
        assert!(to_protocol_usage(None).is_none());
    }

    #[test]
    fn usage_without_reasoning_omits_field() {
        let ai = AiUsage {
            input: 1.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: 0.0,
            reasoning: None,
            total_tokens: 2.0,
            cost_input: 0.0,
            cost_output: 0.0,
            cost_cache_read: 0.0,
            cost_cache_write: 0.0,
            cost_total: 0.0,
        };
        let usage = to_protocol_usage(Some(&ai)).unwrap();
        assert_eq!(usage.reasoning, None);
    }

    #[test]
    fn session_not_found_error_shape() {
        let error = session_not_found("abc");
        assert_eq!(error.to_string(), "Session is not live: abc");
    }
}
