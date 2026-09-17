//! Where extension *sources* come from, host-side.
//!
//! The VM crate takes a `SourceReader` and never touches a disk itself
//! (docs/DEVELOPMENT-STRATEGY.md §4): the host decides whether a path resolves
//! to the filesystem, a project tree it already walked, an embedded bundle, or
//! a Wasm VFS. This module is the native implementation.
//!
//! Why not inside the Luau wiring (`runner/luau.rs`): that file is the one that
//! must go through the effect broker for every capability an *extension* asks
//! for, and `tests/extension_safety_parity.rs` asserts it contains no direct
//! `std::fs` / `std::process` call. Reading the extension file itself is the
//! host's own business and happens before any extension code runs (the trust
//! decision gates it), so it lives here instead.

use std::sync::Arc;

use pillar_extensions::loader::SourceReader;

/// Read extension sources from the filesystem.
pub fn filesystem_source_reader() -> SourceReader {
    Arc::new(|path: &str| std::fs::read_to_string(path).map_err(|error| error.to_string()))
}
