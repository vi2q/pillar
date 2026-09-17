//! Port of packages/agent/src/harness/session/jsonl (pi v0.84.3) — the
//! durable JSONL file-backed session backend.
//!
//! Upstream splits this into `types.ts` / `errors.ts` / `codec.ts` /
//! `storage.ts` / `repo.ts`; the port keeps that layout as `types.rs` /
//! `codec.rs` / `storage.rs` / `repo.rs`. `errors.ts` is folded into
//! `codec.rs` (two tiny error helpers) and `index.ts` into `mod.rs`.

pub mod codec;
// The file-backed backend (`storage.rs` + `repo.ts`): the durable session
// store. The format (`codec` / `types`) stays available; a host that keeps
// state elsewhere turns the file backend off (docs/DEVELOPMENT-STRATEGY.md §4).
#[cfg(feature = "session-files")]
pub mod repo;
#[cfg(feature = "session-files")]
pub mod storage;
pub mod types;

pub use codec::{JsonlDecodeError, parse_header, parse_mutation};
#[cfg(feature = "session-files")]
pub use repo::JsonlSessionRepo;
#[cfg(feature = "session-files")]
pub use storage::JsonlSessionStorage;
pub use types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlV4Header,
};
