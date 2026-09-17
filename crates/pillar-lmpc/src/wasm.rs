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

/// Install the panic hook. A host calls this once after instantiating.
///
/// This target is `panic = "abort"`, so `catch_unwind` cannot help: a panic
/// traps and the module's exports throw. The hook still runs *before* the
/// abort, so it can leave the message in the trace buffer for the host to read
/// after the trap — otherwise a failing turn is an opaque `RuntimeError`.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_init() {
    std::panic::set_hook(Box::new(|info| {
        *TRACE.lock().expect("trace lock") = format!("panic: {info}");
    }));
}

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

/// The stored trace's length (also the panic hook's message length).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_trace_len() -> u32 {
    TRACE.lock().expect("trace lock").len() as u32
}

/// The stored trace's address (for `new Uint8Array(memory.buffer, ptr, len)`).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_trace_ptr() -> *const u8 {
    TRACE.lock().expect("trace lock").as_ptr()
}

// ============================================================================
// The host-model protocol over the C ABI (see `crate::host_model`)
// ============================================================================
//
// The host owns the model, so the turn is a conversation between the two:
//
//   write prompt -> lmpc_input_ptr()/lmpc_input_cap()
//   lmpc_host_turn_start(len)                       -> state
//   loop: state = lmpc_host_poll()
//         NeedsModel: read lmpc_host_request_ptr()/len(), decide,
//                     write the reply JSON into the input buffer,
//                     lmpc_host_reply(len)
//         Done:       read the trace (lmpc_trace_ptr()/len())
//         Failed:     read the reason (same buffer)
//
// States: 0 = running, 1 = needs model, 2 = done, 3 = failed.

const INPUT_CAPACITY: usize = 64 * 1024;

/// The scratch buffer the host writes prompts and replies into.
static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static SESSION: Mutex<Option<crate::host_model::HostModelSession>> = Mutex::new(None);
static REQUEST: Mutex<String> = Mutex::new(String::new());

/// Make sure the buffer has its full length. Growing it once (never again) is
/// what lets the host keep the pointer `lmpc_input_ptr` handed out.
fn ensure_input() {
    let mut guard = INPUT.lock().expect("input lock");
    if guard.len() < INPUT_CAPACITY {
        guard.resize(INPUT_CAPACITY, 0);
    }
}

/// The first `length` bytes of the input buffer as text.
///
/// The host writes straight into the buffer, so the guest reads it by the
/// length the host passes rather than by the buffer's own length.
fn read_input(length: u32) -> Option<String> {
    ensure_input();
    let guard = INPUT.lock().expect("input lock");
    let count = (length as usize).min(INPUT_CAPACITY);
    std::str::from_utf8(&guard[..count]).ok().map(str::to_string)
}

/// The address of the input buffer (`new Uint8Array(memory.buffer, ptr, cap)`).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_input_ptr() -> *mut u8 {
    ensure_input();
    INPUT.lock().expect("input lock").as_mut_ptr()
}

/// The input buffer's capacity in bytes.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_input_cap() -> u32 {
    INPUT_CAPACITY as u32
}

fn state_code(state: crate::host_model::HostModelState) -> i32 {
    match state {
        crate::host_model::HostModelState::Running => 0,
        crate::host_model::HostModelState::NeedsModel => 1,
        crate::host_model::HostModelState::Done => 2,
        crate::host_model::HostModelState::Failed => 3,
        crate::host_model::HostModelState::Cancelled => 4,
    }
}

/// Start a host-model turn; the prompt is `length` bytes of the input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_turn_start(length: u32) -> i32 {
    let Some(prompt) = read_input(length) else {
        return 3;
    };
    let host = crate::FrameHost::new();
    let mut session =
        crate::host_model::HostModelSession::start(&host, &prompt, crate::demo_tools());
    let state = state_code(session.poll(std::time::Duration::from_millis(1)));
    *SESSION.lock().expect("session lock") = Some(session);
    state
}

/// Start another turn on the running session (the guest keeps its state, so an
/// NPC remembers); the prompt is `length` bytes of the input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_say(length: u32) -> i32 {
    let Some(prompt) = read_input(length) else {
        return 3;
    };
    let mut guard = SESSION.lock().expect("session lock");
    let Some(session) = guard.as_mut() else {
        return 3;
    };
    match session.say(&prompt) {
        Ok(()) => state_code(session.poll(std::time::Duration::from_millis(1))),
        Err(_) => 3,
    }
}

/// Advance one frame of the host-model turn.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_poll() -> i32 {
    let mut guard = SESSION.lock().expect("session lock");
    let Some(session) = guard.as_mut() else {
        return 3;
    };
    let state = session.poll(std::time::Duration::from_millis(1));
    match state {
        crate::host_model::HostModelState::Done => {
            let trace = crate::trace_text(&session.trace());
            *TRACE.lock().expect("trace lock") = trace;
        }
        crate::host_model::HostModelState::Failed => {
            *TRACE.lock().expect("trace lock") = format!(
                "error: {}",
                session.error().unwrap_or("the host model turn failed")
            );
        }
        crate::host_model::HostModelState::Cancelled => {
            let trace = crate::trace_text(&session.trace());
            *TRACE.lock().expect("trace lock") = trace;
        }
        _ => {}
    }
    state_code(state)
}

/// The pending model request's length (0 when there is none).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_request_len() -> u32 {
    let mut guard = SESSION.lock().expect("session lock");
    let Some(session) = guard.as_mut() else {
        return 0;
    };
    match session.request_json() {
        Some(request) => {
            let length = request.len() as u32;
            *REQUEST.lock().expect("request lock") = request;
            length
        }
        None => 0,
    }
}

/// The pending model request's address.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_request_ptr() -> *const u8 {
    REQUEST.lock().expect("request lock").as_ptr()
}

/// Answer the pending request with `length` bytes of the input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_reply(length: u32) -> i32 {
    let Some(reply) = read_input(length) else {
        return 3;
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(session) = guard.as_ref() else {
        return 3;
    };
    match session.reply(&reply) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// Cancel the running turn (state 4 afterwards; the trace shows how far it got).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_cancel() {
    if let Some(session) = SESSION.lock().expect("session lock").as_mut() {
        session.cancel();
    }
}
