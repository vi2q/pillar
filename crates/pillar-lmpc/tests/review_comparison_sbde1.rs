//! Review probes for `docs/PI-COMPARISON-sbde1.md`: the cases that reproduce a
//! suspected limitation assert the *limitation* (a "characterization" — passing
//! means the suspicion is confirmed, not that the product contract is good);
//! the cases that have since been fixed assert the *desired contract* instead,
//! so this file stays green and keeps them pinned. The headers below say which
//! is which. The fuller C1/C3 coverage lives in
//! `crates/pillar-agent/tests/tool_contract_parity.rs`.
use pillar_agent::{AgentTool, AgentToolResult, ToolExecutionMode};
use pillar_ai::types::{Content, Tool};
use pillar_lmpc::{FrameHost, HostModelSession, HostModelState};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

fn result() -> AgentToolResult {
    AgentToolResult {
        content: vec![Content::text("finished")],
        details: serde_json::json!({}),
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}
fn tool(calls: Arc<AtomicUsize>, release: Arc<AtomicBool>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "checked".into(),
            description: "review probe".into(),
            parameters: serde_json::json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
            constrained_sampling: None,
        },
        label: "checked".into(),
        prepare_arguments: None,
        execution_mode: Some(ToolExecutionMode::Parallel),
        execute: Arc::new(move |_, _, _, update| {
            let calls = calls.clone();
            let release = release.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(update) = update {
                    update(result());
                }
                futures::future::poll_fn(move |_| {
                    if release.load(Ordering::SeqCst) {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
                Ok(result())
            })
        }),
    }
}
fn next_model(session: &mut HostModelSession) {
    for _ in 0..100 {
        if session.poll(Duration::from_millis(1)) == HostModelState::NeedsModel {
            return;
        }
    }
    panic!("no model request");
}
fn finish(session: &mut HostModelSession) {
    for _ in 0..100 {
        match session.poll(Duration::from_millis(1)) {
            HostModelState::NeedsModel => {
                let ticket = session.request_ticket().unwrap();
                session
                    .reply_to(ticket, r#"[{"type":"text","text":"done"}]"#)
                    .unwrap();
            }
            HostModelState::Done => return,
            HostModelState::Failed => panic!("{:?}", session.error()),
            _ => {}
        }
    }
    panic!("turn never finished");
}
#[test]
fn rejects_a_missing_required_argument_before_execute() {
    // C1, fixed: the declared schema is enforced by the shared loop, so
    // `execute` is never reached (this used to run the tool).
    let calls = Arc::new(AtomicUsize::new(0));
    let host = FrameHost::new();
    let mut session = HostModelSession::start(
        &host,
        "probe",
        vec![tool(calls.clone(), Arc::new(AtomicBool::new(true)))],
    );
    next_model(&mut session);
    session
        .reply_to(
            session.request_ticket().unwrap(),
            r#"[{"type":"toolCall","id":"good","name":"checked","arguments":{}}]"#,
        )
        .unwrap();
    finish(&mut session);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "invalid arguments must not execute"
    );
}
#[test]
fn observes_progress_only_after_tool_settlement() {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let host = FrameHost::new();
    let mut session =
        HostModelSession::start(&host, "probe", vec![tool(calls.clone(), release.clone())]);
    next_model(&mut session);
    session
        .reply_to(
            session.request_ticket().unwrap(),
            r#"[{"type":"toolCall","id":"good","name":"checked","arguments":{"value":"ok"}}]"#,
        )
        .unwrap();
    for _ in 0..10 {
        session.poll(Duration::from_millis(1));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !session
            .trace()
            .events
            .iter()
            .any(|event| event == "tool_execution_update"),
        "characterization: progress is withheld while tool waits"
    );
    release.store(true, Ordering::SeqCst);
    finish(&mut session);
    assert!(
        session
            .trace()
            .events
            .iter()
            .any(|event| event == "tool_execution_update")
    );
}
#[test]
fn keeps_the_assistant_source_order_in_the_history() {
    // C3, fixed: results keep the positions of the assistant's tool calls
    // (this used to report the immediate failure first: [bad, good]).
    let host = FrameHost::new();
    let mut session = HostModelSession::start(
        &host,
        "probe",
        vec![tool(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(true)),
        )],
    );
    next_model(&mut session);
    session.reply_to(session.request_ticket().unwrap(), r#"[{"type":"toolCall","id":"good","name":"checked","arguments":{"value":"ok"}},{"type":"toolCall","id":"bad","name":"missing","arguments":{}}]"#).unwrap();
    finish(&mut session);
    let messages: serde_json::Value =
        serde_json::from_str(&session.messages_json().unwrap()).unwrap();
    let ids: Vec<_> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "toolResult")
        .map(|m| m["toolCallId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["good", "bad"],
        "the history keeps the assistant's call order"
    );
}
fn chain(host: Arc<FrameHost>, count: Arc<AtomicUsize>, remaining: usize) {
    let next = host.clone();
    host.enqueue(Box::pin(async move {
        count.fetch_add(1, Ordering::SeqCst);
        if remaining > 1 {
            chain(next, count, remaining - 1);
        }
    }));
}
#[test]
fn observes_one_pump_draining_a_thousand_generation_chain() {
    let host = FrameHost::new();
    let count = Arc::new(AtomicUsize::new(0));
    chain(host.clone(), count.clone(), 1000);
    host.pump(Duration::from_millis(1));
    assert_eq!(
        count.load(Ordering::SeqCst),
        1000,
        "characterization: elapsed is not an execution budget"
    );
}
fn put_prefix(text: &str) {
    let count = text.len().min(pillar_lmpc::lmpc_input_cap() as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), pillar_lmpc::lmpc_input_ptr(), count);
    }
}
#[test]
fn observes_abi_export_larger_than_import_capacity() {
    let capacity = pillar_lmpc::lmpc_input_cap() as usize;
    pillar_lmpc::lmpc_session_create(0);
    put_prefix(&"x".repeat(capacity));
    assert_eq!(
        pillar_lmpc::lmpc_host_turn_start((capacity + 1) as u32),
        0,
        "characterization: oversized length is silently clamped"
    );
    for _ in 0..100 {
        let state = pillar_lmpc::lmpc_host_poll();
        if state == 1 {
            let ticket = pillar_lmpc::lmpc_host_request_ticket();
            put_prefix("[]");
            assert_eq!(pillar_lmpc::lmpc_host_reply(ticket, 2), 0);
        }
        if state == 2 {
            break;
        }
    }
    let length = pillar_lmpc::lmpc_session_export();
    assert!(length as usize > capacity);
    let stored = unsafe {
        std::slice::from_raw_parts(pillar_lmpc::lmpc_host_request_ptr(), length as usize)
    }
    .to_vec();
    let stored = String::from_utf8(stored).unwrap();
    serde_json::from_str::<serde_json::Value>(&stored).unwrap();
    pillar_lmpc::lmpc_session_create(0);
    put_prefix(&stored);
    assert_eq!(
        pillar_lmpc::lmpc_session_import(length),
        0,
        "characterization: the exported valid conversation cannot be reimported"
    );
}
