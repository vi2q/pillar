//! Port of packages/protocol/src/framing.ts (pi v0.84.3).
//!
//! Length-prefixed binary framing: a four-byte unsigned big-endian length
//! followed by the payload. [`FrameDecoder`] incrementally splits arbitrary
//! byte chunks into payloads, copying bytes so decoded frames never alias
//! the caller's input.

const FRAME_HEADER_LENGTH: usize = 4;
const PAYLOAD_BLOCK_SIZE: usize = 64 * 1024;

/// Default upper bound for one framed CBOR payload.
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FrameError(pub String);

pub type FrameResult<T> = Result<T, FrameError>;

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDecoderOptions {
    pub max_frame_length: Option<usize>,
}

impl FrameDecoderOptions {
    pub const fn new() -> Self {
        Self {
            max_frame_length: None,
        }
    }

    pub const fn max_frame_length(mut self, value: usize) -> Self {
        self.max_frame_length = Some(value);
        self
    }
}

fn resolve_max_frame_length(options: FrameDecoderOptions) -> Result<usize, FrameError> {
    let value = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    if value > u32::MAX as usize {
        return Err(FrameError(format!(
            "maxFrameLength must be an integer between 0 and {}",
            u32::MAX
        )));
    }
    Ok(value)
}

/// Prefixes a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> FrameResult<Vec<u8>> {
    if payload.len() > u32::MAX as usize {
        return Err(FrameError(
            "Frame payload exceeds the unsigned 32-bit length limit".into(),
        ));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Validates that bytes contain exactly one complete frame within the configured limit.
pub fn assert_complete_frame(frame: &[u8], options: FrameDecoderOptions) -> FrameResult<()> {
    if frame.len() < FRAME_HEADER_LENGTH {
        return Err(FrameError(
            "Frame does not contain a complete length prefix".into(),
        ));
    }
    let length = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let max_frame_length = resolve_max_frame_length(options)?;
    if length > max_frame_length {
        return Err(FrameError(format!(
            "Frame length {length} exceeds configured limit of {max_frame_length}"
        )));
    }
    if frame.len() != FRAME_HEADER_LENGTH + length {
        return Err(FrameError(
            "Frame must contain exactly one complete payload".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
#[derive(Debug)]
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_length: usize,
    max_frame_length: usize,
    payload_blocks: Vec<Vec<u8>>,
    current_payload_block: Option<Vec<u8>>,
    current_payload_block_length: usize,
    expected_payload_length: Option<usize>,
    payload_length: usize,
    state: DecoderState,
}

impl FrameDecoder {
    pub fn new(options: FrameDecoderOptions) -> Result<Self, FrameError> {
        Ok(Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_length: 0,
            max_frame_length: resolve_max_frame_length(options)?,
            payload_blocks: Vec::new(),
            current_payload_block: None,
            current_payload_block_length: 0,
            expected_payload_length: None,
            payload_length: 0,
            state: DecoderState::Open,
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> FrameResult<Vec<Vec<u8>>> {
        match self.state {
            DecoderState::Ended => return Err(FrameError("Frame decoder has ended".into())),
            DecoderState::Failed => return Err(FrameError("Frame decoder has failed".into())),
            DecoderState::Open => {}
        }

        let mut frames = Vec::new();
        let mut chunk_offset = 0;
        while chunk_offset < chunk.len() {
            if self.expected_payload_length.is_none() {
                let header_bytes =
                    (FRAME_HEADER_LENGTH - self.header_length).min(chunk.len() - chunk_offset);
                self.header[self.header_length..self.header_length + header_bytes]
                    .copy_from_slice(&chunk[chunk_offset..chunk_offset + header_bytes]);
                self.header_length += header_bytes;
                chunk_offset += header_bytes;
                if self.header_length < FRAME_HEADER_LENGTH {
                    continue;
                }

                let frame_length = u32::from_be_bytes(self.header) as usize;
                self.header_length = 0;
                if frame_length > self.max_frame_length {
                    self.fail(format!(
                        "Frame length {frame_length} exceeds configured limit of {}",
                        self.max_frame_length
                    ))?;
                }
                if frame_length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                self.expected_payload_length = Some(frame_length);
                self.payload_blocks = Vec::new();
                self.current_payload_block = None;
                self.current_payload_block_length = 0;
                self.payload_length = 0;
            }

            let expected_payload_length = match self.expected_payload_length {
                Some(length) => length,
                None => continue,
            };
            while chunk_offset < chunk.len() && self.payload_length < expected_payload_length {
                let needs_block = match &self.current_payload_block {
                    Some(block) => self.current_payload_block_length == block.len(),
                    None => true,
                };
                if needs_block {
                    // Retire the completed block before starting a new one.
                    if let Some(completed) = self.current_payload_block.take() {
                        self.payload_blocks.push(completed);
                    }
                    let block_size =
                        PAYLOAD_BLOCK_SIZE.min(expected_payload_length - self.payload_length);
                    self.current_payload_block = Some(vec![0; block_size]);
                    self.current_payload_block_length = 0;
                }
                let block = self
                    .current_payload_block
                    .as_mut()
                    .expect("block just created");
                let payload_bytes = (block.len() - self.current_payload_block_length)
                    .min(chunk.len() - chunk_offset);
                block[self.current_payload_block_length
                    ..self.current_payload_block_length + payload_bytes]
                    .copy_from_slice(&chunk[chunk_offset..chunk_offset + payload_bytes]);
                self.current_payload_block_length += payload_bytes;
                self.payload_length += payload_bytes;
                chunk_offset += payload_bytes;
            }
            if self.payload_length == expected_payload_length {
                // Blocks are created with exactly the remaining byte count, so
                // concatenating completed blocks plus the live one yields the
                // payload with no padding to trim.
                let last_block = self
                    .current_payload_block
                    .take()
                    .expect("payload block exists");
                self.payload_blocks.push(last_block);
                let mut payload = Vec::with_capacity(expected_payload_length);
                for payload_block in self.payload_blocks.drain(..) {
                    payload.extend_from_slice(&payload_block);
                }
                debug_assert_eq!(payload.len(), expected_payload_length);
                frames.push(payload);
                self.current_payload_block_length = 0;
                self.expected_payload_length = None;
                self.payload_length = 0;
            }
        }
        Ok(frames)
    }

    pub fn end(&mut self) -> FrameResult<()> {
        match self.state {
            DecoderState::Ended => return Err(FrameError("Frame decoder has ended".into())),
            DecoderState::Failed => return Err(FrameError("Frame decoder has failed".into())),
            DecoderState::Open => {}
        }
        if self.header_length != 0 || self.expected_payload_length.is_some() {
            return self.fail("Truncated frame at end of stream");
        }
        self.state = DecoderState::Ended;
        Ok(())
    }

    fn fail<T>(&mut self, message: impl Into<String>) -> FrameResult<T> {
        self.state = DecoderState::Failed;
        self.header_length = 0;
        self.payload_blocks = Vec::new();
        self.current_payload_block = None;
        self.current_payload_block_length = 0;
        self.expected_payload_length = None;
        self.payload_length = 0;
        Err(FrameError(message.into()))
    }
}
