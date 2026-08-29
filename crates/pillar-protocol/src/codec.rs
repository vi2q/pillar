//! Port of packages/protocol/src/codec.ts (pi v0.84.3).
//!
//! Validates and encodes/decodes framed protocol messages. The wire-validation
//! layer is hand-written (see [`crate::schemas::validation`]) because upstream
//! typebox schemas reject unknown fields, which typed serde structs accept.
//! Messages cross this module as [`serde_json::Value`]; typed conversion is
//! available through [`crate::schemas::validation::parse_client_message`] and
//! `parse_server_message`.

use serde_json::Value;

use crate::cbor::{CborOptions, decode_cbor, encode_cbor};
use crate::framing::{
    DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError, assert_complete_frame,
    encode_frame,
};
use crate::schemas::validation::{
    ProtocolValidationError, validate_client_message, validate_server_message,
};

fn bounded(message: String) -> String {
    if message.len() <= 500 {
        message
    } else {
        let mut cut = 497;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &message[..cut])
    }
}

/// Validates and encodes one complete length-prefixed client message.
pub fn encode_client_message(
    message: &Value,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    encode_protocol_message(message, "client", options)
}

/// Validates and encodes one complete length-prefixed server message.
pub fn encode_server_message(
    message: &Value,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    encode_protocol_message(message, "server", options)
}

fn encode_protocol_message(
    value: &Value,
    kind: &'static str,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    if kind == "client" {
        validate_client_message(value)?;
    } else {
        validate_server_message(value)?;
    }
    let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    let payload = encode_cbor(value, CborOptions::new().max_byte_length(max_frame_length))
        .map_err(|error| {
            ProtocolValidationError::new(kind).with_message(format!(
                "Unable to encode {kind} protocol message: {}",
                bounded(error.0)
            ))
        })?;
    let frame = encode_frame(&payload).map_err(|error| {
        ProtocolValidationError::new(kind).with_message(format!(
            "Unable to encode {kind} protocol message: {}",
            bounded(error.0)
        ))
    })?;
    assert_complete_frame(&frame, options).map_err(|error| {
        ProtocolValidationError::new(kind).with_message(format!(
            "Unable to encode {kind} protocol message: {}",
            bounded(error.0)
        ))
    })?;
    Ok(frame)
}

/// Incrementally decodes and validates framed client messages.
pub struct ClientMessageDecoder {
    frames: FrameDecoder,
    failed: bool,
    max_frame_length: usize,
}

impl ClientMessageDecoder {
    pub fn new(options: FrameDecoderOptions) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            failed: false,
            max_frame_length: options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new("client")
                .with_message("client message decoder has failed"));
        }
        match self.push_inner(chunk) {
            Ok(messages) => Ok(messages),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolValidationError> {
        let frames = self
            .frames
            .push(chunk)
            .map_err(|error| invalid_frame("client", error.0))?;
        let mut messages = Vec::with_capacity(frames.len());
        for frame in frames {
            let value = decode_cbor(
                &frame,
                CborOptions::new().max_byte_length(self.max_frame_length),
            )
            .map_err(|error| invalid_frame("client", error.0))?;
            validate_client_message(&value)?;
            messages.push(value);
        }
        Ok(messages)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new("client")
                .with_message("client message decoder has failed"));
        }
        self.frames.end().map_err(|error| {
            self.failed = true;
            ProtocolValidationError::new("client").with_message(format!(
                "Invalid client protocol framing: {}",
                bounded(error.0)
            ))
        })
    }
}

/// Incrementally decodes and validates framed server messages.
pub struct ServerMessageDecoder {
    frames: FrameDecoder,
    failed: bool,
    max_frame_length: usize,
}

impl ServerMessageDecoder {
    pub fn new(options: FrameDecoderOptions) -> Result<Self, FrameError> {
        Ok(Self {
            frames: FrameDecoder::new(options)?,
            failed: false,
            max_frame_length: options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new("server")
                .with_message("server message decoder has failed"));
        }
        match self.push_inner(chunk) {
            Ok(messages) => Ok(messages),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolValidationError> {
        let frames = self
            .frames
            .push(chunk)
            .map_err(|error| invalid_frame("server", error.0))?;
        let mut messages = Vec::with_capacity(frames.len());
        for frame in frames {
            let value = decode_cbor(
                &frame,
                CborOptions::new().max_byte_length(self.max_frame_length),
            )
            .map_err(|error| invalid_frame("server", error.0))?;
            validate_server_message(&value)?;
            messages.push(value);
        }
        Ok(messages)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new("server")
                .with_message("server message decoder has failed"));
        }
        self.frames.end().map_err(|error| {
            self.failed = true;
            ProtocolValidationError::new("server").with_message(format!(
                "Invalid server protocol framing: {}",
                bounded(error.0)
            ))
        })
    }
}

fn invalid_frame(kind: &'static str, message: String) -> ProtocolValidationError {
    ProtocolValidationError::new(kind).with_message(format!(
        "Invalid {kind} protocol frame: {}",
        bounded(message)
    ))
}
