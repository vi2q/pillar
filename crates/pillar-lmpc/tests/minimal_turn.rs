//! The LMPC minimum as an always-on gate: one turn runs on host services only.
//!
//! A plain `#[test]` on purpose — the profile must not need a tokio runtime,
//! a terminal, a filesystem, or the extension VM.
//!
//! The two tests share the process-global host clock, so they take a lock
//! instead of racing each other.

use std::sync::Mutex;

static TESTS: Mutex<()> = Mutex::new(());

#[test]
fn a_turn_runs_with_host_services() {
    let _guard = TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// The host can install its own clock and spawner; the demo uses them for the
/// waits and the loop body. This records both to prove the wiring.
#[test]
fn host_services_are_used_but_replaceable() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let _guard = TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let spawns = Arc::new(AtomicUsize::new(0));
    let seen_spawns = Arc::clone(&spawns);
    let waits: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_waits = Arc::clone(&waits);

    pillar_lmpc::install_host_services(
        Arc::new(move |body| {
            seen_spawns.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || futures::executor::block_on(body));
        }),
        Some(Arc::new(move |duration| {
            seen_waits.lock().unwrap().push(duration);
            Box::pin(async {})
        })),
    );

    let trace = pillar_lmpc::demo_turn("hello").expect("the demo turn runs");
    assert!(
        spawns.load(Ordering::SeqCst) >= 1,
        "the host spawner ran the loop"
    );
    assert!(
        !waits.lock().unwrap().is_empty(),
        "the host clock served the model's wait"
    );
    assert_eq!(trace.messages[3].1, "the answer is 42");

    pillar_ai::set_default_sleep(None);
}
