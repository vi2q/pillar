//! The LMPC minimum as an always-on gate: one turn runs on host services only.
//!
//! A plain `#[test]` on purpose — the profile must not need a tokio runtime,
//! a terminal, a filesystem, or the extension VM.
//!
//! These tests share the process-global *spawner* (`install_host_services`),
//! so they take a lock instead of racing each other. The clock is no longer
//! shared: each frame host routes timers and timestamps to itself.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

static TESTS: Mutex<()> = Mutex::new(());

#[test]
fn a_turn_runs_with_host_services() {
    let _guard = TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let trace = pillar_lmpc::demo_turn("remember something").expect("the demo turn runs");

    // The loop reached its end and the host saw the tool round trip.
    assert!(
        trace
            .events
            .iter()
            .any(|event| event == "tool_execution_start"),
        "the host tool ran: {:?}",
        trace.events
    );
    assert_eq!(
        trace.events.last().map(String::as_str),
        Some("agent_end"),
        "{:?}",
        trace.events
    );

    // The transcript is the whole turn: prompt, tool call, tool result, answer.
    let roles: Vec<&str> = trace
        .messages
        .iter()
        .map(|(role, _)| role.as_str())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "toolResult", "assistant"],
        "{:?}",
        trace.messages
    );
    assert_eq!(trace.messages[0].1, "remember something");
    assert_eq!(trace.messages[2].1, "remembered: the answer is 42");
    assert_eq!(
        trace.messages[3].1, "the answer is 42",
        "the answer came back through the host model"
    );
}

/// A frame-driven host runs the same turn: no threads, no tokio, and a virtual
/// clock — the shape the embedding target (a game frame, a browser animation
/// frame) can actually provide. The trace must match the thread-driven one,
/// which is the §5-7 comparison in miniature.
#[test]
fn a_frame_driven_host_runs_the_same_turn() {
    let _guard = TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // The thread-driven run first (it installs the native defaults), then the
    // frame-driven one (which points the services at its own queue).
    let threaded =
        pillar_lmpc::demo_turn("remember something").expect("the thread-driven turn runs");

    let host = pillar_lmpc::FrameHost::new();
    let trace = pillar_lmpc::demo_turn_on(
        &host,
        "remember something",
        std::time::Duration::from_millis(1),
    )
    .expect("the frame-driven turn runs");

    assert_eq!(
        trace.messages, threaded.messages,
        "the host model produced a different transcript on the frame-driven host"
    );
    assert_eq!(
        trace.events, threaded.events,
        "the host model produced a different event trace on the frame-driven host"
    );

    // The host's own services were the ones used: it started the body, its
    // virtual clock advanced past the model's wait, and its queue drained.
    assert!(host.spawns() >= 1, "the host's spawner ran the loop body");
    assert!(
        host.now() >= std::time::Duration::from_millis(1),
        "the virtual clock advanced: {:?}",
        host.now()
    );
    assert!(host.is_idle(), "the turn left nothing queued");
}

/// A body that enqueues another body must not lose it: the pump merges what is
/// still pending with what was queued while polling (policy review sb39f R8 —
/// the pump used to assign the pending list back over the queue, dropping a
/// child enqueued during its parent's poll, so a host adapter that spawned and
/// awaited a sub-task saw it never run).
#[test]
fn a_task_that_enqueues_a_child_is_not_lost() {
    let _guard = TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let host = pillar_lmpc::FrameHost::new();
    let child_ran = Arc::new(AtomicUsize::new(0));
    let ran = Arc::clone(&child_ran);
    let parent = Arc::clone(&host);
    host.enqueue(Box::pin(async move {
        let ran = Arc::clone(&ran);
        parent.enqueue(Box::pin(async move {
            ran.fetch_add(1, Ordering::SeqCst);
        }));
    }));

    host.pump(Duration::from_millis(1));
    assert_eq!(
        child_ran.load(Ordering::SeqCst),
        1,
        "the child body ran in the same pump"
    );
    assert!(host.is_idle(), "nothing is left queued");
}

/// Two hosts in one process keep their own clock and their own timers: the
/// runtime's timer and wall clock are process-wide, so the dispatcher resolves
/// the host that is *polling* instead of whichever host installed last (policy
/// review sb39f R7 — host A's timers used to be registered on host B, so only
/// B's pump could resolve them).
#[test]
fn two_hosts_keep_their_own_clock_and_timers() {
    let _guard = TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let a = pillar_lmpc::FrameHost::new();
    let b = pillar_lmpc::FrameHost::new();
    // Both install; the second one must not take over the first one's clock.
    a.install();
    b.install();
    assert_ne!(a.id(), b.id());

    let a_slept = Arc::new(AtomicBool::new(false));
    let a_time = Arc::new(AtomicI64::new(-1));
    let flag = Arc::clone(&a_slept);
    let time = Arc::clone(&a_time);
    a.enqueue(Box::pin(async move {
        time.store(pillar_lmpc::now_millis(), Ordering::SeqCst);
        pillar_lmpc::sleep(Duration::from_secs(5)).await;
        flag.store(true, Ordering::SeqCst);
    }));
    let b_slept = Arc::new(AtomicBool::new(false));
    let b_time = Arc::new(AtomicI64::new(-1));
    let flag = Arc::clone(&b_slept);
    let time = Arc::clone(&b_time);
    b.enqueue(Box::pin(async move {
        time.store(pillar_lmpc::now_millis(), Ordering::SeqCst);
        pillar_lmpc::sleep(Duration::from_secs(1)).await;
        flag.store(true, Ordering::SeqCst);
    }));

    a.pump(Duration::from_millis(3000));
    b.pump(Duration::from_millis(1000));
    assert_eq!(
        a_time.load(Ordering::SeqCst),
        3000,
        "a task of host A reads A's clock"
    );
    assert_eq!(
        b_time.load(Ordering::SeqCst),
        1000,
        "a task of host B reads B's clock"
    );
    // B's wait was created during B's first frame, so it comes due one frame
    // later; A (5 s) is still waiting.
    b.pump(Duration::from_millis(1000));
    assert!(b_slept.load(Ordering::SeqCst), "B's pump resolved B's wait");
    assert!(
        !a_slept.load(Ordering::SeqCst),
        "A's 5 s wait must not be resolved by B's clock"
    );

    a.pump(Duration::from_millis(10_000));
    assert!(
        a_slept.load(Ordering::SeqCst),
        "A's own pump resolved A's wait"
    );
    assert!(a.is_idle() && b.is_idle());
}
