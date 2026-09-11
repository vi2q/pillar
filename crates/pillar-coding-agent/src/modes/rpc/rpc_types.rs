//! Port of packages/coding-agent/src/modes/rpc/rpc-types.ts (pi v0.84.3):
//! the JSON-lines RPC protocol for headless operation. Commands arrive on
//! stdin, responses and events are emitted on stdout.
//!
//! divergence: upstream types the response `data` payloads with concrete
//! interfaces; the port keeps `serde_json::Value` for response data (a trust
//! boundary, like the extension payloads) while commands, session state, and
//! the extension UI envelopes stay typed. Field names (camelCase) and the
//! command/method tags match the wire exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// RPC command types read from stdin (upstream `RpcCommand`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RpcCommand {
    // Prompting
    Prompt {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<String>,
    },
    Steer {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Value>>,
    },
    FollowUp {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<Value>>,
    },
    Abort,
    ClearQueue,
    NewSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    },

    // State
    GetState,

    // Model
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    GetAvailableModels,

    // Thinking
    SetThinkingLevel {
        level: String,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,

    // Queue modes
    SetSteeringMode {
        mode: String,
    },
    SetFollowUpMode {
        mode: String,
    },

    // Compaction
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },

    // Retry
    SetAutoRetry {
        enabled: bool,
    },
    AbortRetry,

    // Bash
    Bash {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
    },
    AbortBash,

    // Session
    GetSessionStats,
    ExportHtml {
        #[serde(skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    Fork {
        entry_id: String,
    },
    Clone,
    GetForkMessages,
    GetEntries {
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    GetTree,
    GetLastAssistantText,
    SetSessionName {
        name: String,
    },

    // Messages
    GetMessages,

    // Commands
    GetCommands,
}

/// A command envelope: the optional correlation id plus the command body
/// (upstream every `RpcCommand` has `id?: string`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcCommandEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub command: RpcCommand,
}

/// The command type tag (upstream `RpcCommandType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcCommandType {
    Prompt,
    Steer,
    FollowUp,
    Abort,
    ClearQueue,
    NewSession,
    GetState,
    SetModel,
    CycleModel,
    GetAvailableModels,
    SetThinkingLevel,
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    SetSteeringMode,
    SetFollowUpMode,
    Compact,
    SetAutoCompaction,
    SetAutoRetry,
    AbortRetry,
    Bash,
    AbortBash,
    GetSessionStats,
    ExportHtml,
    SwitchSession,
    Fork,
    Clone,
    GetForkMessages,
    GetEntries,
    GetTree,
    GetLastAssistantText,
    SetSessionName,
    GetMessages,
    GetCommands,
}

/// A slash command available for invocation via prompt (upstream
/// `RpcSlashCommand`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSlashCommand {
    /// Command name (without leading slash).
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// "extension" | "prompt" | "skill".
    pub source: String,
    /// Source metadata for the owning resource.
    pub source_info: Value,
}

/// Session state returned by `get_state` (upstream `RpcSessionState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    pub thinking_level: String,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: u64,
    pub pending_message_count: u64,
}

/// An RPC response on stdout (upstream `RpcResponse`).
///
/// `type` is always `"response"`; `command` echoes the command type; a
/// failed response carries `error` and a successful one may carry `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    /// A successful response (upstream `{ type: "response", command, success: true }`).
    pub fn success(id: Option<String>, command: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            id,
            kind: "response".to_string(),
            command: command.into(),
            success: true,
            data,
            error: None,
        }
    }

    /// A failed response (upstream `{ type: "response", command, success: false, error }`).
    pub fn failure(
        id: Option<String>,
        command: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind: "response".to_string(),
            command: command.into(),
            success: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

/// An extension UI request on stdout (upstream `RpcExtensionUIRequest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcExtensionUiRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(flatten)]
    pub request: RpcExtensionUiMethod,
}

impl RpcExtensionUiRequest {
    pub fn new(id: impl Into<String>, request: RpcExtensionUiMethod) -> Self {
        Self {
            kind: "extension_ui_request".to_string(),
            id: id.into(),
            request,
        }
    }
}

/// The method-specific extension UI request payload (tag: `method`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RpcExtensionUiMethod {
    Select {
        title: String,
        options: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Confirm {
        title: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Input {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Editor {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefill: Option<String>,
    },
    Notify {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        notify_type: Option<String>,
    },
    SetStatus {
        status_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status_text: Option<String>,
    },
    SetWidget {
        widget_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_lines: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        widget_placement: Option<String>,
    },
    SetTitle {
        title: String,
    },
    #[serde(rename = "set_editor_text")]
    SetEditorText {
        text: String,
    },
}

/// An extension UI response on stdin (upstream `RpcExtensionUIResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcExtensionUiResponse {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(flatten)]
    pub answer: RpcExtensionUiAnswer,
}

impl RpcExtensionUiResponse {
    pub fn new(id: impl Into<String>, answer: RpcExtensionUiAnswer) -> Self {
        Self {
            kind: "extension_ui_response".to_string(),
            id: id.into(),
            answer,
        }
    }
}

/// The answer payload of an extension UI response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcExtensionUiAnswer {
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled { cancelled: bool },
}
