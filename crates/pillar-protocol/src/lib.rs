//! Transport-neutral CBOR protocol for remote pillar sessions.
//!
//! Port of pi `packages/protocol` (pi v0.84.3, commit `56700d4`):
//! a strict definite-length RFC 8949 CBOR subset, 4-byte big-endian
//! length-prefixed framing, and the remote-session message schemas.
//!
//! divergence: message validation is hand-written instead of typebox, so
//! unknown-field rejection happens in [`schemas::validation`]; typed serde
//! structs are provided for consumers that trust the validated values.

#![forbid(unsafe_code)]
pub mod cbor;
pub mod codec;
pub mod framing;
pub mod schemas;

pub use cbor::{
    CborError, CborOptions, CborResult, DEFAULT_MAX_CBOR_BYTE_LENGTH,
    DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH, decode_cbor, encode_cbor,
};
pub use codec::{
    ClientMessageDecoder, ServerMessageDecoder, encode_client_message, encode_server_message,
};
pub use framing::{
    DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError, FrameResult,
    assert_complete_frame, encode_frame,
};
pub use schemas::validation::{
    ProtocolValidationError, parse_client_message, parse_server_message, validate_client_message,
    validate_server_message,
};
pub use schemas::{
    AssistantContent, AssistantStatus, AssistantStopReason, AssistantTranscriptItem, ClientMessage,
    Command, CommandResult, DeltaKind, InputKind, ModelCost, ModelMetadata, ModelRef,
    NonTerminalItem, NonTerminalItemKind, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    RequestEnvelope, ServerEvent, ServerMessage, ServerSnapshot, SessionMetadata, SessionPhase,
    SessionSnapshot, TerminalItem, TerminalItemKind, ThinkingLevel, ToolContent, ToolStatus,
    ToolTranscriptItem, TranscriptItem, TranscriptProgress, Usage, UsageCost, UserContent,
    is_supported_protocol_version,
};
