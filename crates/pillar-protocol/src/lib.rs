//! Transport-neutral CBOR protocol for remote pillar sessions.
//!
//! Port of pi `packages/protocol` (pi v0.84.3, commit `56700d4`):
//! a strict definite-length RFC 8949 CBOR subset, 4-byte big-endian
//! length-prefixed framing, and the remote-session message schemas.
//!
//! divergence: message validation is hand-written instead of typebox, so
//! unknown-field rejection happens in [`schemas::validation`]; typed serde
//! structs are provided for consumers that trust the validated values.

pub mod cbor;
pub mod codec;
pub mod framing;
pub mod schemas;

pub use cbor::{
    decode_cbor, encode_cbor, CborError, CborOptions, CborResult, DEFAULT_MAX_CBOR_BYTE_LENGTH,
    DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH,
};
pub use codec::{
    encode_client_message, encode_server_message, ClientMessageDecoder, ServerMessageDecoder,
};
pub use framing::{
    assert_complete_frame, encode_frame, FrameDecoder, FrameDecoderOptions, FrameError,
    FrameResult, DEFAULT_MAX_FRAME_LENGTH,
};
pub use schemas::validation::{
    parse_client_message, parse_server_message, validate_client_message, validate_server_message,
    ProtocolValidationError,
};
pub use schemas::{
    is_supported_protocol_version, AssistantContent, AssistantStatus, AssistantStopReason,
    AssistantTranscriptItem, ClientMessage, Command, CommandResult, DeltaKind, InputKind,
    ModelCost, ModelMetadata, ModelRef, NonTerminalItem, NonTerminalItemKind, ProtocolError,
    ProtocolErrorCode, RequestEnvelope, ServerEvent, ServerMessage, ServerSnapshot,
    SessionMetadata, SessionPhase, SessionSnapshot, TerminalItem, TerminalItemKind, ThinkingLevel,
    ToolContent, ToolStatus, ToolTranscriptItem, TranscriptItem, TranscriptProgress, Usage,
    UsageCost, UserContent, PROTOCOL_VERSION,
};
