//! Port of packages/agent/src/harness/session (pi v0.84.3).
//!
//! divergence: upstream `SessionStorage`/`SessionTree` are TS interfaces
//! implemented by jsonl/memory backends; the port inverts this — the
//! in-process `SessionState` is the shared core, `Session` wraps a
//! `SessionStorage` trait object, and backends implement the trait.

pub mod context;
pub mod jsonl;
pub mod memory;
// The upstream `session.ts` facade lives in `memory.rs` (see the module
// comment there); `session.rs` is an empty re-export stub kept for layout
// parity, which clippy's module_inception lint would flag.
pub mod state;
pub mod types;
