//! The Wasm host's entry point: a thin C ABI over one demo turn.
//!
//! A Wasm host (a browser page, an engine's script sandbox, a `node` script)
//! instantiates the module, calls [`lmpc_demo_turn`], and reads the trace from
//! [`lmpc_trace_ptr`] / the returned length. The turn itself runs on a
//! [`FrameHost`](crate::FrameHost) — no threads, no tokio, virtual time — which
//! is the only host shape this target can provide.
//!
//! The trace text is byte-identical to the native binary's (`lmpc-minimal`),
//! which is what makes the §5-7 comparison a string equality.

use std::sync::Mutex;

/// The trace of the last run, kept alive for the host to copy out.
static TRACE: Mutex<String> = Mutex::new(String::new());

/// The prompt the demo turn uses (fixed so native and Wasm traces match).
pub const WASM_DEMO_PROMPT: &str = "remember something";

/// Run one demo turn and store its trace. Returns the trace length in bytes
/// (0 when the turn failed, in which case [`lmpc_trace_text`] holds the error).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_demo_turn() -> u32 {
    let text = match crate::demo_turn_on(
        &crate::FrameHost::new(),
        WASM_DEMO_PROMPT,
        std::time::Duration::from_millis(1),
    ) {
        Ok(trace) => crate::trace_text(&trace),
        Err(error) => format!("error: {error}"),
    };
    let length = text.len() as u32;
    *TRACE.lock().expect("trace lock") = text;
    length
}

/// The stored trace's address (for `new Uint8Array(memory.buffer, ptr, len)`).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_trace_ptr() -> *const u8 {
    TRACE.lock().expect("trace lock").as_ptr()
}
