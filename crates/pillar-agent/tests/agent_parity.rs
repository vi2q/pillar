//! Port of packages/agent/test/agent.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream case, same names in comments. The mock stream
//! function replays scripted assistant messages via
//! `AssistantMessageEventStream`; typebox schemas become JSON-Schema objects.
//!
//! divergence: prompts resolve to `Result` instead of throwing; busy-state
//! and continuation errors are asserted via `Err` payloads.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::{
    Agent, AgentEvent, AgentOptions, AgentState, AgentThinkingLevel, AgentTool, AgentToolResult,
    FauxModelRef, QueueMode, ToolExecuteError, set_default_stream_fn,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{Content, Message, StopReason, Tool, Usage, UsageCost};

type DelayedUpdateSlot = Arc<Mutex<Option<pillar_agent::AgentToolUpdateCallback>>>;

fn create_usage() -> Usage {
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

fn mock_model() -> FauxModelRef {
    FauxModelRef {
        id: "mock".into(),
        name: "mock".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec![],
        cost: UsageCost::default(),
        context_window: 0,
        max_tokens: 0,
    }
}

fn create_assistant_message(text: &str) -> pillar_ai::AssistantMessage {
    pillar_ai::AssistantMessage {
        content: vec![Content::text(text)],
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "mock".into(),
        response_model: None,
        usage: create_usage(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

fn create_assistant_tool_use_message(content: Vec<Content>) -> pillar_ai::AssistantMessage {
    pillar_ai::AssistantMessage {
        content,
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "mock".into(),
        response_model: None,
        usage: create_usage(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

fn create_user_message(text: &str) -> AgentEventMessage {
    Message::User {
        content: pillar_ai::types::UserContent::Text(text.to_owned()),
        timestamp: 1,
    }
    .into()
}

/// AgentMessage alias to keep helper signatures short.
use pillar_agent::AgentMessage as AgentEventMessage;
use pillar_agent::PrepareNextFuture as PrepareNextFutureAlias;
use pillar_agent::StopFuture as StopFutureAlias;

/// Stream fn that resolves with a fixed message on every call.
fn done_stream_fn(text: &'static str) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| async move {
        let stream = assistant_message_event_stream();
        stream.push(pillar_ai::types::AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: create_assistant_message(text),
        });
        stream
    })
}

fn noop_tool() -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "noop".into(),
            description: "Noop tool".into(),
            parameters: serde_json::json!({"type": "object"}),
            constrained_sampling: None,
        },
        label: "Noop".into(),
        prepare_arguments: None,
        execute: Arc::new(|_id, _args, _signal, _on_update| {
            Box::pin(async move {
                Ok(AgentToolResult {
                    content: vec![Content::text("ok")],
                    details: serde_json::json!({}),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}

fn agent_from(options: AgentOptions) -> Arc<Agent> {
    Arc::new(Agent::new(options))
}

#[tokio::test]
async fn uses_the_configured_default_when_a_legacy_caller_omits_stream_fn() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_fn = Arc::clone(&calls);
    set_default_stream_fn(Some(pillar_agent::StreamFn::new(
        move |_context, _options| {
            let calls = Arc::clone(&calls_for_fn);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let stream = assistant_message_event_stream();
                stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: create_assistant_message("fallback"),
                });
                stream
            }
        },
    )));

    let agent = match std::panic::catch_unwind(|| Agent::new(options_with(None))) {
        Ok(agent) => agent,
        Err(_) => {
            set_default_stream_fn(None);
            panic!("default stream fn was not configured");
        }
    };
    agent.prompt("Hello").await.expect("prompt");

    set_default_stream_fn(None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn should_create_an_agent_instance_with_default_state() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));
    let state = agent.state();

    assert_eq!(state.system_prompt, "");
    assert_eq!(state.model, FauxModelRef::unknown());
    assert_eq!(state.thinking_level, AgentThinkingLevel::Off);
    assert!(state.tools.is_empty());
    assert!(state.messages.is_empty());
    assert!(!state.is_streaming);
    assert!(state.streaming_message.is_none());
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.error_message.is_none());
}

#[tokio::test]
async fn should_create_an_agent_instance_with_custom_initial_state() {
    let agent = agent_from(with_initial_state(
        AgentOptions::new(unused_stream_fn()),
        AgentState {
            system_prompt: "You are a helpful assistant.".into(),
            model: mock_model(),
            thinking_level: AgentThinkingLevel::Low,
            ..Default::default()
        },
    ));

    let state = agent.state();
    assert_eq!(state.system_prompt, "You are a helpful assistant.");
    assert_eq!(state.model, mock_model());
    assert_eq!(state.thinking_level, AgentThinkingLevel::Low);
}

#[tokio::test]
async fn should_subscribe_to_events() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));

    let event_count = Arc::new(AtomicU32::new(0));
    let count_for_listener = Arc::clone(&event_count);
    let unsubscribe = agent.subscribe(move |_event, _signal| {
        let count = Arc::clone(&count_for_listener);
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
        }) as pillar_agent::ListenerFuture
    });

    // No initial event on subscribe.
    assert_eq!(event_count.load(Ordering::SeqCst), 0);

    // State mutators don't emit events.
    agent.set_system_prompt("Test prompt");
    assert_eq!(event_count.load(Ordering::SeqCst), 0);
    assert_eq!(agent.state().system_prompt, "Test prompt");

    // Unsubscribe should work.
    unsubscribe();
    agent.set_system_prompt("Another prompt");
    assert_eq!(event_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn emits_full_lifecycle_events_for_thrown_run_failures() {
    let stream_fn = pillar_agent::StreamFn::new(|_context, _options| async move {
        panic_with_event_stream("provider exploded")
    });
    let agent = agent_from(AgentOptions::new(stream_fn));
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_listener = Arc::clone(&events);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let events = Arc::clone(&events_for_listener);
        Box::pin(async move {
            events.lock().unwrap().push(event_kind(&event));
        }) as pillar_agent::ListenerFuture
    });

    agent.prompt("hello").await.expect("prompt");

    let kinds = events.lock().unwrap().clone();
    assert_eq!(
        kinds,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    let state = agent.state();
    let last = state.messages.last().expect("last message").clone();
    match last.as_base_message() {
        Message::Assistant(assistant) => {
            assert_eq!(assistant.stop_reason, StopReason::Error);
            assert_eq!(
                assistant.error_message.as_deref(),
                Some("provider exploded")
            );
        }
        other => panic!("expected assistant message, got {other:?}"),
    }
    assert_eq!(state.error_message.as_deref(), Some("provider exploded"));
}

#[tokio::test]
async fn should_await_async_subscribers_before_prompt_resolves() {
    let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel::<()>();
    let barrier = Arc::new(tokio::sync::Mutex::new(Some(barrier_tx)));
    let agent = agent_from(AgentOptions::new(done_stream_fn("ok")));

    let listener_finished = Arc::new(AtomicBool::new(false));
    let finished_for_listener = Arc::clone(&listener_finished);
    let barrier_for_listener = Arc::clone(&barrier);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let finished = Arc::clone(&finished_for_listener);
        let barrier = Arc::clone(&barrier_for_listener);
        Box::pin(async move {
            if event_kind(&event) == "agent_end" {
                // Wait for the test to release the barrier.
                if let Some(tx) = barrier.lock().await.take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                finished.store(true, Ordering::SeqCst);
            }
        }) as pillar_agent::ListenerFuture
    });

    let agent_for_prompt = Arc::clone(&agent);
    let prompt_task = tokio::spawn(async move {
        let _ = agent_for_prompt.prompt("hello").await;
    });

    // Signal the listener, then verify the prompt has not resolved yet.
    let () = barrier_rx.await.expect("barrier");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(!listener_finished.load(Ordering::SeqCst));
    assert!(agent.state().is_streaming);
    assert!(!prompt_task.is_finished(), "prompt should still be pending");

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert!(listener_finished.load(Ordering::SeqCst));

    prompt_task.await.unwrap();
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn wait_for_idle_should_wait_for_async_subscribers() {
    let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel::<()>();
    let barrier = Arc::new(tokio::sync::Mutex::new(Some(barrier_tx)));
    let agent = agent_from(AgentOptions::new(done_stream_fn("ok")));

    let _unsubscribe = agent.subscribe(move |_event, _signal| {
        let barrier = Arc::clone(&barrier);
        Box::pin(async move {
            if let Some(tx) = barrier.lock().await.take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }) as pillar_agent::ListenerFuture
    });

    let agent_for_prompt = Arc::clone(&agent);
    let prompt_task = tokio::spawn(async move {
        agent_for_prompt.prompt("hello").await.expect("prompt");
    });

    let () = barrier_rx.await.expect("barrier");
    let agent_for_idle = Arc::clone(&agent);
    let idle_task = tokio::spawn(async move {
        agent_for_idle.wait_for_idle().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(!idle_task.is_finished(), "idle should still be waiting");
    assert!(agent.state().is_streaming);

    tokio::join!(async {
        prompt_task.await.unwrap();
        idle_task.await.unwrap();
    });
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn should_pass_the_active_abort_signal_to_subscribers() {
    let agent = agent_from(AgentOptions::new(abort_aware_stream_fn()));

    let received_signal = Arc::new(Mutex::new(None::<pillar_agent::AbortSignal>));
    let signal_for_listener = Arc::clone(&received_signal);
    let _unsubscribe = agent.subscribe(move |event, signal| {
        let slot = Arc::clone(&signal_for_listener);
        Box::pin(async move {
            if event_kind(&event) == "agent_start" {
                *slot.lock().unwrap() = signal;
            }
        }) as pillar_agent::ListenerFuture
    });

    let agent_for_prompt = Arc::clone(&agent);
    let prompt_task = tokio::spawn(async move {
        agent_for_prompt.prompt("hello").await.expect("prompt");
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    {
        let guard = received_signal.lock().unwrap();
        let signal = guard.as_ref().expect("signal received");
        assert!(!signal.is_aborted());
    }

    agent.abort();
    prompt_task.await.unwrap();

    let guard = received_signal.lock().unwrap();
    assert!(guard.as_ref().expect("signal").is_aborted());
}

#[tokio::test]
async fn should_ignore_tool_updates_after_the_tool_execution_settles() {
    let delayed_update: DelayedUpdateSlot = Arc::new(Mutex::new(None));
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let slot_for_tool = Arc::clone(&delayed_update);
    let mut tool = noop_tool();
    tool.tool.name = "delayed_tool".into();
    tool.execute = Arc::new(move |_id, _args, _signal, on_update| {
        let slot = Arc::clone(&slot_for_tool);
        Box::pin(async move {
            if let Some(on_update) = &on_update {
                on_update(AgentToolResult {
                    content: vec![Content::text("running")],
                    details: serde_json::json!({"status": "running"}),
                    ..Default::default()
                });
                *slot.lock().unwrap() = Some(on_update.clone());
            }
            Ok(AgentToolResult {
                content: vec![Content::text("ok")],
                details: serde_json::json!({"status": "done"}),
                terminate: true,
                ..Default::default()
            })
        })
    });

    let stream_fn = tool_call_stream_fn(
        "delayed_tool",
        vec![Content::tool_call(
            "call-1",
            "delayed_tool",
            serde_json::json!({}),
        )],
    );
    let options = with_initial_state(
        AgentOptions::new(stream_fn),
        AgentState {
            tools: vec![tool],
            ..Default::default()
        },
    );
    let agent = agent_from(options);
    let events_for_listener = Arc::clone(&events);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let events = Arc::clone(&events_for_listener);
        Box::pin(async move {
            events.lock().unwrap().push(event);
        }) as pillar_agent::ListenerFuture
    });

    agent.prompt("run tool").await.expect("prompt");
    let event_count_after_prompt = events.lock().unwrap().len();

    // Late update after the run has settled: must be ignored.
    let late_update = delayed_update.lock().unwrap().take();
    if let Some(on_update) = late_update {
        on_update(AgentToolResult {
            content: vec![Content::text("late")],
            details: serde_json::json!({"status": "late"}),
            ..Default::default()
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let guard = events.lock().unwrap();
    let updates = guard
        .iter()
        .filter(|event| event_kind(event) == "tool_execution_update")
        .count();
    assert_eq!(updates, 1);
    assert_eq!(guard.len(), event_count_after_prompt);
}

#[tokio::test]
async fn should_ignore_a_settled_parallel_tool_update_while_another_tool_is_still_running() {
    let (slow_started_tx, slow_started_rx) = tokio::sync::oneshot::channel::<()>();
    let (settled_ended_tx, settled_ended_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_slow_tx, release_slow_rx) = tokio::sync::oneshot::channel::<()>();
    let settled_tool_update: DelayedUpdateSlot = Arc::new(Mutex::new(None));
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let settled_slot = Arc::clone(&settled_tool_update);
    let mut settled_tool = noop_tool();
    settled_tool.tool.name = "settled_tool".into();
    settled_tool.execute = Arc::new(move |_id, _args, _signal, on_update| {
        let slot = Arc::clone(&settled_slot);
        Box::pin(async move {
            if let Some(on_update) = &on_update {
                *slot.lock().unwrap() = Some(on_update.clone());
            }
            Ok(AgentToolResult {
                content: vec![Content::text("done")],
                details: serde_json::json!({"status": "done"}),
                terminate: true,
                ..Default::default()
            })
        })
    });

    let slow_started = Arc::new(Mutex::new(Some(slow_started_tx)));
    let release_slow = Arc::new(Mutex::new(Some(release_slow_rx)));
    let mut slow_tool = noop_tool();
    slow_tool.tool.name = "slow_tool".into();
    slow_tool.execute = Arc::new(move |_id, _args, _signal, _on_update| {
        let started = Arc::clone(&slow_started);
        let release = Arc::clone(&release_slow);
        Box::pin(async move {
            let tx = started.lock().unwrap().take();
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
            let rx = release.lock().unwrap().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(AgentToolResult {
                content: vec![Content::text("done")],
                details: serde_json::json!({"status": "done"}),
                terminate: true,
                ..Default::default()
            })
        })
    });

    let stream_fn = tool_call_stream_fn(
        "multi",
        vec![
            Content::tool_call("call-1", "settled_tool", serde_json::json!({})),
            Content::tool_call("call-2", "slow_tool", serde_json::json!({})),
        ],
    );
    let options = with_initial_state(
        AgentOptions::new(stream_fn),
        AgentState {
            tools: vec![settled_tool, slow_tool],
            ..Default::default()
        },
    );
    let agent = agent_from(options);
    let events_for_listener = Arc::clone(&events);
    let ended_tx = Arc::new(Mutex::new(Some(settled_ended_tx)));
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let events = Arc::clone(&events_for_listener);
        let ended_tx = Arc::clone(&ended_tx);
        Box::pin(async move {
            if event_kind(&event) == "tool_execution_end" {
                if let AgentEvent::ToolExecutionEnd { tool_call_id, .. } = &event {
                    if tool_call_id == "call-1" {
                        if let Some(tx) = ended_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                    }
                }
            }
            events.lock().unwrap().push(event);
        }) as pillar_agent::ListenerFuture
    });

    let agent_for_prompt = Arc::clone(&agent);
    let prompt_task = tokio::spawn(async move {
        agent_for_prompt.prompt("run tools").await.expect("prompt");
    });

    tokio::join!(async {
        let () = slow_started_rx.await.expect("slow started");
        let () = settled_ended_rx.await.expect("settled ended");
    });
    let event_count_before_late_update = events.lock().unwrap().len();

    // The settled tool's late update must not surface while the slow tool
    // still runs (upstream drops it: acceptingUpdates was already false).
    let late_update = settled_tool_update.lock().unwrap().take();
    if let Some(on_update) = late_update {
        on_update(AgentToolResult {
            content: vec![Content::text("late")],
            details: serde_json::json!({"status": "late"}),
            ..Default::default()
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(events.lock().unwrap().len(), event_count_before_late_update);

    let _ = release_slow_tx.send(());
    prompt_task.await.unwrap();
    let updates = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event_kind(event) == "tool_execution_update")
        .count();
    assert_eq!(updates, 0);
}

#[tokio::test]
async fn should_update_state_with_mutators() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));

    agent.set_system_prompt("Custom prompt");
    assert_eq!(agent.state().system_prompt, "Custom prompt");

    agent.set_model(mock_model());
    assert_eq!(agent.state().model, mock_model());

    agent.set_thinking_level(AgentThinkingLevel::High);
    assert_eq!(agent.state().thinking_level, AgentThinkingLevel::High);

    let tools = vec![noop_tool()];
    agent.set_tools(tools);
    assert_eq!(agent.state().tools.len(), 1);

    agent.set_messages(vec![create_user_message("Hello")]);
    assert_eq!(agent.state().messages.len(), 1);

    agent.append_message(create_user_message("second"));
    assert_eq!(agent.state().messages.len(), 2);

    agent.clear_messages();
    assert!(agent.state().messages.is_empty());
}

#[tokio::test]
async fn should_support_steering_message_queue() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));

    let message = create_user_message("Steering message");
    agent.steer(message.clone());

    assert!(!agent.state().messages.contains(&message));
}

#[tokio::test]
async fn should_support_follow_up_message_queue() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));

    let message = create_user_message("Follow-up message");
    agent.follow_up(message.clone());

    assert!(!agent.state().messages.contains(&message));
}

#[tokio::test]
async fn should_handle_abort_controller() {
    let agent = agent_from(AgentOptions::new(unused_stream_fn()));

    // Should not throw even if nothing is running.
    agent.abort();
}

#[tokio::test]
async fn should_reject_reset_while_processing_without_corrupting_the_transcript() {
    let (release_tx, _release_rx_unused) = tokio::sync::oneshot::channel::<()>();
    let release_for_fn: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(Some(release_tx)));
    let release_for_stream = Arc::clone(&release_for_fn);

    let stream_fn = pillar_agent::StreamFn::new(move |_context, _options| {
        let release = Arc::clone(&release_for_stream);
        Box::pin(async move {
            let stream = assistant_message_event_stream();
            stream.push(pillar_ai::types::AssistantMessageEvent::Start {
                partial: create_assistant_message(""),
            });
            // Hold the stream open until the test releases it.
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            *release.lock().unwrap() = Some(tx);
            let _ = rx.await;
            stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: create_assistant_message("Done"),
            });
            stream
        })
    });
    let agent = agent_from(AgentOptions::new(stream_fn));

    let agent_for_prompt = Arc::clone(&agent);
    let prompt_task = tokio::spawn(async move {
        agent_for_prompt.prompt("Hello").await.expect("prompt");
    });
    let () = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        loop {
            if agent.state().is_streaming {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("stream started");

    // Upstream throws; the Rust port returns Err and must not corrupt state.
    let error = agent.reset().expect_err("reset must fail while streaming");
    assert_eq!(
        error.to_string(),
        "Agent is already processing. Wait for completion before resetting."
    );
    assert!(agent.state().is_streaming);

    // Release the held stream; the prompt finishes normally.
    let release = release_for_fn.lock().unwrap().take();
    if let Some(tx) = release {
        let _ = tx.send(());
    }
    prompt_task.await.unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[1].role_name(), "assistant");
}

#[tokio::test]
async fn should_throw_when_prompt_called_while_streaming() {
    let agent = agent_from(AgentOptions::new(abort_aware_stream_fn()));

    let agent_for_prompt = Arc::clone(&agent);
    let first_prompt = tokio::spawn(async move {
        // Blocks until abort; the abort error path resolves the run normally.
        let _ = agent_for_prompt.prompt("First message").await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(agent.state().is_streaming);

    let error = agent
        .prompt("Second message")
        .await
        .expect_err("second prompt must be rejected");
    assert_eq!(
        error.to_string(),
        "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
    );

    agent.abort();
    first_prompt.await.unwrap();
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn should_throw_when_continue_called_while_streaming() {
    let agent = agent_from(AgentOptions::new(abort_aware_stream_fn()));

    let agent_for_prompt = Arc::clone(&agent);
    let first_prompt = tokio::spawn(async move {
        let _ = agent_for_prompt.prompt("First message").await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(agent.state().is_streaming);

    let error = agent
        .continue_run()
        .await
        .expect_err("continue must be rejected");
    assert_eq!(
        error.to_string(),
        "Agent is already processing. Wait for completion before continuing."
    );

    agent.abort();
    first_prompt.await.unwrap();
}

#[tokio::test]
async fn continue_should_process_queued_follow_up_messages_after_an_assistant_turn() {
    let agent = agent_from(AgentOptions::new(done_stream_fn("Processed")));

    agent.set_messages(vec![
        create_user_message("Initial"),
        create_assistant_message("Initial response").into(),
    ]);
    agent.follow_up(create_user_message("Queued follow-up"));

    agent.continue_run().await.expect("continue");

    let messages = agent.state().messages;
    let has_queued_follow_up = messages
        .iter()
        .any(|message| match message.as_base_message() {
            Message::User {
                content: pillar_ai::types::UserContent::Text(text),
                ..
            } => text == "Queued follow-up",
            _ => false,
        });
    assert!(has_queued_follow_up);
    assert_eq!(messages.last().map(|m| m.role_name()), Some("assistant"));
}

#[tokio::test]
async fn continue_should_keep_one_at_a_time_steering_semantics_from_assistant_tail() {
    let responses: Arc<Mutex<Vec<pillar_ai::AssistantMessage>>> = Arc::new(Mutex::new(Vec::new()));
    // Two scripted responses delivered in order across two runs.
    let scripted = Arc::new(Mutex::new(vec![
        create_assistant_message("Processed 1"),
        create_assistant_message("Processed 2"),
    ]));
    let calls = Arc::new(AtomicU32::new(0));
    let stream_fn = {
        let scripted = Arc::clone(&scripted);
        let calls = Arc::clone(&calls);
        pillar_agent::StreamFn::new(move |_context, _options| {
            let scripted = Arc::clone(&scripted);
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let message = scripted.lock().unwrap().remove(0);
                let stream = assistant_message_event_stream();
                stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message,
                });
                stream
            }
        })
    };
    let agent = agent_from(AgentOptions::new(stream_fn));

    agent.set_messages(vec![
        create_user_message("Initial"),
        create_assistant_message("Initial response").into(),
    ]);
    agent.steer(create_user_message("Steering 1"));
    agent.steer(create_user_message("Steering 2"));

    agent.continue_run().await.expect("continue");

    let messages = agent.state().messages;
    let recent: Vec<&'static str> = messages
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|m| m.role_name())
        .collect();
    assert_eq!(recent, vec!["user", "assistant", "user", "assistant"]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let _ = responses;
}

#[tokio::test]
async fn keeps_legacy_prepare_next_turn_signal_callback_behavior() {
    let mut tool = noop_tool();
    tool.tool.name = "noop".into();
    let request_count = Arc::new(AtomicU32::new(0));
    let saw_abort_signal = Arc::new(AtomicBool::new(false));

    let count_for_fn = Arc::clone(&request_count);
    let saw_for_fn = Arc::clone(&saw_abort_signal);
    let options = with_prepare_next_turn(
        with_initial_state(
            AgentOptions::new(scripted_tool_then_done_stream_fn(Arc::clone(&count_for_fn))),
            AgentState {
                tools: vec![tool],
                ..Default::default()
            },
        ),
        move |_context, signal| {
            let saw = Arc::clone(&saw_for_fn);
            Box::pin(async move {
                saw.store(
                    signal.as_ref().map(|s| !s.is_aborted()).unwrap_or(false),
                    Ordering::SeqCst,
                );
                None
            }) as PrepareNextFutureAlias
        },
    );
    let agent = agent_from(options);

    agent.prompt("start").await.expect("prompt");

    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    assert!(saw_abort_signal.load(Ordering::SeqCst));
}

#[tokio::test]
async fn forwards_should_stop_after_turn_through_agent_options() {
    let mut tool = noop_tool();
    tool.tool.name = "noop".into();
    let request_count = Arc::new(AtomicU32::new(0));
    let saw_abort_signal = Arc::new(AtomicBool::new(false));
    let callback_context_roles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let count_for_fn = Arc::clone(&request_count);
    let saw_for_fn = Arc::clone(&saw_abort_signal);
    let roles_for_hook = Arc::clone(&callback_context_roles);
    let options = with_should_stop_after_turn(
        with_initial_state(
            AgentOptions::new(scripted_tool_then_done_stream_fn(Arc::clone(&count_for_fn))),
            AgentState {
                tools: vec![tool],
                ..Default::default()
            },
        ),
        move |context, signal| {
            let saw = Arc::clone(&saw_for_fn);
            let roles = Arc::clone(&roles_for_hook);
            let roles_snapshot: Vec<String> = context
                .context
                .messages
                .iter()
                .map(|m| m.role_name().to_owned())
                .collect();
            Box::pin(async move {
                saw.store(signal.is_some(), Ordering::SeqCst);
                *roles.lock().unwrap() = roles_snapshot;
                true
            }) as StopFutureAlias
        },
    );
    let agent = agent_from(options);

    agent.prompt("start").await.expect("prompt");

    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    assert!(saw_abort_signal.load(Ordering::SeqCst));
    assert_eq!(
        callback_context_roles.lock().unwrap().clone(),
        vec!["user", "assistant", "toolResult"]
    );
}

#[tokio::test]
async fn forwards_session_id_to_stream_function_options() {
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let received_for_fn = Arc::clone(&received);
    let session_holder = Arc::new(Mutex::new("session-abc".to_owned()));
    let session_for_fn = Arc::clone(&session_holder);

    let stream_fn = pillar_agent::StreamFn::new(move |_context, options| {
        let received = Arc::clone(&received_for_fn);
        let session = Arc::clone(&session_for_fn);
        async move {
            if let Some(session_id) = options.and_then(|options| options.simple.session_id.clone())
            {
                received.lock().unwrap().push(session_id);
            }
            let _ = session;
            let stream = assistant_message_event_stream();
            stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: create_assistant_message("ok"),
            });
            stream
        }
    });
    let agent = Arc::new(Agent::new(with_session_id(
        AgentOptions::new(stream_fn),
        "session-abc",
    )));

    agent.prompt("hello").await.expect("prompt");
    assert_eq!(received.lock().unwrap().as_slice(), ["session-abc"]);

    // Setter parity: upstream `agent.sessionId = "session-def"`.
    agent.set_session_id("session-def");
    assert_eq!(agent.session_id().as_deref(), Some("session-def"));

    agent.prompt("hello again").await.expect("prompt");
    assert_eq!(
        received.lock().unwrap().as_slice(),
        ["session-abc", "session-def"]
    );
}

// --- Helpers -------------------------------------------------------------

fn event_kind(event: &AgentEvent) -> &'static str {
    event.kind()
}

fn unused_stream_fn() -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(
        |_context, _options| async move { panic!("Unexpected stream call") },
    )
}

fn tool_call_stream_fn(_label: &str, tool_calls: Vec<Content>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| {
        let tool_calls = tool_calls.clone();
        async move {
            let stream = assistant_message_event_stream();
            stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                reason: StopReason::ToolUse,
                message: create_assistant_tool_use_message(tool_calls),
            });
            stream
        }
    })
}

/// First call: toolUse with one tool call; later calls: plain stop message.
fn scripted_tool_then_done_stream_fn(calls: Arc<AtomicU32>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| {
        let calls = Arc::clone(&calls);
        async move {
            let stream = assistant_message_event_stream();
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let message = if index == 0 {
                create_assistant_tool_use_message(vec![Content::tool_call(
                    "tool-1",
                    "noop",
                    serde_json::json!({}),
                )])
            } else {
                create_assistant_message("done")
            };
            stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        }
    })
}

/// Stream fn that polls for abort and then terminates with an aborted error
/// event, mirroring the upstream mock's periodic `checkAbort` timer.
fn abort_aware_stream_fn() -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, options| async move {
        let signal = options.and_then(|options| options.abort.clone());
        let stream = assistant_message_event_stream();
        stream.push(pillar_ai::types::AssistantMessageEvent::Start {
            partial: create_assistant_message(""),
        });
        let watcher = stream.clone_stream();
        tokio::spawn(async move {
            loop {
                if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
                    watcher.push(pillar_ai::types::AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: create_assistant_message("Aborted"),
                    });
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        stream
    })
}

/// Upstream "streamFn throws" parity: the stream fn future itself panics.
/// The panic is caught by the agent-loop task and surfaces as an error
/// assistant message; the failure message text is matched below.
fn panic_with_event_stream(_message: &'static str) -> pillar_ai::AssistantMessageEventStream {
    panic!("provider exploded")
}

// --- AgentOptions builder shims (upstream object literal parity) ----------

fn options_with(stream_fn: Option<pillar_agent::StreamFn>) -> AgentOptions {
    match stream_fn {
        Some(stream_fn) => AgentOptions::new(stream_fn),
        None => {
            // Placeholder: replaced below when the default stream fn is set.
            let placeholder = unused_stream_fn();
            let mut options = AgentOptions::new(placeholder);
            options.stream_fn = None;
            options
        }
    }
}

fn with_initial_state(mut options: AgentOptions, initial_state: AgentState) -> AgentOptions {
    options.initial_state = Some(initial_state);
    options
}

fn with_prepare_next_turn<F>(mut options: AgentOptions, f: F) -> AgentOptions
where
    F: Fn(
            &pillar_agent::ShouldStopAfterTurnContext,
            Option<pillar_agent::AbortSignal>,
        ) -> PrepareNextFutureAlias
        + Send
        + Sync
        + 'static,
{
    options.prepare_next_turn = Some(Arc::new(f));
    options
}

fn with_should_stop_after_turn<F>(mut options: AgentOptions, f: F) -> AgentOptions
where
    F: Fn(
            &pillar_agent::ShouldStopAfterTurnContext,
            Option<pillar_agent::AbortSignal>,
        ) -> StopFutureAlias
        + Send
        + Sync
        + 'static,
{
    options.should_stop_after_turn = Some(Arc::new(f));
    options
}

fn with_session_id(mut options: AgentOptions, session_id: &str) -> AgentOptions {
    options.session_id = Some(session_id.to_owned());
    options
}

#[allow(unused)]
fn _unused_helpers() {
    let _ = options_with;
}

/// Unused in some configs; keeps ToolExecuteError import stable.
#[allow(dead_code)]
fn _witness_error(_error: ToolExecuteError) {}

/// QueueMode import witness (steering defaults verified via continue tests).
#[allow(dead_code)]
fn _witness_queue_mode(_mode: QueueMode) {}
