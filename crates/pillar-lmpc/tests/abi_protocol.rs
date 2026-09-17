//! The C ABI itself, driven from the native target.
//!
//! `src/abi.rs` is compiled for every target, so these tests call exactly the
//! functions a Wasm / engine host calls (`scripts/wasm_host_model.mjs` is the
//! JavaScript twin) — the same order, the same buffers, the same tickets. Policy
//! review sb39f R3 asked for this: a Rust-API reproduction cannot catch an ABI
//! that publishes the first request before the host imports its stored
//! conversation, and R4 asked for stale answers to be refused *at the boundary*,
//! not only in the Rust API.
//!
//! All calls share the crate's static session, so everything lives in one test.

use std::sync::atomic::{AtomicU64, Ordering};

/// The ABI's states (see `src/abi.rs`).
const NEEDS_MODEL: i32 = 1;
const DONE: i32 = 2;
const FAILED: i32 = 3;
const CANCELLED: i32 = 4;

/// Put `text` into the guest's input buffer and return its length (what the ABI
/// functions take).
fn put(text: &str) -> u32 {
    let capacity = pillar_lmpc::lmpc_input_cap() as usize;
    assert!(text.len() <= capacity, "the input buffer holds {capacity} bytes");
    // Ask for the pointer after the capacity: the first call allocates the
    // buffer, and a pointer taken before it would dangle.
    let pointer = pillar_lmpc::lmpc_input_ptr();
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len()) };
    text.len() as u32
}

/// Copy `length` bytes out of a guest buffer.
fn read(pointer: *const u8, length: u32) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) }.to_vec();
    String::from_utf8(bytes).expect("the guest stores UTF-8")
}

fn request_text() -> String {
    let length = pillar_lmpc::lmpc_host_request_len();
    read(pillar_lmpc::lmpc_host_request_ptr(), length)
}

fn trace_text() -> String {
    let length = pillar_lmpc::lmpc_trace_len();
    read(pillar_lmpc::lmpc_trace_ptr(), length)
}

/// Answer every published request with the scripted replies until the turn ends;
/// returns the final state.
fn drive(replies: &mut usize) -> i32 {
    for _ in 0..10_000 {
        let state = pillar_lmpc::lmpc_host_poll();
        if state == NEEDS_MODEL {
            let ticket = pillar_lmpc::lmpc_host_request_ticket();
            assert_ne!(ticket, 0, "a published request carries a ticket");
            let reply = pillar_lmpc::host_model_scripted_reply(*replies);
            assert_eq!(
                pillar_lmpc::lmpc_host_reply(ticket, put(reply)),
                0,
                "the guest accepts the answer for the ticket it published"
            );
            *replies += 1;
            continue;
        }
        if state == DONE || state == FAILED || state == CANCELLED {
            return state;
        }
    }
    panic!("the ABI turn never finished");
}

#[test]
fn the_abi_restores_a_conversation_before_the_first_poll() {
    // ---- turn 1: run it and export the conversation ------------------------
    // The ticket epoch is per session: a ticket from one session must not name
    // anything in the next, so remember what turn 1 published.
    static FIRST_TICKET: AtomicU64 = AtomicU64::new(0);

    assert_eq!(pillar_lmpc::lmpc_session_create(0), 0);
    // Creating a session publishes nothing: the host can still import.
    assert_eq!(
        pillar_lmpc::lmpc_host_request_ticket(),
        0,
        "no request before the first poll"
    );
    assert_eq!(pillar_lmpc::lmpc_host_turn_start(put("remember this")), 0);

    let mut replies = 0usize;
    for _ in 0..10_000 {
        let state = pillar_lmpc::lmpc_host_poll();
        if state == NEEDS_MODEL {
            let ticket = pillar_lmpc::lmpc_host_request_ticket();
            FIRST_TICKET.store(ticket, Ordering::SeqCst);
            let request = request_text();
            assert!(
                request.contains("remember this"),
                "the first request carries the prompt: {request}"
            );
            assert_eq!(
                pillar_lmpc::lmpc_host_reply(
                    ticket,
                    put(pillar_lmpc::host_model_scripted_reply(replies))
                ),
                0
            );
            replies += 1;
            continue;
        }
        if state == DONE {
            break;
        }
        if state == FAILED {
            panic!("turn 1 failed: {}", trace_text());
        }
    }

    // The stored conversation goes through the export buffer.
    let stored = {
        let length = pillar_lmpc::lmpc_session_export();
        assert!(length > 0, "the session exported its conversation");
        read(pillar_lmpc::lmpc_host_request_ptr(), length)
    };
    assert!(stored.contains("remember this"), "{stored}");

    // A request that is already answered cannot be answered again — the model
    // side of the ticket check.
    assert_ne!(
        pillar_lmpc::lmpc_host_reply(FIRST_TICKET.load(Ordering::SeqCst), put("[]")),
        0,
        "an answer for an already answered request is refused"
    );

    // ---- turn 2 in a *new* session, restored before the first poll ---------
    assert_eq!(pillar_lmpc::lmpc_session_create(0), 0);
    let restored = pillar_lmpc::lmpc_session_import(put(&stored));
    assert!(restored > 0, "the conversation was restored: {restored}");
    assert_eq!(pillar_lmpc::lmpc_host_turn_start(put("what is my name?")), 0);

    let state = pillar_lmpc::lmpc_host_poll();
    assert_eq!(state, NEEDS_MODEL, "the first poll publishes the request");
    let request = request_text();
    assert!(
        request.contains("remember this") && request.contains("what is my name?"),
        "the resumed request carries the stored conversation *and* the new prompt: {request}"
    );
    let ticket = pillar_lmpc::lmpc_host_request_ticket();
    assert_ne!(ticket, 0);

    // The new session is a new epoch: turn 1's ticket names nothing here.
    assert_ne!(
        pillar_lmpc::lmpc_host_reply(FIRST_TICKET.load(Ordering::SeqCst), put("[]")),
        0,
        "the previous session's ticket is refused at the boundary"
    );
    // Importing once the turn is driven is refused (returns 0 messages).
    assert_eq!(
        pillar_lmpc::lmpc_session_import(put(&stored)),
        0,
        "the import must be refused after the first poll"
    );

    assert_eq!(
        pillar_lmpc::lmpc_host_reply(ticket, put(r#"[{"type":"text","text":"Ada"}]"#)),
        0
    );
    let mut replies = 1usize;
    assert_eq!(drive(&mut replies), DONE);
    let trace = trace_text();
    assert!(trace.contains("Ada"), "{trace}");

    // ---- cancelling voids the published request at the boundary -----------
    assert_eq!(pillar_lmpc::lmpc_session_create(0), 0);
    assert_eq!(pillar_lmpc::lmpc_host_turn_start(put("stop me")), 0);
    assert_eq!(pillar_lmpc::lmpc_host_poll(), NEEDS_MODEL);
    let cancelled_ticket = pillar_lmpc::lmpc_host_request_ticket();
    assert_ne!(cancelled_ticket, 0);
    pillar_lmpc::lmpc_host_cancel();
    assert_ne!(
        pillar_lmpc::lmpc_host_reply(cancelled_ticket, put(r#"[{"type":"text","text":"late"}]"#)),
        0,
        "an answer after the cancel is refused"
    );
    assert_ne!(
        pillar_lmpc::lmpc_host_stream(cancelled_ticket, put("late")),
        0,
        "a stale stream delta is refused too"
    );
}
