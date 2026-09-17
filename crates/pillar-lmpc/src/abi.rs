//! The host's entry point: a thin C ABI over the embedding protocol.
//!
//! A host (a browser page, an engine's script sandbox, a `node` script — and the
//! tests here, which call these functions directly on the native target) calls
//! [`lmpc_session_create`], optionally [`lmpc_session_import`], then
//! [`lmpc_host_turn_start`] and drives the turn with [`lmpc_host_poll`]. The turn
//! runs on a [`FrameHost`](crate::FrameHost) — no threads, no tokio, virtual
//! time — which is the only host shape the embedding target can provide.
//!
//! The trace text is byte-identical to the native binary's (`lmpc-minimal`),
//! which is what makes the §5-7 comparison a string equality.
//!
//! # Identity at the boundary
//!
//! Every request the host has to answer (a model request, a tool call) is
//! published with a **ticket**, and an answer must name the ticket it answers:
//! `lmpc_host_reply(ticket, len)`, `lmpc_host_stream(ticket, len)`,
//! `lmpc_host_tool_result(ticket, len)`. A ticket that is not the pending one —
//! an answer that arrives after a cancel, after the next turn started, or for a
//! *different* parallel tool call — is rejected (the call returns non-zero)
//! instead of being applied to whatever happens to be pending. Tickets pack the
//! session epoch in the high 32 bits, so replacing the session (a resume) also
//! invalidates the old session's tickets.
//!
//! This is the fix for policy review sb39f R2/R4.

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
// Storing and resuming a conversation is symmetric: `lmpc_session_export`
// refuses (returns -1) a state past `lmpc_session_state_cap`, and import takes
// anything up to that cap — through the input buffer when it fits, and through
// `lmpc_session_import_begin` / `…_write` / `…_commit` in chunks when it does
// not. A `len` past the buffer it is read into is refused rather than clamped
// (negative return, see `refusal`).
//
// States: 0 = running, 1 = needs model, 2 = done, 3 = failed.

const INPUT_CAPACITY: usize = 64 * 1024;

/// The largest stored conversation the ABI will hand over and take back.
///
/// It is a *policy* value the host can read ([`lmpc_session_state_cap`]): the
/// point is not the number but that export and import agree on it, so anything
/// [`lmpc_session_export`] hands out can be put back with
/// [`lmpc_session_import_begin`] / `…_write` / `…_commit`. A conversation past
/// this size is refused *explicitly* (export answers `-1`) instead of being
/// truncated on the way in.
const STATE_CAPACITY: usize = 4 * 1024 * 1024;

/// The ABI's refusals for host-supplied bytes. State codes are non-negative, so
/// a negative return is always "the argument was refused, and why".
mod refusal {
    /// More bytes than the destination holds.
    pub const TOO_LONG: i32 = -1;
    /// The bytes are not valid UTF-8.
    pub const NOT_UTF8: i32 = -2;
    /// `offset`/`length` fall outside the announced transfer.
    pub const RANGE: i32 = -3;
    /// There is no import in progress.
    pub const NO_IMPORT: i32 = -4;
}
use refusal::{NOT_UTF8, NO_IMPORT, RANGE, TOO_LONG};

/// The state a chunked import is assembling. `covered` is how many leading
/// bytes the host has handed over: writes must be sequential, so a gap or an
/// overlap is refused (and a commit of an unfinished transfer is refused too).
struct PendingImport {
    buffer: Vec<u8>,
    covered: usize,
}

static PENDING_IMPORT: Mutex<Option<PendingImport>> = Mutex::new(None);

/// The scratch buffer the host writes prompts and replies into.
static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static SESSION: Mutex<Option<AbiSession>> = Mutex::new(None);
/// Bumped whenever the session is replaced, so the tickets of the old session
/// cannot name anything in the new one.
static NEXT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A session as the ABI holds it, with the epoch its tickets carry.
struct AbiSession {
    session: crate::host_model::HostModelSession,
    epoch: u64,
}

impl AbiSession {
    /// The ticket the host sees (the epoch goes in the high 32 bits).
    fn ticket(&self, local: crate::host_model::Ticket) -> u64 {
        (self.epoch << 32) | (local & 0xffff_ffff)
    }

    /// The local ticket an answer names, when it belongs to this session
    /// (a stale session's ticket has another epoch).
    fn local_ticket(&self, public: u64) -> Option<crate::host_model::Ticket> {
        if public >> 32 != self.epoch {
            return None;
        }
        Some(public & 0xffff_ffff)
    }
}

/// Replace the session with a fresh one. `host_tools` != 0 also hands the
/// guest's tool calls to the host (engine actions).
///
/// The lifecycle is explicit so a host can restore a stored conversation *before*
/// the first request is published: create → import → start (policy review sb39f
/// R3, where the import happened after the first poll and the model still saw an
/// empty history).
fn install_session(host_tools: bool) -> u64 {
    let host = crate::FrameHost::new();
    let session = if host_tools {
        crate::host_model::HostModelSession::prepare(&host, Vec::new(), true)
    } else {
        crate::host_model::HostModelSession::prepare(&host, crate::demo_tools(), false)
    };
    let epoch = NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    *SESSION.lock().expect("session lock") = Some(AbiSession { session, epoch });
    epoch
}
static REQUEST: Mutex<String> = Mutex::new(String::new());

/// Make sure the buffer has its full length. Growing it once (never again) is
/// what lets the host keep the pointer `lmpc_input_ptr` handed out.
fn ensure_input() {
    let mut guard = INPUT.lock().expect("input lock");
    if guard.len() < INPUT_CAPACITY {
        guard.resize(INPUT_CAPACITY, 0);
    }
}

/// The first `length` bytes of the input buffer, uninterpreted (the chunked
/// import moves bytes; the state is validated where it is restored).
fn read_input_bytes(length: u32) -> Result<Vec<u8>, i32> {
    ensure_input();
    if length as usize > INPUT_CAPACITY {
        return Err(TOO_LONG);
    }
    let guard = INPUT.lock().expect("input lock");
    Ok(guard[..length as usize].to_vec())
}

/// The first `length` bytes of the input buffer as text.
///
/// The host writes straight into the buffer, so the guest reads it by the
/// length the host passes rather than by the buffer's own length. A `length`
/// past the buffer, or bytes that are not UTF-8, is refused with the reason
/// ([`TOO_LONG`] / [`NOT_UTF8`]) rather than silently truncated: a host that
/// sends more than the buffer holds has a bug the boundary should report.
fn read_input(length: u32) -> Result<String, i32> {
    ensure_input();
    if length as usize > INPUT_CAPACITY {
        return Err(TOO_LONG);
    }
    let guard = INPUT.lock().expect("input lock");
    std::str::from_utf8(&guard[..length as usize])
        .map(str::to_string)
        .map_err(|_| NOT_UTF8)
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
        crate::host_model::HostModelState::NeedsTool => 5,
    }
}

/// Create a fresh session (the host then imports a stored conversation, if it
/// has one, and starts a turn). Returns 0.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_create(host_tools: u32) -> i32 {
    install_session(host_tools != 0);
    0
}

/// Start a host-model turn on the current session (creating a default one when
/// the host did not call [`lmpc_session_create`] first); the prompt is `length`
/// bytes of the input buffer.
///
/// This does **not** poll: the first request is published by the first
/// [`lmpc_host_poll`], so a host may still import a stored conversation in
/// between. Returns 0, or 3 when the input is not valid UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_turn_start(length: u32) -> i32 {
    let prompt = match read_input(length) {
        Ok(prompt) => prompt,
        Err(refusal) => return refusal,
    };
    if SESSION.lock().expect("session lock").is_none() {
        install_session(false);
    }
    let mut guard = SESSION.lock().expect("session lock");
    let Some(session) = guard.as_mut() else {
        return 3;
    };
    match session.session.begin_turn(&prompt) {
        // A session that is idle starts a turn; a running one is reported to the
        // host instead of silently queueing a second prompt.
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// Stream a partial answer for the request `ticket` names (the guest forwards it
/// as `message_update` events); the text is `length` bytes of the input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_stream(ticket: u64, length: u32) -> i32 {
    let delta = match read_input(length) {
        Ok(delta) => delta,
        Err(refusal) => return refusal,
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 3;
    };
    let Some(local) = abi.local_ticket(ticket) else {
        return 3;
    };
    match abi.session.stream_delta_to(local, &delta) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// Start another turn on the running session (the guest keeps its state, so an
/// NPC remembers); the prompt is `length` bytes of the input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_say(length: u32) -> i32 {
    let prompt = match read_input(length) {
        Ok(prompt) => prompt,
        Err(refusal) => return refusal,
    };
    let mut guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_mut() else {
        return 3;
    };
    match abi.session.begin_turn(&prompt) {
        // As with `turn_start`, the first request comes from the next poll.
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// Advance one frame of the host-model turn.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_poll() -> i32 {
    let mut guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_mut() else {
        return 3;
    };
    let session = &mut abi.session;
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

/// The pending tool call's length (0 when there is none).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_tool_request_len() -> u32 {
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    match abi.session.tool_request_json() {
        Some(call) => {
            let length = call.len() as u32;
            *REQUEST.lock().expect("request lock") = call;
            length
        }
        None => 0,
    }
}

/// The pending tool call's address.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_tool_request_ptr() -> *const u8 {
    REQUEST.lock().expect("request lock").as_ptr()
}

/// The ticket of the tool call [`lmpc_host_tool_request_len`] published (0 when
/// there is none): the host answers with it.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_tool_ticket() -> u64 {
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    match abi.session.tool_ticket() {
        Some(local) => abi.ticket(local),
        None => 0,
    }
}

/// Answer the tool call `ticket` names with `length` bytes of the input buffer
/// (the result JSON). A ticket that is not pending — already answered, cancelled,
/// or from a previous session — is rejected.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_tool_result(ticket: u64, length: u32) -> i32 {
    let result = match read_input(length) {
        Ok(result) => result,
        Err(refusal) => return refusal,
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 3;
    };
    let Some(local) = abi.local_ticket(ticket) else {
        return 3;
    };
    match abi.session.tool_result_to(local, &result) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// The pending model request's length (0 when there is none).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_request_len() -> u32 {
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    match abi.session.request_json() {
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

/// The ticket of the model request [`lmpc_host_request_len`] published (0 when
/// there is none): the host answers with it.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_request_ticket() -> u64 {
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    match abi.session.request_ticket() {
        Some(local) => abi.ticket(local),
        None => 0,
    }
}

/// Answer the request `ticket` names with `length` bytes of the input buffer. A
/// ticket that is not pending — a cancelled turn, an earlier turn, or the
/// previous session — is rejected instead of being applied to whatever is
/// pending now.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_reply(ticket: u64, length: u32) -> i32 {
    let reply = match read_input(length) {
        Ok(reply) => reply,
        Err(refusal) => return refusal,
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 3;
    };
    let Some(local) = abi.local_ticket(ticket) else {
        return 3;
    };
    match abi.session.reply_to(local, &reply) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// The conversation so far (JSON) for the host to store, and its length.
///
/// Returns the length (the host copies that many bytes from
/// [`lmpc_host_request_ptr`]), `0` when there is no session or nothing to
/// store, and `-1` when the conversation is larger than
/// [`lmpc_session_state_cap`].
///
/// The `-1` is the point of the cap: a host must not be able to export a
/// conversation it cannot put back. Import is capped at the same number, so a
/// successful export is always restorable — see [`lmpc_session_import_begin`]
/// for states larger than the 64 KiB input buffer.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_export() -> i32 {
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    match abi.session.messages_json() {
        Ok(json) if json.len() > STATE_CAPACITY => TOO_LONG,
        Ok(json) => {
            let length = json.len() as u32;
            *REQUEST.lock().expect("request lock") = json;
            length as i32
        }
        Err(_) => 0,
    }
}

/// The largest conversation [`lmpc_session_export`] hands out and
/// [`lmpc_session_import_*`](lmpc_session_import_begin) takes back, in bytes.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_state_cap() -> u32 {
    STATE_CAPACITY as u32
}

/// Resume a stored conversation (`length` bytes of the input buffer) in the
/// current session; returns how many messages were restored, or 0 when the
/// conversation was refused (invalid JSON, a `length` past the input buffer or
/// the state cap, or a turn that has already been polled — import before
/// driving it).
///
/// This single-shot form goes through the 64 KiB input buffer, so a larger
/// conversation needs the chunked transfer:
/// [`lmpc_session_import_begin`] → [`lmpc_session_import_write`] (repeatedly) →
/// [`lmpc_session_import_commit`].
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_import(length: u32) -> u32 {
    let Ok(json) = read_input(length) else {
        return 0;
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    abi.session.restore(&json).unwrap_or(0) as u32
}

/// Begin a chunked import of `total` bytes. Returns 0, or `-1` when `total` is
/// past [`lmpc_session_state_cap`].
///
/// Beginning again supersedes an unfinished transfer (an interrupted one leaves
/// nothing behind), and the announced size is what every
/// [`lmpc_session_import_write`] is checked against, so a short or long
/// transfer cannot be committed as if it were complete.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_import_begin(total: u32) -> i32 {
    if total as usize > STATE_CAPACITY {
        return TOO_LONG;
    }
    let mut pending = PENDING_IMPORT.lock().expect("import lock");
    let mut buffer = Vec::new();
    buffer.resize(total as usize, 0);
    *pending = Some(PendingImport {
        buffer,
        covered: 0,
    });
    0
}

/// Copy `length` bytes of the input buffer into the pending import at `offset`,
/// which must be where the previous write ended (the transfer is sequential).
///
/// The bytes are *not* interpreted here: a chunk boundary may fall inside a
/// multi-byte character, so the JSON is validated at
/// [`lmpc_session_import_commit`] where the whole state is known. Returns 0,
/// `-1` when `length` is past the input buffer, `-3` when the write does not
/// continue the transfer exactly, or `-4` when there is no transfer in
/// progress.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_import_write(offset: u32, length: u32) -> i32 {
    let bytes = match read_input_bytes(length) {
        Ok(bytes) => bytes,
        Err(refusal) => return refusal,
    };
    let mut pending = PENDING_IMPORT.lock().expect("import lock");
    let Some(import) = pending.as_mut() else {
        return NO_IMPORT;
    };
    if offset as usize != import.covered {
        return RANGE;
    }
    let end = import.covered + bytes.len();
    if end > import.buffer.len() {
        return RANGE;
    }
    import.buffer[import.covered..end].copy_from_slice(&bytes);
    import.covered = end;
    0
}

/// Restore the conversation assembled by the chunked import; returns how many
/// messages were restored, or 0 when it was refused (the bytes are not UTF-8, not
/// a valid conversation, the transfer is incomplete, or the turn has already
/// been polled). The pending transfer is dropped either way.
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_import_commit() -> u32 {
    let Some(import) = PENDING_IMPORT.lock().expect("import lock").take() else {
        return 0;
    };
    if import.covered != import.buffer.len() {
        // An unfinished transfer is refused, not restored as far as it got.
        return 0;
    }
    let Ok(json) = String::from_utf8(import.buffer) else {
        return 0;
    };
    let guard = SESSION.lock().expect("session lock");
    let Some(abi) = guard.as_ref() else {
        return 0;
    };
    abi.session.restore(&json).unwrap_or(0) as u32
}

/// Drop an unfinished chunked import. Returns 0 (also when there was none).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_session_import_abort() -> i32 {
    *PENDING_IMPORT.lock().expect("import lock") = None;
    0
}

/// Cancel the running turn (state 4 afterwards; the trace shows how far it got).
#[unsafe(no_mangle)]
pub extern "C" fn lmpc_host_cancel() {
    if let Some(abi) = SESSION.lock().expect("session lock").as_mut() {
        abi.session.cancel();
    }
}
