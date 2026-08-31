//! Port of packages/agent/src/harness/session/jsonl/codec.ts and errors.ts
//! (pi v0.84.3) — JSONL line encode/decode for headers and mutations.

use serde_json::Value;

use super::super::state::SessionMutation;
use super::super::types::{
    Entry, EntryPayload, LaneRecord, RecordPayload, SessionError, SessionErrorCode,
};
use super::types::JsonlV4Header;

const ENTRY_TYPES: [&str; 7] = [
    "message",
    "model_change",
    "thinking_level_change",
    "active_tools_change",
    "compaction",
    "branch_summary",
    "custom",
];

const RECORD_TYPES: [&str; 9] = [
    "operation_started",
    "abort_requested",
    "operation_finished",
    "step_attempt",
    "tool_started",
    "queue_enqueued",
    "queue_cancelled",
    "write_deferred",
    "usage",
];

const OPERATION_KINDS: [&str; 3] = ["run", "compaction", "navigation"];

/// Decode failure for a single JSONL line (upstream `JsonlDecodeError`).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct JsonlDecodeError {
    /// `syntax` (invalid JSON) or `schema` (valid JSON, wrong shape).
    pub kind: JsonlDecodeKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlDecodeKind {
    Syntax,
    Schema,
}

impl JsonlDecodeError {
    pub(crate) fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeKind::Syntax,
            message: message.into(),
        }
    }

    pub(crate) fn schema(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeKind::Schema,
            message: message.into(),
        }
    }
}

/// Wrap a storage `FileError` into a `SessionError` (upstream `fileResult`).
/// `not_found` maps to `SessionError::NotFound`, everything else to
/// `SessionError::Storage`.
pub fn file_error_to_session_error(message: &str) -> SessionError {
    SessionError::new(SessionErrorCode::Storage, message)
}

/// Build the "invalid JSONL line" session error (upstream `invalidFile`).
pub fn invalid_file(path: &str, line: usize, cause: &JsonlDecodeError) -> SessionError {
    SessionError::new(
        SessionErrorCode::InvalidEntry,
        format!("Invalid JSONL v4 session {path}: line {line} {cause}"),
    )
}

fn parse_object(line: &str) -> Result<Value, JsonlDecodeError> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| JsonlDecodeError::syntax(error.to_string()))?;
    if !value.is_object() {
        return Err(JsonlDecodeError::schema("is not a JSON object"));
    }
    Ok(value)
}

fn require_string(value: Option<&Value>, field: &str) -> Result<String, JsonlDecodeError> {
    match value {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(JsonlDecodeError::schema(format!("has invalid {field}"))),
    }
}

fn require_sequence(value: Option<&Value>) -> Result<u64, JsonlDecodeError> {
    match value {
        Some(Value::Number(n)) => {
            let n = n
                .as_u64()
                .ok_or_else(|| JsonlDecodeError::schema("has invalid seq"))?;
            if n == 0 {
                return Err(JsonlDecodeError::schema("has invalid seq"));
            }
            Ok(n)
        }
        _ => Err(JsonlDecodeError::schema("has invalid seq")),
    }
}

fn require_timestamp(value: Option<&Value>) -> Result<u64, JsonlDecodeError> {
    match value {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| JsonlDecodeError::schema("has invalid timestamp")),
        _ => Err(JsonlDecodeError::schema("has invalid timestamp")),
    }
}

fn require_nullable_id(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<String>, JsonlDecodeError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(JsonlDecodeError::schema(format!("has invalid {field}"))),
    }
}

fn decode_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    let value = parse_object(line)?;
    let obj = value.as_object().expect("checked is_object");

    if obj.get("kind").and_then(Value::as_str) != Some("header") {
        return Err(JsonlDecodeError::schema("is not a header"));
    }
    if obj.get("version").and_then(Value::as_u64) != Some(4) {
        return Err(JsonlDecodeError::schema("has unsupported session version"));
    }

    let parent_session_id = match obj.get("parentSessionId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(JsonlDecodeError::schema("has invalid parentSessionId")),
    };
    let legacy_parent_session_path = match obj.get("legacyParentSessionPath") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(JsonlDecodeError::schema(
                "has invalid legacyParentSessionPath",
            ));
        }
    };
    if parent_session_id.is_some() && legacy_parent_session_path.is_some() {
        return Err(JsonlDecodeError::schema(
            "has both parentSessionId and legacyParentSessionPath",
        ));
    }

    let metadata = match obj.get("metadata") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(_) => return Err(JsonlDecodeError::schema("has invalid metadata")),
    };

    Ok(JsonlV4Header {
        kind: "header".to_owned(),
        version: 4,
        id: require_string(obj.get("id"), "id")?,
        created_at: require_timestamp(obj.get("createdAt"))?,
        cwd: require_string(obj.get("cwd"), "cwd")?,
        parent_session_id,
        legacy_parent_session_path,
        metadata,
    })
}

/// Parse a header line (upstream `parseHeader`). Returns the decode error
/// instead of panicking; non-decode errors cannot occur here.
pub fn parse_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    decode_header(line)
}

/// Serialize a header to a JSONL line (upstream `encodeHeader`).
pub fn encode_header(header: &JsonlV4Header) -> String {
    let mut line = serde_json::to_string(header).expect("header is serializable");
    line.push('\n');
    line
}

fn parse_entry_mutation(value: &Value, seq: u64) -> Result<SessionMutation, JsonlDecodeError> {
    let obj = value.as_object().expect("checked is_object");

    let lane = match obj.get("lane") {
        None | Some(Value::Null) => None,
        Some(_) => Some(require_string(obj.get("lane"), "lane")?),
    };
    let id = require_string(obj.get("id"), "id")?;
    let kind = require_string(obj.get("type"), "entry type")?;
    if !ENTRY_TYPES.contains(&kind.as_str()) {
        return Err(JsonlDecodeError::schema(format!(
            "has unknown entry type {kind}"
        )));
    }
    let parent_id = require_nullable_id(obj.get("parentId"), "parentId")?;
    let timestamp = require_timestamp(obj.get("timestamp"))?;
    if kind == "custom" {
        require_string(obj.get("customType"), "customType")?;
    }

    // Rebuild the entry from the raw object minus the envelope fields the
    // storage layer assigns (upstream strips kind/lane and spreads the
    // rest). serde deserializes the payload from the same flattened shape.
    let mut raw = obj.clone();
    raw.remove("kind");
    raw.remove("lane");
    raw.insert("id".to_owned(), Value::String(id));
    raw.insert("type".to_owned(), Value::String(kind.clone()));
    raw.insert(
        "parentId".to_owned(),
        parent_id.clone().map(Value::String).unwrap_or(Value::Null),
    );
    raw.insert("seq".to_owned(), Value::from(seq));
    raw.insert("timestamp".to_owned(), Value::from(timestamp));

    let entry: Entry = serde_json::from_value(Value::Object(raw))
        .map_err(|error| JsonlDecodeError::schema(error.to_string()))?;

    Ok(match lane {
        Some(lane) => SessionMutation::Entry {
            lane: Some(lane),
            entry,
        },
        None => SessionMutation::Entry { lane: None, entry },
    })
}

fn parse_record_mutation(value: &Value, seq: u64) -> Result<SessionMutation, JsonlDecodeError> {
    let obj = value.as_object().expect("checked is_object");

    let id = require_string(obj.get("id"), "id")?;
    let lane = require_string(obj.get("lane"), "lane")?;
    let kind = require_string(obj.get("type"), "record type")?;
    if !RECORD_TYPES.contains(&kind.as_str()) {
        return Err(JsonlDecodeError::schema(format!(
            "has unknown record type {kind}"
        )));
    }
    let timestamp = require_timestamp(obj.get("timestamp"))?;

    if kind == "operation_started" {
        let intent = obj
            .get("intent")
            .ok_or_else(|| JsonlDecodeError::schema("has invalid intent"))?;
        if !intent.is_object() {
            return Err(JsonlDecodeError::schema("has invalid intent"));
        }
        let operation_kind = require_string(intent.get("kind"), "operation kind")?;
        if !OPERATION_KINDS.contains(&operation_kind.as_str()) {
            return Err(JsonlDecodeError::schema(format!(
                "has unknown operation kind {operation_kind}"
            )));
        }
    }
    if kind == "operation_finished" {
        require_string(obj.get("runId"), "runId")?;
    }

    let mut raw = obj.clone();
    raw.remove("kind");
    raw.insert("id".to_owned(), Value::String(id));
    raw.insert("lane".to_owned(), Value::String(lane));
    raw.insert("type".to_owned(), Value::String(kind));
    raw.insert("seq".to_owned(), Value::from(seq));
    raw.insert("timestamp".to_owned(), Value::from(timestamp));

    let record: LaneRecord = serde_json::from_value(Value::Object(raw))
        .map_err(|error| JsonlDecodeError::schema(error.to_string()))?;

    Ok(SessionMutation::Record { record })
}

fn parse_lane_mutation(value: &Value, seq: u64) -> Result<SessionMutation, JsonlDecodeError> {
    let obj = value.as_object().expect("checked is_object");
    Ok(SessionMutation::Lane {
        seq,
        lane: require_string(obj.get("lane"), "lane")?,
        leaf_id: require_nullable_id(obj.get("leafId"), "leafId")?,
    })
}

fn parse_fact_mutation(value: &Value, seq: u64) -> Result<SessionMutation, JsonlDecodeError> {
    let obj = value.as_object().expect("checked is_object");
    match obj.get("fact").and_then(Value::as_str) {
        Some("name") => {
            let name = match obj.get("name") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(JsonlDecodeError::schema("has invalid name")),
            };
            Ok(SessionMutation::Name { seq, name })
        }
        Some("label") => {
            let label = match obj.get("label") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(JsonlDecodeError::schema("has invalid label")),
            };
            Ok(SessionMutation::Label {
                seq,
                target_id: require_string(obj.get("targetId"), "targetId")?,
                label,
            })
        }
        _ => Err(JsonlDecodeError::schema("has unknown fact type")),
    }
}

fn decode_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    let value = parse_object(line)?;
    let obj = value.as_object().expect("checked is_object");
    let seq = require_sequence(obj.get("seq"))?;
    match obj.get("kind").and_then(Value::as_str) {
        Some("entry") => parse_entry_mutation(&value, seq),
        Some("record") => parse_record_mutation(&value, seq),
        Some("lane") => parse_lane_mutation(&value, seq),
        Some("fact") => parse_fact_mutation(&value, seq),
        _ => Err(JsonlDecodeError::schema("has unknown mutation kind")),
    }
}

/// Parse a mutation line (upstream `parseMutation`).
pub fn parse_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    decode_mutation(line)
}

/// Serialize a mutation to a JSONL line (upstream `encodeMutation`).
///
/// Entry lines carry the lane (when set) ahead of the entry fields, matching
/// upstream `{ kind, lane, ...entry }`. Record lines are
/// `{ kind: "record", ...record }`. Lane and fact lines serialize the
/// mutation directly.
pub fn encode_mutation(mutation: &SessionMutation) -> String {
    let line = match mutation {
        SessionMutation::Entry { lane, entry } => {
            let mut value = serde_json::to_value(entry).expect("entry is serializable");
            if let Value::Object(map) = &mut value {
                map.insert("kind".to_owned(), Value::String("entry".to_owned()));
                if let Some(lane) = lane {
                    map.insert("lane".to_owned(), Value::String(lane.clone()));
                }
            }
            value
        }
        SessionMutation::Record { record } => {
            let mut value = serde_json::to_value(record).expect("record is serializable");
            if let Value::Object(map) = &mut value {
                map.insert("kind".to_owned(), Value::String("record".to_owned()));
            }
            value
        }
        SessionMutation::Lane { seq, lane, leaf_id } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".to_owned(), Value::String("lane".to_owned()));
            map.insert("seq".to_owned(), Value::from(*seq));
            map.insert("lane".to_owned(), Value::String(lane.clone()));
            map.insert(
                "leafId".to_owned(),
                leaf_id.clone().map(Value::String).unwrap_or(Value::Null),
            );
            Value::Object(map)
        }
        SessionMutation::Name { seq, name } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".to_owned(), Value::String("fact".to_owned()));
            map.insert("seq".to_owned(), Value::from(*seq));
            map.insert("fact".to_owned(), Value::String("name".to_owned()));
            map.insert(
                "name".to_owned(),
                name.clone().map(Value::String).unwrap_or(Value::Null),
            );
            Value::Object(map)
        }
        SessionMutation::Label {
            seq,
            target_id,
            label,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".to_owned(), Value::String("fact".to_owned()));
            map.insert("seq".to_owned(), Value::from(*seq));
            map.insert("fact".to_owned(), Value::String("label".to_owned()));
            map.insert("targetId".to_owned(), Value::String(target_id.clone()));
            map.insert(
                "label".to_owned(),
                label.clone().map(Value::String).unwrap_or(Value::Null),
            );
            Value::Object(map)
        }
    };
    let mut line = serde_json::to_string(&line).expect("mutation is serializable");
    line.push('\n');
    line
}

/// Serialize a typed entry payload + envelope into the wire shape used by
/// `encode_mutation` (helper for callers that hold payloads directly).
pub fn entry_from_payload(id: String, payload: EntryPayload) -> Entry {
    Entry {
        kind: payload.kind().to_owned(),
        id,
        seq: 0,
        parent_id: None,
        timestamp: 0,
        payload,
    }
}

/// Serialize a typed record payload + envelope into the wire shape used by
/// `encode_mutation` (helper for callers that hold payloads directly).
pub fn record_from_payload(id: String, lane: String, payload: RecordPayload) -> LaneRecord {
    LaneRecord {
        kind: payload.kind().to_owned(),
        id,
        seq: 0,
        lane,
        timestamp: 0,
        payload,
    }
}
