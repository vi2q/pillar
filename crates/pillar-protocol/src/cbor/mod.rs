//! Port of packages/protocol/src/cbor/index.ts (pi v0.84.3).

mod decoder;
mod encoder;
mod options;

pub use decoder::decode_cbor;
pub use encoder::encode_cbor;
pub use options::{
    CborError, CborOptions, CborResult, DEFAULT_MAX_CBOR_BYTE_LENGTH,
    DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH,
};
