//! Port of packages/protocol/src/schemas.ts (pi v0.84.3).
//!
//! Wire types for the remote-session protocol. Shapes are `serde` structs
//! mirroring the upstream typebox schemas; JSON-valued fields use
//! [`serde_json::Value`], and the cross-field status/stopReason/isError
//! consistency rules plus unknown-field rejection are enforced by the
//! hand-written validators in [`validation`], matching the upstream
//! `Check(...)` semantics including `additionalProperties: false`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u64 = 1;

pub fn is_supported_protocol_version(version: f64) -> bool {
    version == version as u64 as f64 && version == PROTOCOL_VERSION as f64
}

// --- Shared primitives -------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Matches AgentHarnessPhase so adapters do not need a second phase vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Turn,
    Compaction,
    BranchSummary,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub reasoning: bool,
    pub input: Vec<InputKind>,
    pub context_window: u64,
    pub max_tokens: u64,
    pub cost: ModelCost,
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub authenticated: bool,
}

// --- Content blocks ----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

// --- Transcript items --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStatus {
    Streaming,
    Complete,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStopReason {
    Stop,
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Complete,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantTranscriptItem {
    pub id: String,
    pub content: Vec<AssistantContent>,
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub status: AssistantStatus,
    /// Present on complete/error/aborted; absent while streaming (the
    /// per-status wire rules live in `validation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<AssistantStopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTranscriptItem {
    pub id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
    pub content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub status: ToolStatus,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        id: String,
        content: Vec<UserContent>,
        timestamp: u64,
    },
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum UserContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantContent {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        input: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ToolContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

// --- Progress ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    Text,
    Thinking,
    ToolCall,
}

/// Assistant or tool item, as those are the only updatable item kinds.
/// Wire tags (`role`) are flattened by `#[serde(flatten)]` on the wrappers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NonTerminalItem {
    #[serde(flatten)]
    pub item: NonTerminalItemKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum NonTerminalItemKind {
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

/// Terminal assistant states (complete/error/aborted) or terminal tool
/// states (complete/error); `streaming`/`running` are rejected here,
/// enforced by [`validation`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalItem {
    #[serde(flatten)]
    pub item: TerminalItemKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TerminalItemKind {
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TranscriptProgress {
    ItemStarted {
        item: TranscriptItem,
    },
    AssistantDelta {
        message_id: String,
        content_index: u64,
        kind: DeltaKind,
        delta: String,
    },
    ItemUpdated {
        item: NonTerminalItem,
    },
    ItemFinished {
        item: TerminalItem,
    },
}

// --- Session metadata and snapshots ------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub phase: SessionPhase,
    pub model: ModelRef,
    pub thinking_level: ThinkingLevel,
    pub attached: bool,
    pub locked: bool,
    pub revision: u64,
    pub transcript: Vec<TranscriptItem>,
    pub queued_steer: Vec<TranscriptItem>,
    pub queued_steer_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSnapshot {
    pub server_id: String,
    pub protocol_version: u64,
    pub revision: u64,
    pub sessions: Vec<SessionMetadata>,
    pub models: Vec<ModelMetadata>,
}

// --- Errors ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    Version,
    Busy,
    SessionLocked,
    NotFound,
    InvalidRequest,
    NotImplemented,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

// --- Commands ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "command",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    List,
    Create {
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_level: Option<ThinkingLevel>,
    },
    Attach {
        session_id: String,
    },
    Detach {
        session_id: String,
    },
    Prompt {
        session_id: String,
        text: String,
    },
    Steer {
        session_id: String,
        text: String,
    },
    Abort {
        session_id: String,
    },
    SetModel {
        session_id: String,
        model: ModelRef,
    },
    SetThinking {
        session_id: String,
        thinking_level: ThinkingLevel,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "command",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CommandResult {
    List { sessions: Vec<SessionMetadata> },
    Create { session: SessionSnapshot },
    Attach { session: SessionSnapshot },
    Detach { session_id: String },
    Prompt { session: SessionSnapshot },
    Steer { session: SessionSnapshot },
    Abort { session: SessionSnapshot },
    SetModel { session: SessionSnapshot },
    SetThinking { session: SessionSnapshot },
}

// --- Envelopes ---------------------------------------------------------

/// Must be the first frame sent by a client. Version is intentionally an
/// integer, not a coercible string; negotiation accepts any integer version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello { version: u64 },
    Request(RequestEnvelope),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RequestEnvelope {
    pub id: String,
    pub request: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        version: u64,
        connection_id: String,
        snapshot: ServerSnapshot,
    },
    HelloError {
        error: ProtocolError,
    },
    Response {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<CommandResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    Event {
        event: ServerEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ServerEvent {
    ServerSnapshot {
        snapshot: ServerSnapshot,
    },
    SessionSnapshot {
        snapshot: SessionSnapshot,
    },
    SessionProgress {
        session_id: String,
        progress: TranscriptProgress,
    },
    SessionRemoved {
        session_id: String,
    },
}

// --- Cross-field validation --------------------------------------------

pub use validation::{parse_client_message, parse_server_message};

pub mod validation {
    //! Hand-written validators replacing upstream typebox `Check()` calls.
    //! Enforces: no unknown fields (typebox `additionalProperties: false`),
    //! status/stopReason consistency for assistant items, status/isError
    //! consistency for tool items, and the protocol version literal.
    //!
    //! The representation level is [`serde_json::Value`] because validation
    //! must see and reject unknown fields, which typed serde structs drop.

    use serde_json::{Map, Value};

    use super::PROTOCOL_VERSION;

    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    pub struct ProtocolValidationError {
        pub kind: &'static str,
        /// Bounded detail. The upstream error never retains the rejected
        /// payload; a short message keeps that guarantee.
        pub detail: Option<String>,
        message: String,
    }

    impl ProtocolValidationError {
        pub fn new(kind: &'static str) -> Self {
            Self {
                kind,
                detail: None,
                message: format!("Invalid {kind} protocol message"),
            }
        }

        /// Overrides the display message; the wire `kind` stays authoritative.
        pub fn with_message(mut self, message: impl Into<String>) -> Self {
            self.message = message.into();
            self.detail = Some(self.message.clone());
            self
        }
    }

    type VResult<T> = Result<T, ProtocolValidationError>;

    const CLIENT: &str = "client";
    const SERVER: &str = "server";

    fn fail(kind: &'static str) -> ProtocolValidationError {
        ProtocolValidationError::new(kind)
    }

    fn is_json_value(value: &Value) -> bool {
        match value {
            // The byte-string carrier (\u{0} prefix) is not a JSON value.
            Value::String(s) => !s.starts_with('\u{0}'),
            Value::Null | Value::Bool(_) | Value::Number(_) => true,
            Value::Array(items) => items.iter().all(is_json_value),
            Value::Object(map) => map.values().all(is_json_value),
        }
    }

    fn json_field(value: &Value, kind: &'static str) -> VResult<()> {
        if is_json_value(value) {
            Ok(())
        } else {
            Err(fail(kind))
        }
    }

    fn str_ok(s: &str, min_len: usize) -> bool {
        s.chars().count() >= min_len
    }

    fn validate_thinking_level(value: &str) -> bool {
        matches!(
            value,
            "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        )
    }

    fn validate_session_phase(value: &str) -> bool {
        matches!(
            value,
            "idle" | "turn" | "compaction" | "branch_summary" | "retry"
        )
    }

    /// Validates a fully-decoded wire object against the protocol schemas.
    pub fn validate_client_message(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(CLIENT))?;
        let kind = map
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(CLIENT))?;
        match kind {
            "hello" => validate_client_hello(map),
            "request" => validate_request(map),
            _ => Err(fail(CLIENT)),
        }
    }

    pub fn validate_server_message(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        let kind = map
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        match kind {
            "hello" => validate_server_hello(map),
            "hello_error" => validate_hello_error(map),
            "response" => validate_response(map),
            "event" => validate_event(map),
            _ => Err(fail(SERVER)),
        }
    }

    /// Schema-parse entry point mirroring upstream `parseClientMessage`:
    /// validate, then convert into the typed serde representation.
    pub fn parse_client_message(value: &Value) -> VResult<super::ClientMessage> {
        validate_client_message(value)?;
        serde_json::from_value(value.clone()).map_err(|_| fail(CLIENT))
    }

    pub fn parse_server_message(value: &Value) -> VResult<super::ServerMessage> {
        validate_server_message(value)?;
        serde_json::from_value(value.clone()).map_err(|_| fail(SERVER))
    }

    fn validate_client_hello(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(map, &["type", "version"], CLIENT)?;
        let version = map.get("version").ok_or_else(|| fail(CLIENT))?;
        // Accepts any non-negative integer for negotiation; fractional or
        // string versions are rejected (upstream: Type.Integer).
        let n = version.as_f64().ok_or_else(|| fail(CLIENT))?;
        if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n >= 9.007_199_254_740_992e15 {
            return Err(fail(CLIENT));
        }
        Ok(())
    }

    fn validate_server_hello(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(
            map,
            &["type", "version", "connectionId", "snapshot"],
            SERVER,
        )?;
        if map.get("version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
            return Err(fail(SERVER));
        }
        str_field(map, "connectionId", 1)?;
        validate_server_snapshot(field(map, "snapshot")?)
    }

    fn validate_hello_error(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(map, &["type", "error"], SERVER)?;
        validate_protocol_error(field(map, "error")?)
    }

    fn validate_response(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(map, &["type", "id", "ok", "result", "error"], SERVER)?;
        str_field(map, "id", 1)?;
        match map.get("ok").and_then(Value::as_bool) {
            Some(true) => {
                if map.contains_key("error") {
                    return Err(fail(SERVER));
                }
                validate_command_result(field(map, "result")?)
            }
            Some(false) => {
                if map.contains_key("result") {
                    return Err(fail(SERVER));
                }
                validate_protocol_error(field(map, "error")?)
            }
            None => Err(fail(SERVER)),
        }
    }

    fn validate_event(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(map, &["type", "event"], SERVER)?;
        let event = field(map, "event")?;
        let event_map = event.as_object().ok_or_else(|| fail(SERVER))?;
        let kind = event_map
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        match kind {
            "server_snapshot" => {
                expect_keys(event_map, &["type", "snapshot"], SERVER)?;
                validate_server_snapshot(field(event_map, "snapshot")?)
            }
            "session_snapshot" => {
                expect_keys(event_map, &["type", "snapshot"], SERVER)?;
                validate_snapshot(field(event_map, "snapshot")?)
            }
            "session_progress" => {
                expect_keys(event_map, &["type", "sessionId", "progress"], SERVER)?;
                str_field(event_map, "sessionId", 1)?;
                validate_progress(field(event_map, "progress")?)
            }
            "session_removed" => {
                expect_keys(event_map, &["type", "sessionId"], SERVER)?;
                str_field(event_map, "sessionId", 1)
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_server_snapshot(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "serverId",
                "protocolVersion",
                "revision",
                "sessions",
                "models",
            ],
            SERVER,
        )?;
        str_field(map, "serverId", 1)?;
        if map.get("protocolVersion").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
            return Err(fail(SERVER));
        }
        uint_field(map, "revision", 0)?;
        array_field(map, "sessions", validate_session_metadata)?;
        array_field(map, "models", validate_model_metadata)?;
        Ok(())
    }

    fn validate_session_metadata(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "id",
                "createdAt",
                "updatedAt",
                "parentSessionId",
                "sessionName",
                "cwd",
            ],
            SERVER,
        )?;
        str_field(map, "id", 1)?;
        uint_field(map, "createdAt", 0)?;
        opt_uint_field(map, "updatedAt", 0)?;
        opt_str_field(map, "parentSessionId", 1)?;
        opt_str_field(map, "sessionName", 0)?;
        opt_str_field(map, "cwd", 1)?;
        Ok(())
    }

    fn validate_model_metadata(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "provider",
                "id",
                "name",
                "api",
                "reasoning",
                "input",
                "contextWindow",
                "maxTokens",
                "cost",
                "supportedThinkingLevels",
                "authenticated",
            ],
            SERVER,
        )?;
        str_field(map, "provider", 1)?;
        str_field(map, "id", 1)?;
        str_field(map, "name", 1)?;
        str_field(map, "api", 1)?;
        map.get("reasoning")
            .and_then(Value::as_bool)
            .ok_or_else(|| fail(SERVER))?;
        array_field(map, "input", |v| {
            matches!(v.as_str(), Some("text" | "image"))
                .then_some(())
                .ok_or_else(|| fail(SERVER))
        })?;
        uint_field(map, "contextWindow", 1)?;
        uint_field(map, "maxTokens", 1)?;
        validate_model_cost(field(map, "cost")?)?;
        let levels = map
            .get("supportedThinkingLevels")
            .and_then(Value::as_array)
            .ok_or_else(|| fail(SERVER))?;
        if levels.is_empty() {
            return Err(fail(SERVER));
        }
        for level in levels {
            let s = level.as_str().ok_or_else(|| fail(SERVER))?;
            if !validate_thinking_level(s) {
                return Err(fail(SERVER));
            }
        }
        map.get("authenticated")
            .and_then(Value::as_bool)
            .ok_or_else(|| fail(SERVER))?;
        Ok(())
    }

    fn validate_model_cost(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(map, &["input", "output", "cacheRead", "cacheWrite"], SERVER)?;
        for key in ["input", "output", "cacheRead", "cacheWrite"] {
            let n = map
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| fail(SERVER))?;
            if !(n >= 0.0 && n.is_finite()) {
                return Err(fail(SERVER));
            }
        }
        Ok(())
    }

    fn validate_snapshot(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "id",
                "name",
                "cwd",
                "createdAt",
                "updatedAt",
                "phase",
                "model",
                "thinkingLevel",
                "attached",
                "locked",
                "revision",
                "transcript",
                "queuedSteer",
                "queuedSteerCount",
            ],
            SERVER,
        )?;
        str_field(map, "id", 1)?;
        opt_str_field(map, "name", 0)?;
        str_field(map, "cwd", 1)?;
        uint_field(map, "createdAt", 0)?;
        uint_field(map, "updatedAt", 0)?;
        let phase = map
            .get("phase")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        if !validate_session_phase(phase) {
            return Err(fail(SERVER));
        }
        validate_model_ref(field(map, "model")?)?;
        let level = map
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        if !validate_thinking_level(level) {
            return Err(fail(SERVER));
        }
        map.get("attached")
            .and_then(Value::as_bool)
            .ok_or_else(|| fail(SERVER))?;
        map.get("locked")
            .and_then(Value::as_bool)
            .ok_or_else(|| fail(SERVER))?;
        uint_field(map, "revision", 0)?;
        array_field(map, "transcript", validate_transcript_item)?;
        array_field(map, "queuedSteer", validate_user_item)?;
        uint_field(map, "queuedSteerCount", 0)?;
        Ok(())
    }

    fn validate_model_ref(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(map, &["provider", "id"], SERVER)?;
        str_field(map, "provider", 1)?;
        str_field(map, "id", 1)?;
        Ok(())
    }

    fn validate_progress(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        let kind = map
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        match kind {
            "item_started" => {
                expect_keys(map, &["type", "item"], SERVER)?;
                validate_transcript_item(field(map, "item")?)
            }
            "assistant_delta" => {
                expect_keys(
                    map,
                    &["type", "messageId", "contentIndex", "kind", "delta"],
                    SERVER,
                )?;
                str_field(map, "messageId", 1)?;
                uint_field(map, "contentIndex", 0)?;
                if !matches!(
                    map.get("kind").and_then(Value::as_str),
                    Some("text" | "thinking" | "toolCall")
                ) {
                    return Err(fail(SERVER));
                }
                map.get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                Ok(())
            }
            "item_updated" => {
                expect_keys(map, &["type", "item"], SERVER)?;
                validate_nonterminal_item(field(map, "item")?)
            }
            "item_finished" => {
                expect_keys(map, &["type", "item"], SERVER)?;
                validate_terminal_item(field(map, "item")?)
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_transcript_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("role").and_then(Value::as_str) {
            Some("user") => validate_user_item(value),
            Some("assistant") => validate_assistant_item(value),
            Some("tool") => validate_tool_item(value),
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_user_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(map, &["role", "id", "content", "timestamp"], SERVER)?;
        str_field(map, "id", 1)?;
        array_field(map, "content", validate_user_content)?;
        uint_field(map, "timestamp", 0)?;
        Ok(())
    }

    fn validate_user_content(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("type").and_then(Value::as_str) {
            Some("text") => {
                expect_keys(map, &["type", "text"], SERVER)?;
                map.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                Ok(())
            }
            Some("image") => {
                expect_keys(map, &["type", "data", "mimeType"], SERVER)?;
                map.get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                str_field(map, "mimeType", 1)?;
                Ok(())
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_assistant_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "role",
                "id",
                "content",
                "model",
                "responseModel",
                "usage",
                "status",
                "stopReason",
                "errorMessage",
                "timestamp",
            ],
            SERVER,
        )?;
        str_field(map, "id", 1)?;
        array_field(map, "content", validate_assistant_content)?;
        validate_model_ref(field(map, "model")?)?;
        opt_str_field(map, "responseModel", 1)?;
        if let Some(usage) = map.get("usage") {
            validate_usage(usage)?;
        }
        let status = map
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        // Cross-field consistency: status pins which of stopReason/errorMessage
        // must be present, matching the upstream per-status schemas exactly:
        // streaming has no stopReason; complete/error/aborted require theirs.
        let stop_reason = map.get("stopReason").and_then(Value::as_str);
        match (status, stop_reason) {
            ("streaming", None) => {}
            ("complete", Some("stop" | "length" | "toolUse")) => {}
            ("error", Some("error")) => {}
            ("aborted", Some("aborted")) => {}
            _ => return Err(fail(SERVER)),
        }
        match (status, map.get("errorMessage")) {
            ("error", Some(Value::String(msg))) if str_ok(msg, 1) => {}
            ("error", Some(_)) => return Err(fail(SERVER)),
            ("aborted", Some(Value::String(_))) => {}
            ("aborted", Some(_)) => return Err(fail(SERVER)),
            (_, Some(_)) => return Err(fail(SERVER)),
            (_, None) => {}
        }
        uint_field(map, "timestamp", 0)?;
        Ok(())
    }

    fn validate_assistant_content(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("type").and_then(Value::as_str) {
            Some("text") => {
                expect_keys(map, &["type", "text"], SERVER)?;
                map.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                Ok(())
            }
            Some("thinking") => {
                expect_keys(map, &["type", "thinking", "redacted"], SERVER)?;
                map.get("thinking")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                if let Some(r) = map.get("redacted") {
                    r.as_bool().ok_or_else(|| fail(SERVER))?;
                }
                Ok(())
            }
            Some("toolCall") => {
                expect_keys(map, &["type", "toolCallId", "toolName", "input"], SERVER)?;
                str_field(map, "toolCallId", 1)?;
                str_field(map, "toolName", 1)?;
                json_field(field(map, "input")?, SERVER)
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_tool_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            map,
            &[
                "role",
                "id",
                "toolCallId",
                "toolName",
                "input",
                "content",
                "details",
                "status",
                "isError",
                "usage",
                "timestamp",
            ],
            SERVER,
        )?;
        str_field(map, "id", 1)?;
        str_field(map, "toolCallId", 1)?;
        str_field(map, "toolName", 1)?;
        json_field(field(map, "input")?, SERVER)?;
        array_field(map, "content", validate_tool_content)?;
        if let Some(details) = map.get("details") {
            json_field(details, SERVER)?;
        }
        let status = map
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        let is_error = map
            .get("isError")
            .and_then(Value::as_bool)
            .ok_or_else(|| fail(SERVER))?;
        match (status, is_error) {
            ("running" | "complete", false) | ("error", true) => {}
            _ => return Err(fail(SERVER)),
        }
        if let Some(usage) = map.get("usage") {
            validate_usage(usage)?;
        }
        uint_field(map, "timestamp", 0)?;
        Ok(())
    }

    fn validate_tool_content(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("type").and_then(Value::as_str) {
            Some("text") => {
                expect_keys(map, &["type", "text"], SERVER)?;
                map.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                Ok(())
            }
            Some("image") => {
                expect_keys(map, &["type", "data", "mimeType"], SERVER)?;
                map.get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(SERVER))?;
                str_field(map, "mimeType", 1)?;
                Ok(())
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_usage(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        // `reasoning` is emitted only when the provider reported it
        // (upstream `{...(reasoning === undefined ? {} : {reasoning})}`),
        // so it is validated when present rather than required.
        expect_keys(
            map,
            &[
                "input",
                "output",
                "cacheRead",
                "cacheWrite",
                "totalTokens",
                "cost",
            ],
            SERVER,
        )?;
        for key in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
            uint_field(map, key, 0)?;
        }
        if let Some(r) = map.get("reasoning")
            && !r.is_null()
        {
            uint_value(r, 0)?;
        }
        let cost = field(map, "cost")?;
        let cost_map = cost.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(
            cost_map,
            &["input", "output", "cacheRead", "cacheWrite", "total"],
            SERVER,
        )?;
        for key in ["input", "output", "cacheRead", "cacheWrite", "total"] {
            let n = cost_map
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| fail(SERVER))?;
            if !(n >= 0.0 && n.is_finite()) {
                return Err(fail(SERVER));
            }
        }
        Ok(())
    }

    fn validate_nonterminal_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                validate_assistant_item(value)?;
                if map.get("status").and_then(Value::as_str) == Some("streaming") {
                    Ok(())
                } else {
                    Err(fail(SERVER))
                }
            }
            Some("tool") => {
                validate_tool_item(value)?;
                if map.get("status").and_then(Value::as_str) == Some("running") {
                    Ok(())
                } else {
                    Err(fail(SERVER))
                }
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_terminal_item(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        match map.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                validate_assistant_item(value)?;
                if matches!(
                    map.get("status").and_then(Value::as_str),
                    Some("complete" | "error" | "aborted")
                ) {
                    Ok(())
                } else {
                    Err(fail(SERVER))
                }
            }
            Some("tool") => {
                validate_tool_item(value)?;
                if matches!(
                    map.get("status").and_then(Value::as_str),
                    Some("complete" | "error")
                ) {
                    Ok(())
                } else {
                    Err(fail(SERVER))
                }
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_command_result(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        let command = map
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        match command {
            "list" => {
                expect_keys(map, &["command", "sessions"], SERVER)?;
                array_field(map, "sessions", validate_session_metadata)
            }
            "detach" => {
                expect_keys(map, &["command", "sessionId"], SERVER)?;
                str_field(map, "sessionId", 1)
            }
            "create" | "attach" | "prompt" | "steer" | "abort" | "set_model" | "set_thinking" => {
                expect_keys(map, &["command", "session"], SERVER)?;
                validate_snapshot(field(map, "session")?)
            }
            _ => Err(fail(SERVER)),
        }
    }

    fn validate_protocol_error(value: &Value) -> VResult<()> {
        let map = value.as_object().ok_or_else(|| fail(SERVER))?;
        expect_keys(map, &["code", "message", "details"], SERVER)?;
        if !matches!(
            map.get("code").and_then(Value::as_str),
            Some(
                "version"
                    | "busy"
                    | "session_locked"
                    | "not_found"
                    | "invalid_request"
                    | "not_implemented"
                    | "internal_error"
            )
        ) {
            return Err(fail(SERVER));
        }
        map.get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(SERVER))?;
        if let Some(details) = map.get("details") {
            json_field(details, SERVER)?;
        }
        Ok(())
    }

    fn validate_request(map: &Map<String, Value>) -> VResult<()> {
        expect_keys(map, &["type", "id", "request"], CLIENT)?;
        str_field(map, "id", 1)?;
        let request = field(map, "request")?;
        let request_map = request.as_object().ok_or_else(|| fail(CLIENT))?;
        let command = request_map
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(CLIENT))?;
        match command {
            "list" => expect_keys(request_map, &["command"], CLIENT),
            "create" => {
                expect_keys(
                    request_map,
                    &["command", "cwd", "name", "model", "thinkingLevel"],
                    CLIENT,
                )?;
                opt_str_field(request_map, "cwd", 1)?;
                opt_str_field(request_map, "name", 0)?;
                if let Some(model) = request_map.get("model") {
                    validate_model_ref(model)?;
                }
                if let Some(level) = request_map.get("thinkingLevel") {
                    let level = level.as_str().ok_or_else(|| fail(CLIENT))?;
                    if !validate_thinking_level(level) {
                        return Err(fail(CLIENT));
                    }
                }
                Ok(())
            }
            "attach" | "detach" | "abort" => {
                expect_keys(request_map, &["command", "sessionId"], CLIENT)?;
                str_field(request_map, "sessionId", 1)
            }
            "prompt" | "steer" => {
                expect_keys(request_map, &["command", "sessionId", "text"], CLIENT)?;
                str_field(request_map, "sessionId", 1)?;
                request_map
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(CLIENT))?;
                Ok(())
            }
            "set_model" => {
                expect_keys(request_map, &["command", "sessionId", "model"], CLIENT)?;
                str_field(request_map, "sessionId", 1)?;
                validate_model_ref(field(request_map, "model")?)
            }
            "set_thinking" => {
                expect_keys(
                    request_map,
                    &["command", "sessionId", "thinkingLevel"],
                    CLIENT,
                )?;
                str_field(request_map, "sessionId", 1)?;
                let level = request_map
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(CLIENT))?;
                if !validate_thinking_level(level) {
                    return Err(fail(CLIENT));
                }
                Ok(())
            }
            _ => Err(fail(CLIENT)),
        }
    }

    // --- field helpers -------------------------------------------------

    fn field<'a>(map: &'a Map<String, Value>, key: &str) -> VResult<&'a Value> {
        map.get(key).ok_or_else(|| fail(SERVER))
    }

    fn str_field(map: &Map<String, Value>, key: &str, min_len: usize) -> VResult<()> {
        let v = map.get(key).ok_or_else(|| fail(SERVER))?;
        let s = v.as_str().ok_or_else(|| fail(SERVER))?;
        if str_ok(s, min_len) {
            Ok(())
        } else {
            Err(fail(SERVER))
        }
    }

    fn opt_str_field(map: &Map<String, Value>, key: &str, min_len: usize) -> VResult<()> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(()),
            Some(Value::String(s)) if str_ok(s, min_len) => Ok(()),
            Some(_) => Err(fail(SERVER)),
        }
    }

    fn uint_field(map: &Map<String, Value>, key: &str, minimum: u64) -> VResult<()> {
        uint_value(map.get(key).ok_or_else(|| fail(SERVER))?, minimum)
    }

    fn uint_value(value: &Value, minimum: u64) -> VResult<()> {
        let n = value.as_u64().ok_or_else(|| fail(SERVER))?;
        if n >= minimum {
            Ok(())
        } else {
            Err(fail(SERVER))
        }
    }

    fn opt_uint_field(map: &Map<String, Value>, key: &str, minimum: u64) -> VResult<()> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(()),
            Some(v) => uint_value(v, minimum),
        }
    }

    fn array_field(
        map: &Map<String, Value>,
        key: &str,
        item: fn(&Value) -> VResult<()>,
    ) -> VResult<()> {
        let v = map.get(key).ok_or_else(|| fail(SERVER))?;
        let arr = v.as_array().ok_or_else(|| fail(SERVER))?;
        for item_value in arr {
            item(item_value)?;
        }
        Ok(())
    }

    /// `additionalProperties: false`: only the listed keys may be present.
    /// Optional keys may be absent (upstream serializes `undefined` fields
    /// away before encoding).
    fn expect_keys(map: &Map<String, Value>, allowed: &[&str], kind: &'static str) -> VResult<()> {
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(fail(kind));
            }
        }
        Ok(())
    }
}
