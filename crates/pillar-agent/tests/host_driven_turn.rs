//! A whole turn on host-provided services only: no tokio runtime, no timer
//! driver, no TUI, no extension VM.
//!
//! This is the §5-2 host-driven shape (docs/DEVELOPMENT-STRATEGY.md): the host
//! supplies where the loop body runs (`AgentOptions::spawn`) and where waiting
//! happens (`pillar_ai::set_default_sleep`). The test is deliberately *not*
//! `#[tokio::test]` — nothing in this path may need a tokio reactor, and this
//! binary would deadlock or panic if something did.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pillar_agent::types::ToolExecuteFuture;
use pillar_agent::{
    Agent, AgentOptions, AgentState, AgentTool, AgentToolResult, SpawnFn, StreamFn,
    ToolExecutionMode,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessageEvent, Content, StopReason, Usage, UsageCost,
};

fn usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost::default(),
    }
}

fn assistant(content: Vec<Content>, stop_reason: StopReason) -> pillar_ai::AssistantMessage {
    pillar_ai::AssistantMessage {
        content,
        api: "host".into(),
        provider: "host".into(),
        model: "host".into(),
        response_model: None,
        usage: usage(),
        stop_reason,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// A scripted stream function that *waits* before answering: the wait has to go
/// through the host clock (the default would need a tokio timer).
fn scripted_stream_fn(
    answers: Arc<Mutex<Vec<Vec<Content>>>>,
) -> StreamFn {
    StreamFn::new(move |_context, _options| {
        let answers = Arc::clone(&answers);
        async move {
            pillar_ai::sleep(Duration::from_millis(5)).await;
            let stream = assistant_message_event_stream();
            let content = answers.lock().unwrap().remove(0);
            let stop_reason = if content
                .iter()
                .any(|part| matches!(part, Content::ToolCall { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
            stream.push(AssistantMessageEvent::Done {
                reason: stop_reason,
                message: assistant(content, stop_reason),
            });
            stream
        }
    })
}

fn echo_tool(seen: Arc<Mutex<Vec<String>>>) -> AgentTool {
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "echo".to_string(),
            description: "Echo the value back".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
            }),
            constrained_sampling: None,
        },
        label: "Echo".to_string(),
        execute: Arc::new(
            move |_id: String,
                  args: serde_json::Value,
                  _signal: Option<pillar_agent::AbortSignal>,
                  _on_update: Option<pillar_agent::AgentToolUpdateCallback>|
                  -> ToolExecuteFuture {
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    let value = args
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    seen.lock().unwrap().push(value.clone());
                    Ok(AgentToolResult {
                        content: vec![pillar_ai::types::Content::text(format!("echo:{value}"))],
                        details: serde_json::json!({}),
                        usage: None,
                        added_tool_names: None,
                        terminate: false,
                    })
                })
            },
        ),
        execution_mode: Some(ToolExecutionMode::Parallel),
        prepare_arguments: None,
    }
}

#[test]
fn a_turn_runs_on_host_services_only() {
    // The host clock: record the waits and resolve them immediately.
    let waits = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let seen_waits = Arc::clone(&waits);
    pillar_ai::set_default_sleep(Some(Arc::new(move |duration| {
        seen_waits.lock().unwrap().push(duration);
        Box::pin(async {})
    })));

    // The host spawner: a plain thread with a futures executor — no tokio task
    // and no reactor. (A single-threaded host would use its own queue; the
    // point here is that the loop asks the host instead of tokio.)
    let spawns = Arc::new(AtomicUsize::new(0));
    let seen_spawns = Arc::clone(&spawns);
    let handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_handles = Arc::clone(&handles);
    let host_spawner: SpawnFn = Arc::new(move |body| {
        seen_spawns.fetch_add(1, Ordering::SeqCst);
        seen_handles
            .lock()
            .unwrap()
            .push(std::thread::spawn(move || futures::executor::block_on(body)));
    });

    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let answers = Arc::new(Mutex::new(vec![
        vec![Content::tool_call(
            "call-1",
            "echo",
            serde_json::json!({ "value": "hello" }),
        )],
        vec![Content::text("done")],
    ]));
    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentState {
            system_prompt: String::new(),
            model: pillar_agent::FauxModelRef {
                id: "host".into(),
                name: "host".into(),
                api: "host".into(),
                provider: "host".into(),
                base_url: "https://example.invalid".into(),
                reasoning: false,
                input: vec!["text".into()],
                cost: UsageCost::default(),
                context_window: 8192,
                max_tokens: 2048,
            },
            tools: vec![echo_tool(Arc::clone(&tool_calls))],
            ..Default::default()
        }),
        stream_fn: Some(scripted_stream_fn(answers)),
        spawn: Some(host_spawner),
        ..AgentOptions::new(StreamFn::new(|_, _| async {
            unreachable!("the agent takes its stream fn explicitly")
        }))
    });

    // No tokio runtime in this test: the turn is driven by the host spawner and
    // the outer futures executor.
    futures::executor::block_on(agent.prompt("hi")).expect("prompt");
    for handle in handles.lock().unwrap().drain(..) {
        handle.join().expect("the host's run body finished");
    }

    assert_eq!(spawns.load(Ordering::SeqCst), 1, "the host drove the run body");
    assert!(!waits.lock().unwrap().is_empty(), "waiting used the host clock");
    assert_eq!(
        tool_calls.lock().unwrap().as_slice(),
        ["hello"],
        "the tool ran"
    );
    let state = agent.state();
    assert_eq!(
        state.messages.len(),
        4,
        "user + tool call + tool result + answer: {:?}",
        state.messages
    );
    assert!(!state.is_streaming);
    assert!(
        matches!(
            state.messages.last(),
            Some(pillar_agent::AgentMessage::Message(
                pillar_ai::types::Message::Assistant(_)
            ))
        ),
        "the turn ended on the assistant's answer: {:?}",
        state.messages.last()
    );

    pillar_ai::set_default_sleep(None);
}
