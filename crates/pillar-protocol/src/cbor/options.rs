//! Port of packages/protocol/src/cbor/options.ts (pi v0.84.3).
//!
//! Strict-subset CBOR limits and error type shared by the encoder and decoder.

/// Safe defaults for untrusted protocol payloads.
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;

const MAX_CONFIGURED_DEPTH: usize = 512;
pub(crate) const MAX_UINT32: usize = 0xffff_ffff;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CborError(pub(crate) String);

pub type CborResult<T> = Result<T, CborError>;

#[derive(Debug, Clone, Copy, Default)]
pub struct CborOptions {
    /// Maximum encoded input/output bytes and maximum byte/text string length.
    pub max_byte_length: Option<usize>,
    /// Maximum number of elements in an array or entries in a map.
    pub max_container_length: Option<usize>,
    /// Maximum recursive item depth.
    pub max_depth: Option<usize>,
}

impl CborOptions {
    pub const fn new() -> Self {
        Self {
            max_byte_length: None,
            max_container_length: None,
            max_depth: None,
        }
    }

    pub const fn max_byte_length(mut self, value: usize) -> Self {
        self.max_byte_length = Some(value);
        self
    }

    pub const fn max_container_length(mut self, value: usize) -> Self {
        self.max_container_length = Some(value);
        self
    }

    pub const fn max_depth(mut self, value: usize) -> Self {
        self.max_depth = Some(value);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedCborOptions {
    pub max_byte_length: usize,
    pub max_container_length: usize,
    pub max_depth: usize,
}

fn resolve_limit(name: &str, value: Option<usize>, maximum: usize) -> Result<usize, CborError> {
    let value = value.unwrap_or(match name {
        "maxByteLength" => DEFAULT_MAX_CBOR_BYTE_LENGTH,
        "maxContainerLength" => DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
        "maxDepth" => DEFAULT_MAX_CBOR_DEPTH,
        _ => unreachable!(),
    });
    if value > maximum {
        return Err(CborError(format!(
            "{name} must be an integer between 0 and {maximum}"
        )));
    }
    Ok(value)
}

pub(crate) fn resolve_options(options: CborOptions) -> CborResult<ResolvedCborOptions> {
    Ok(ResolvedCborOptions {
        max_byte_length: resolve_limit("maxByteLength", options.max_byte_length, MAX_UINT32)?,
        max_container_length: resolve_limit(
            "maxContainerLength",
            options.max_container_length,
            MAX_UINT32,
        )?,
        max_depth: resolve_limit("maxDepth", options.max_depth, MAX_CONFIGURED_DEPTH)?,
    })
}
