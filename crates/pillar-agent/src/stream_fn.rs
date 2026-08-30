//! Port of packages/agent/src/stream-fn.ts (pi v0.84.3).
//!
//! Global fallback stream function used by `Agent` when a caller omits
//! `streamFn`. Hosts that provide a default model runtime can install their
//! stream function here without making the agent runtime depend on a
//! provider catalog.

use std::sync::RwLock;

use crate::types::StreamFn;

static DEFAULT_STREAM_FN: RwLock<Option<StreamFn>> = RwLock::new(None);

/// Configure the fallback used by [`crate::agent::Agent`] when callers omit
/// streamFn. `None` clears the fallback (upstream
/// `setDefaultStreamFn(undefined)`).
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    *DEFAULT_STREAM_FN.write().expect("default stream fn lock") = stream_fn;
}

/// Returns the configured fallback, if any.
pub fn get_default_stream_fn() -> Option<StreamFn> {
    DEFAULT_STREAM_FN
        .read()
        .expect("default stream fn lock")
        .clone()
}

/// Upstream `getDefaultStreamFn`: panics when no fallback is configured.
#[allow(dead_code)]
pub fn require_default_stream_fn() -> StreamFn {
    get_default_stream_fn().unwrap_or_else(|| {
        panic!("No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn().")
    })
}
