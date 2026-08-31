//! Port of packages/agent/src/harness/session/jsonl (pi v0.84.3) — the
//! durable JSONL file-backed session backend.
//!
//! Upstream splits this into `types.ts` / `errors.ts` / `codec.ts` /
//! `storage.ts` / `repo.ts`; the port keeps that layout as `types.rs` /
//! `codec.rs` / `storage.rs` / `repo.rs`. `errors.ts` is folded into
//! `codec.rs` (two tiny error helpers) and `index.ts` into `mod.rs`.

pub mod codec;
pub mod repo;
pub mod storage;
pub mod types;

pub use codec::{JsonlDecodeError, parse_header, parse_mutation};
pub use repo::JsonlSessionRepo;
pub use storage::JsonlSessionStorage;
pub use types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlV4Header,
};
