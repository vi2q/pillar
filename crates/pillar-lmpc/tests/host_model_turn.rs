//! The host-driven model protocol: the *host* answers the model requests, which
//! is how an engine or a page supplies its own brain (§4 "host model").
//!
//! A plain `#[test]`: the whole session runs on the frame host (no threads, no
//! tokio, no provider catalog). The same protocol is what `src/wasm.rs` exposes
//! to a JavaScript host.

use std::sync::Arc;
use std::time::Duration;

use pillar_lmpc::HostModelState;

/// Drive the session: answer every model request, stop at the end.
fn drive(
    session: &mut pillar_lmpc::HostModelSession,
    mut answer: impl FnMut(usize, &str) -> String,
) -> usize {
    let mut requests = 0usize;
    for _ in 0..10_000 {
        match session.poll(Duration::from_millis(1)) {
            HostModelState::NeedsModel => {
                let request = session.request_json().expect("a pending request");
                let reply = answer(requests, &request);
                session.reply(&reply).expect("the host answers");
                requests += 1;
            }
            HostModelState::Running => {}
            HostModelState::Done => return requests,
            HostModelState::Failed => panic!("the turn failed: {:?}", session.error()),
            HostModelState::Cancelled => panic!("the turn was cancelled"),
        }
    }
    panic!("the session never finished");
}

#[test]
fn the_host_answers_the_model_requests() {
    let host = Arc::new(pillar_lmpc::FrameHost::new());
    // No tools: the host model itself produces the answer.
    let mut session = pillar_lmpc::HostModelSession::start(&host, "who are you?", Vec::new());

    let requests = drive(&mut session, |_, request| {
        assert!(
            request.contains("who are you?"),
            "the request carries the context: {request}"
        );
        r#"[{"type":"text","text":"I am the host's NPC."}]"#.to_string()
    });
    assert_eq!(requests, 1, "one model round trip");

    let trace = session.trace();
    assert!(
        trace.events.iter().any(|event| event == "agent_end"),
        "{:?}",
        trace.events
    );
    let roles: Vec<&str> = trace
        .messages
        .iter()
        .map(|(role, _)| role.as_str())
        .collect();
    assert_eq!(roles, ["user", "assistant"], "{:?}", trace.messages);
    assert_eq!(trace.messages[1].1, "I am the host's NPC.");
    assert!(host.is_idle(), "the session left nothing queued");
}

#[test]
fn the_host_can_drive_a_tool_call() {
    let host = Arc::new(pillar_lmpc::FrameHost::new());
    // The host installs its own tool, so the model can call it.
    let host_tool = pillar_agent::AgentTool {
        tool: pillar_ai::types::Tool {
            name: "look_up".to_string(),
            description: "Look a fact up in the host".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"],
            }),
            constrained_sampling: None,
        },
        label: "Look up".to_string(),
        prepare_arguments: None,
        execute: std::sync::Arc::new(|_id, args, _signal, _update| {
            let key = args
                .get("key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Box::pin(async move {
                Ok(pillar_agent::AgentToolResult {
                    content: vec![pillar_ai::types::Content::text(format!("{key} = 42"))],
                    details: serde_json::json!({}),
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                })
            })
        }),
        execution_mode: Some(pillar_agent::ToolExecutionMode::Parallel),
    };
    let mut session =
        pillar_lmpc::HostModelSession::start(&host, "what is the answer?", vec![host_tool]);

    let requests = drive(&mut session, |index, _request| {
        if index == 0 {
            // The host's model decides to call the host's tool.
            r#"[{"type":"toolCall","id":"call-1","name":"look_up","arguments":{"key":"answer"}}]"#
                .to_string()
        } else {
            r#"[{"type":"text","text":"the host says 42"}]"#.to_string()
        }
    });
    assert_eq!(
        requests, 2,
        "the tool call needed a second model round trip"
    );

    let trace = session.trace();
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
    assert_eq!(trace.messages[2].1, "answer = 42", "the host tool ran");
    assert_eq!(trace.messages[3].1, "the host says 42");
}

#[test]
fn a_host_reply_must_be_json() {
    let host = Arc::new(pillar_lmpc::FrameHost::new());
    let mut session = pillar_lmpc::HostModelSession::start(&host, "hi", Vec::new());
    // One poll is enough: the guest publishes its request in the first frame.
    assert_eq!(
        session.poll(Duration::from_millis(1)),
        HostModelState::NeedsModel
    );
    let error = session
        .reply("not json")
        .expect_err("the reply is rejected");
    assert!(error.contains("bad reply JSON"), "{error}");
    // The request stays pending, so the host can try again.
    assert!(session.needs_model());
    session
        .reply(r#"[{"type":"text","text":"ok"}]"#)
        .expect("a valid reply is accepted");
}

/// The host can cancel a running turn (a game cancels an NPC's turn when the
/// scene changes): the guest stops without waiting for another model answer.
#[test]
fn the_host_can_cancel_a_turn() {
    use std::time::Duration;

    let host = Arc::new(pillar_lmpc::FrameHost::new());
    let mut session = pillar_lmpc::HostModelSession::start(&host, "who are you?", Vec::new());

    // Wait until the guest asks for a model answer, then cancel instead of
    // answering.
    assert_eq!(
        session.poll(Duration::from_millis(1)),
        HostModelState::NeedsModel
    );
    session.cancel();
    assert!(session.is_cancelled());

    let mut state = HostModelState::Running;
    for _ in 0..1000 {
        state = session.poll(Duration::from_millis(1));
        if matches!(
            state,
            HostModelState::Cancelled | HostModelState::Done | HostModelState::Failed
        ) {
            break;
        }
    }
    assert_eq!(
        state,
        HostModelState::Cancelled,
        "the cancel ended the turn"
    );

    // The trace shows how far the turn got: the prompt, and no model answer
    // (the guest stops instead of waiting for a reply that will not come).
    let trace = session.trace();
    assert_eq!(
        trace.messages[0].0, "user",
        "the prompt is there: {:?}",
        trace.messages
    );
    let answered = trace
        .messages
        .iter()
        .any(|(role, text)| role == "assistant" && !text.is_empty());
    assert!(
        !answered,
        "no model answer was recorded: {:?}",
        trace.messages
    );
    assert!(
        !session.needs_model(),
        "the guest stopped rather than waiting for a reply"
    );
}

/// A session spans several turns: the agent keeps its state, so the second
/// turn's model request carries the first turn's messages (an NPC remembers).
#[test]
fn a_session_spans_several_turns() {
    use std::time::Duration;

    let host = Arc::new(pillar_lmpc::FrameHost::new());
    let mut session = pillar_lmpc::HostModelSession::start(&host, "my name is Ada", Vec::new());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let answer = |session: &mut pillar_lmpc::HostModelSession,
                  seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
                  text: &str| {
        for _ in 0..1000 {
            match session.poll(Duration::from_millis(1)) {
                HostModelState::NeedsModel => {
                    let request = session.request_json().expect("a pending request");
                    seen.lock().unwrap().push(request);
                    let reply = format!("[{{\"type\":\"text\",\"text\":\"{text}\"}}]");
                    session.reply(&reply).expect("the host answers");
                    // Keep polling until the turn ends.
                }
                HostModelState::Running => {}
                HostModelState::Done => return,
                other => panic!("unexpected state {other:?}"),
            }
        }
        panic!("the turn did not finish");
    };

    answer(&mut session, &seen, "hello Ada");
    let first = session.trace();
    assert_eq!(first.messages.len(), 2, "{:?}", first.messages);

    session.say("what is my name?").expect("a second turn");
    answer(&mut session, &seen, "Ada");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one model request per turn");
    assert!(
        requests[1].contains("my name is Ada") && requests[1].contains("hello Ada"),
        "the second request carries the first turn: {}",
        requests[1]
    );

    let trace = session.trace();
    let roles: Vec<&str> = trace
        .messages
        .iter()
        .map(|(role, _)| role.as_str())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "assistant"],
        "{:?}",
        trace.messages
    );
    assert_eq!(trace.messages[3].1, "Ada");
}

/// The host can stream partial text before its final answer: the guest forwards
/// each delta as a `message_update` event (a game shows text as it arrives).
#[test]
fn the_host_can_stream_partial_text() {
    use std::time::Duration;

    let host = Arc::new(pillar_lmpc::FrameHost::new());
    let mut session = pillar_lmpc::HostModelSession::start(&host, "tell me", Vec::new());

    let mut done = false;
    for _ in 0..1000 {
        match session.poll(Duration::from_millis(1)) {
            HostModelState::NeedsModel => {
                for delta in ["the ", "answer ", "is 42"] {
                    session.stream_delta(delta).expect("a delta");
                }
                session
                    .reply(r#"[{"type":"text","text":"the answer is 42"}]"#)
                    .expect("the final answer");
            }
            HostModelState::Running => {}
            HostModelState::Done => {
                done = true;
                break;
            }
            other => panic!("unexpected state {other:?}"),
        }
    }
    assert!(done, "the turn finished");

    let trace = session.trace();
    let updates = trace
        .events
        .iter()
        .filter(|event| event.as_str() == "message_update")
        .count();
    assert_eq!(updates, 3, "one update per delta: {:?}", trace.events);
    assert_eq!(trace.messages[1].1, "the answer is 42");
}
