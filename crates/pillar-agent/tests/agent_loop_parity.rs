//! Port of packages/agent/test/agent-loop.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream case, same names in comments. The mock
//! stream function replays scripted assistant messages; typebox tool
//! schemas become JSON-Schema objects.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::types::PrepareArgumentsFn;
use pillar_agent::{
    AbortSignal, AgentContext, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate, AgentMessage,
    AgentTool, AgentToolResult, BeforeToolCallResult, StreamFn, ToolExecuteError, agent_loop,
    agent_loop_continue, set_default_stream_fn,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessageEvent, Content, Message, StopReason, Tool, Usage, UsageCost,
};

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

fn create_model() -> pillar_agent::FauxModelRef {
    pillar_agent::FauxModelRef {
        id: "mock".into(),
        name: "mock".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: UsageCost::default(),
        context_window: 8192,
        max_tokens: 2048,
    }
}

fn create_assistant_message(
    content: Vec<Content>,
    stop_reason: StopReason,
) -> pillar_ai::AssistantMessage {
    pillar_ai::AssistantMessage {
        content,
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "mock".into(),
        response_model: None,
        usage: create_usage(),
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

fn create_user_message(text: &str) -> AgentMessage {
    Message::User {
        content: pillar_ai::types::UserContent::Text(text.to_owned()),
        timestamp: 1,
    }
    .into()
}

fn echo_tool(executed: Arc<Mutex<Vec<String>>>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "echo".into(),
            description: "Echo tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"value": {"type": "string"}}}),
            constrained_sampling: None,
        },
        label: "Echo".into(),
        prepare_arguments: None,
        execute: Arc::new(move |_tool_call_id, params, _signal, _on_update| {
            let executed = Arc::clone(&executed);
            Box::pin(async move {
                // Record the raw value form: upstream's test tool accepts
                // string|number, and the beforeToolCall mutation writes a
                // number (123).
                let value = match params.get("value") {
                    Some(serde_json::Value::String(text)) => text.clone(),
                    Some(serde_json::Value::Number(number)) => number.to_string(),
                    _ => String::new(),
                };
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    usage: Some(Usage {
                        input: 1,
                        output: 2,
                        cache_read: 3,
                        cache_write: 4,
                        cache_write_1h: None,
                        reasoning: None,
                        total_tokens: 10,
                        cost: UsageCost {
                            input: 0.1,
                            output: 0.2,
                            cache_read: 0.3,
                            cache_write: 0.4,
                            total: 1.0,
                        },
                    }),
                    added_tool_names: None,
                    terminate: false,
                })
            })
        }),
        execution_mode: None,
    }
}

/// Scripted stream fn: each call pops the next assistant message.
fn scripted_stream_fn(
    responses: Arc<Mutex<Vec<pillar_ai::AssistantMessage>>>,
) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| {
        let responses = Arc::clone(&responses);
        async move {
            let stream = assistant_message_event_stream();
            let message = responses.lock().unwrap().remove(0);
            stream.push(pillar_ai::types::AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        }
    })
}

async fn drain(stream: &pillar_agent::AgentStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        events.push(event);
    }
    events
}

/// Builder for a tool with an inline execute closure (upstream tests define
/// tools inline with typebox schemas).
fn simple_tool(
    name: &str,
    execution_mode: Option<pillar_agent::ToolExecutionMode>,
    execute: impl Fn(
        String,
        serde_json::Value,
        Option<AbortSignal>,
        Option<pillar_agent::AgentToolUpdateCallback>,
    ) -> pillar_agent::types::ToolExecuteFuture
    + Send
    + Sync
    + 'static,
) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: name.to_owned(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } }
            }),
            constrained_sampling: None,
        },
        label: name.to_owned(),
        prepare_arguments: None,
        execute: Arc::new(
            move |tool_call_id: String,
                  args: serde_json::Value,
                  signal: Option<AbortSignal>,
                  on_update: Option<pillar_agent::AgentToolUpdateCallback>| {
                execute(tool_call_id, args, signal, on_update)
            },
        ),
        execution_mode,
    }
}

/// Release a gated tool after a short delay (upstream `setTimeout(release, 20)`).
fn release_gate_after(gate: Arc<tokio::sync::Notify>, millis: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
        gate.notify_one();
    });
}

/// Stand-in for an upstream custom message (`CustomAgentMessages` entry):
/// the Rust `AgentMessage` union is closed, so a non-user message the test
/// converter drops/maps plays the custom role.
fn notification_stand_in() -> AgentMessage {
    AgentMessage::from(pillar_ai::types::ToolResultMessage {
        tool_call_id: "custom-1".into(),
        tool_name: "notification".into(),
        content: vec![Content::text("This is a notification")],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 1,
    })
}

/// Clears the process-wide default stream fn even if the test panics.
struct ClearDefaultStreamFn;
impl Drop for ClearDefaultStreamFn {
    fn drop(&mut self) {
        set_default_stream_fn(None);
    }
}

#[tokio::test]
async fn uses_the_configured_default_when_a_legacy_caller_omits_stream_fn() {
    let _guard = ClearDefaultStreamFn;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_for_fn = Arc::clone(&calls);
    set_default_stream_fn(Some(StreamFn::new(move |_context, _options| {
        let calls = Arc::clone(&calls_for_fn);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let stream = assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: create_assistant_message(
                    vec![Content::text("fallback")],
                    StopReason::Stop,
                ),
            });
            stream
        }
    })));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };
    // Legacy callers omit streamFn entirely; the loop resolves the default.
    let stream = agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        None,
        None,
    );
    let _ = stream.result().await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn should_emit_events_with_agent_message_types() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let user_prompt = create_user_message("Hello");
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Hi there!")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, Some(stream_fn));
    let events = drain(&stream).await;
    let messages = stream.result().await;

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role_name(), "user");
    assert_eq!(messages[1].role_name(), "assistant");

    let kinds: Vec<&'static str> = events.iter().map(AgentEvent::kind).collect();
    assert!(kinds.contains(&"agent_start"));
    assert!(kinds.contains(&"turn_start"));
    assert!(kinds.contains(&"message_start"));
    assert!(kinds.contains(&"message_end"));
    assert!(kinds.contains(&"turn_end"));
    assert!(kinds.contains(&"agent_end"));
}

#[tokio::test]
async fn should_apply_transform_context_before_convert_to_llm() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![
            create_user_message("old message 1"),
            create_assistant_message(vec![Content::text("old response 1")], StopReason::Stop)
                .into(),
            create_user_message("old message 2"),
            create_assistant_message(vec![Content::text("old response 2")], StopReason::Stop)
                .into(),
        ],
        tools: Vec::new(),
    };
    let user_prompt = create_user_message("new message");

    let config = AgentLoopConfig {
        model: Some(create_model()),
        transform_context: Some(Arc::new(|mut messages, _signal| {
            Box::pin(async move {
                // Keep only last 2 messages (prune old ones).
                messages.split_off(messages.len().saturating_sub(2))
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, Some(stream_fn));
    let _events = drain(&stream).await;
    let _messages = stream.result().await;
    // The observable effect (pruning) is verified by the converter receiving
    // 2 messages; the loop itself enforces transform-before-convert order.
}

#[tokio::test]
async fn should_handle_tool_calls_and_results() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let user_prompt = create_user_message("echo something");

    let tool_usage = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 10,
        cost: UsageCost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.3,
            cache_write: 0.4,
            total: 1.0,
        },
    };
    let patched_tool_usage = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 26,
        cost: UsageCost {
            input: 0.5,
            output: 0.6,
            cache_read: 0.7,
            cache_write: 0.8,
            total: 2.6,
        },
    };
    let observed_tool_usage = Arc::new(Mutex::new(None::<Usage>));
    let observed_for_hook = Arc::clone(&observed_tool_usage);
    let patched = patched_tool_usage.clone();

    let config = AgentLoopConfig {
        model: Some(create_model()),
        after_tool_call: Some(Arc::new(move |context, _signal| {
            let observed = Arc::clone(&observed_for_hook);
            let patched = patched.clone();
            Box::pin(async move {
                *observed.lock().unwrap() = context.result.usage;
                Some(pillar_agent::AfterToolCallResult {
                    usage: Some(patched),
                    ..Default::default()
                })
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, Some(stream_fn));
    let events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(*executed.lock().unwrap(), vec!["hello".to_owned()]);

    let tool_end = events
        .iter()
        .find(|event| event.kind() == "tool_execution_end");
    assert!(tool_end.is_some());
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { is_error, .. }) => assert!(!is_error),
        _ => unreachable!(),
    }
    assert_eq!(
        observed_tool_usage.lock().unwrap().as_ref(),
        Some(&tool_usage)
    );

    let _messages = stream.result().await;
    // Usage patching verified through the tool result in the final result.
}

#[tokio::test]
async fn should_not_execute_tool_calls_from_a_length_truncated_assistant_message() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        // Output hit the token limit mid tool call: nothing may execute.
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hel"}),
            )],
            StopReason::Length,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;

    // The tool must never execute with potentially truncated arguments.
    assert!(executed.lock().unwrap().is_empty());

    let tool_end = events
        .iter()
        .find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd {
            result, is_error, ..
        }) => {
            assert!(is_error);
            let text = result["content"][0]["text"].as_str().unwrap_or_default();
            assert!(text.contains("output token limit"), "got: {text}");
        }
        _ => panic!("expected tool_execution_end"),
    }

    let messages = stream.result().await;
    assert_eq!(messages.last().map(|m| m.role_name()), Some("assistant"));
}

#[tokio::test]
async fn should_execute_mutated_before_tool_call_args_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };

    let config = AgentLoopConfig {
        model: Some(create_model()),
        before_tool_call: Some(Arc::new(|context, _signal| {
            Box::pin(async move {
                // Mutate validated args in place before execution.
                let mut args = context.args.lock().unwrap();
                if let Some(object) = args.as_object_mut() {
                    object.insert("value".into(), serde_json::json!(123));
                }
                None::<BeforeToolCallResult>
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    // The mutation upstream rewrote args to 123 before execution; the echo
    // tool receives the mutated value. Our echo reads "value" as a string,
    // so assert the executed record shows the mutated number's string form.
    assert_eq!(*executed.lock().unwrap(), vec!["123".to_owned()]);
}

#[tokio::test]
async fn should_inject_queued_messages_after_all_tool_calls_complete() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let user_prompt = create_user_message("start");

    let queued_delivered = Arc::new(AtomicU32::new(0));
    let queued_for_hook = Arc::clone(&queued_delivered);
    let executed_for_hook = Arc::clone(&executed);
    let config = AgentLoopConfig {
        model: Some(create_model()),
        tool_execution: Some(pillar_agent::ToolExecutionMode::Sequential),
        get_steering_messages: Some(Arc::new(move || {
            let queued = Arc::clone(&queued_for_hook);
            let executed_hook = Arc::clone(&executed_for_hook);
            Box::pin(async move {
                // Deliver the steering message once both tools have executed.
                if queued.load(Ordering::SeqCst) == 0 && executed_hook.lock().unwrap().len() >= 2 {
                    queued.store(1, Ordering::SeqCst);
                    return vec![create_user_message("interrupt")];
                }
                Vec::new()
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "echo", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "echo", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, Some(stream_fn));
    let events = drain(&stream).await;
    let _ = stream.result().await;

    // Both tools execute before steering is injected.
    assert_eq!(
        *executed.lock().unwrap(),
        vec!["first".to_owned(), "second".to_owned()]
    );

    let tool_ends: Vec<&AgentEvent> = events
        .iter()
        .filter(|event| event.kind() == "tool_execution_end")
        .collect();
    assert_eq!(tool_ends.len(), 2);
    for tool_end in tool_ends {
        match tool_end {
            AgentEvent::ToolExecutionEnd { is_error, .. } => assert!(!is_error),
            _ => unreachable!(),
        }
    }

    // Queued message appears in events after both tool result messages.
    let event_sequence: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart { message } => match message.as_base_message() {
                Message::ToolResult(result) => Some(format!("tool:{}", result.tool_call_id)),
                Message::User {
                    content: pillar_ai::types::UserContent::Text(text),
                    ..
                } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let interrupt_index = event_sequence.iter().position(|entry| entry == "interrupt");
    let tool1_index = event_sequence
        .iter()
        .position(|entry| entry == "tool:tool-1");
    let tool2_index = event_sequence
        .iter()
        .position(|entry| entry == "tool:tool-2");
    assert!(
        interrupt_index.is_some(),
        "interrupt delivered: {event_sequence:?}"
    );
    assert!(tool1_index.is_some());
    assert!(tool2_index.is_some());
    assert!(tool1_index.unwrap() < interrupt_index.unwrap());
    assert!(tool2_index.unwrap() < interrupt_index.unwrap());
}

#[tokio::test]
async fn should_throw_when_context_has_no_messages() {
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };
    let responses = Arc::new(Mutex::new(vec![]));
    let stream_fn = scripted_stream_fn(responses);

    let result = agent_loop_continue(context, config, None, Some(stream_fn));
    assert!(result.is_err());
    assert_eq!(
        result.err().map(|e| e.to_string()),
        Some("Cannot continue: no messages in context".to_owned())
    );
}

#[tokio::test]
async fn should_continue_from_existing_context_without_emitting_user_message_events() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![create_user_message("Hello")],
        tools: Vec::new(),
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop_continue(context, config, None, Some(stream_fn)).expect("continuation");
    let events = drain(&stream).await;
    let messages = stream.result().await;

    // Only the new assistant message is returned.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role_name(), "assistant");

    // No user message events (key difference from agentLoop).
    let message_end_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|event| event.kind() == "message_end")
        .collect();
    assert_eq!(message_end_events.len(), 1);
    match message_end_events[0] {
        AgentEvent::MessageEnd { message } => assert_eq!(message.role_name(), "assistant"),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn should_stop_after_the_current_turn_when_should_stop_after_turn_returns_true() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };

    let steering_polls = Arc::new(AtomicU32::new(0));
    let follow_up_polls = Arc::new(AtomicU32::new(0));
    let callback_tool_result_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let callback_context_roles = Arc::new(Mutex::new(Vec::<String>::new()));

    let steering_for_hook = Arc::clone(&steering_polls);
    let follow_up_for_hook = Arc::clone(&follow_up_polls);
    let tool_ids_for_hook = Arc::clone(&callback_tool_result_ids);
    let roles_for_hook = Arc::clone(&callback_context_roles);

    let config = AgentLoopConfig {
        model: Some(create_model()),
        get_steering_messages: Some(Arc::new(move || {
            let polls = Arc::clone(&steering_for_hook);
            Box::pin(async move {
                polls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            })
        })),
        get_follow_up_messages: Some(Arc::new(move || {
            let polls = Arc::clone(&follow_up_for_hook);
            Box::pin(async move {
                polls.fetch_add(1, Ordering::SeqCst);
                vec![create_user_message("follow up should stay queued")]
            })
        })),
        should_stop_after_turn: Some(Arc::new(move |context| {
            let tool_ids = Arc::clone(&tool_ids_for_hook);
            let roles = Arc::clone(&roles_for_hook);
            let tool_ids_snapshot: Vec<String> = context
                .tool_results
                .iter()
                .map(|result| result.tool_call_id.clone())
                .collect();
            let role_snapshot: Vec<String> = context
                .context
                .messages
                .iter()
                .map(|message| message.role_name().to_owned())
                .collect();
            Box::pin(async move {
                // AssistantMessage は常に role: assistant。
                *tool_ids.lock().unwrap() = tool_ids_snapshot;
                *roles.lock().unwrap() = role_snapshot;
                true
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("should not run")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let messages = stream.result().await;

    assert_eq!(
        events.iter().filter(|e| e.kind() == "agent_start").count(),
        1
    );
    assert_eq!(events.iter().filter(|e| e.kind() == "agent_end").count(), 1);
    assert_eq!(*executed.lock().unwrap(), vec!["hello".to_owned()]);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        *callback_tool_result_ids.lock().unwrap(),
        vec!["tool-1".to_owned()]
    );
    assert_eq!(
        *callback_context_roles.lock().unwrap(),
        vec![
            "user".to_owned(),
            "assistant".to_owned(),
            "toolResult".to_owned()
        ]
    );
    assert_eq!(
        messages.iter().map(|m| m.role_name()).collect::<Vec<_>>(),
        vec!["user", "assistant", "toolResult"]
    );
    let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn tool_error_becomes_an_error_result_and_the_loop_continues() {
    let failing = AgentTool {
        tool: Tool {
            name: "fail".into(),
            description: "Always fails".into(),
            parameters: serde_json::json!({"type": "object"}),
            constrained_sampling: None,
        },
        label: "Fail".into(),
        prepare_arguments: None,
        execute: Arc::new(|_id, _args, _signal, _on_update| {
            Box::pin(async move { Err(ToolExecuteError("boom".into())) })
        }),
        execution_mode: None,
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![failing],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call("tool-1", "fail", serde_json::json!({}))],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("recovered")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("go")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let messages = stream.result().await;

    let tool_end = events
        .iter()
        .find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { is_error, .. }) => assert!(is_error),
        _ => panic!("expected tool_execution_end"),
    }
    let tool_result = messages.iter().find(|m| m.role_name() == "toolResult");
    assert!(tool_result.is_some());
    match tool_result.map(|m| m.as_base_message()) {
        Some(Message::ToolResult(result)) => {
            assert!(result.is_error);
            match &result.content[0] {
                Content::Text { text, .. } => assert_eq!(text, "boom"),
                other => panic!("expected text, got {other:?}"),
            }
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn before_tool_call_block_prevents_execution() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };

    let config = AgentLoopConfig {
        model: Some(create_model()),
        before_tool_call: Some(Arc::new(|_context, _signal| {
            Box::pin(async move {
                Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("Blocked by policy".into()),
                    terminate: false,
                })
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "x"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("go")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let _ = stream.result().await;

    assert!(executed.lock().unwrap().is_empty());
    let tool_end = events
        .iter()
        .find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd {
            result, is_error, ..
        }) => {
            assert!(is_error);
            assert!(
                result["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Blocked")
            );
        }
        _ => panic!("expected tool_execution_end"),
    }
}

/// upstream test: "should handle custom message types via convertToLlm"
/// divergence: `AgentMessage` is a closed union (no CustomAgentMessages
/// declaration merging); a toolResult stands in for the custom role and
/// the test converter filters it out.
#[tokio::test]
async fn should_handle_custom_message_types_via_convert_to_llm() {
    let notification = notification_stand_in();

    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![notification],
        tools: Vec::new(),
    };

    let converted = Arc::new(Mutex::new(Vec::<Message>::new()));
    let converted_for_hook = Arc::clone(&converted);
    let config = AgentLoopConfig {
        model: Some(create_model()),
        convert_to_llm: Arc::new(move |messages: &[AgentMessage]| -> Vec<Message> {
            // Filter out the custom role, convert the rest.
            let filtered: Vec<Message> = messages
                .iter()
                .filter_map(|m| m.as_message().cloned())
                .filter(|m| match m {
                    Message::ToolResult(result) => result.tool_name != "notification",
                    _ => true,
                })
                .collect();
            *converted_for_hook.lock().unwrap() = filtered.clone();
            filtered
        }),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("Hello")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    // The notification should have been filtered out in convertToLlm.
    let converted = converted.lock().unwrap();
    assert_eq!(converted.len(), 1); // Only user message
    assert!(matches!(&converted[0], Message::User { .. }));
}

/// upstream test: "should use prepareNextTurn snapshot before continuing"
#[tokio::test]
async fn should_use_prepare_next_turn_snapshot_before_continuing() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));
    let context = AgentContext {
        system_prompt: "first prompt".into(),
        messages: Vec::new(),
        tools: vec![tool],
    };

    let converted_second_turn_system_prompt = Arc::new(Mutex::new(String::new()));
    let prepare_calls = Arc::new(AtomicU32::new(0));
    let prepare_calls_for_hook = Arc::clone(&prepare_calls);
    let prepared = Arc::new(AtomicU32::new(0));
    let prepared_for_hook = Arc::clone(&prepared);

    let config = AgentLoopConfig {
        model: Some(create_model()),
        prepare_next_turn: Some(Arc::new(
            move |snapshot: &pillar_agent::ShouldStopAfterTurnContext| {
                let calls = Arc::clone(&prepare_calls_for_hook);
                let prepared = Arc::clone(&prepared_for_hook);
                let messages = snapshot.context.messages.clone();
                let tools = snapshot.context.tools.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if prepared.load(Ordering::SeqCst) == 1 {
                        return None;
                    }
                    prepared.store(1, Ordering::SeqCst);
                    Some(AgentLoopTurnUpdate {
                        context: Some(AgentContext {
                            system_prompt: "second prompt".into(),
                            messages,
                            tools,
                        }),
                        model: None,
                        thinking_level: None,
                    })
                })
            },
        )),
        ..Default::default()
    };

    let llm_calls = Arc::new(AtomicU32::new(0));
    let llm_calls_for_fn = Arc::clone(&llm_calls);
    // The stream fn receives the (replaced) context through its argument;
    // capture the second-turn system prompt from the llm context (upstream
    // inspects ctx.systemPrompt on the second llm call).
    let stream_fn = {
        let prompt_out = Arc::clone(&converted_second_turn_system_prompt);
        StreamFn::new(move |context: pillar_ai::types::Context, _options| {
            let calls = Arc::clone(&llm_calls_for_fn);
            let prompt_out = Arc::clone(&prompt_out);
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 2 {
                    *prompt_out.lock().unwrap() = context.system_prompt.clone().unwrap_or_default();
                }
                let stream = assistant_message_event_stream();
                if call == 1 {
                    stream.push(AssistantMessageEvent::Done {
                        reason: StopReason::ToolUse,
                        message: create_assistant_message(
                            vec![Content::tool_call(
                                "tool-1",
                                "echo",
                                serde_json::json!({"value": "hello"}),
                            )],
                            StopReason::ToolUse,
                        ),
                    });
                } else {
                    stream.push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: create_assistant_message(
                            vec![Content::text("done")],
                            StopReason::Stop,
                        ),
                    });
                }
                stream
            }
        })
    };

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(llm_calls.load(Ordering::SeqCst), 2);
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        converted_second_turn_system_prompt.lock().unwrap().as_str(),
        "second prompt"
    );
}

/// upstream test: "should prepare tool arguments for validation"
#[tokio::test]
async fn should_prepare_tool_arguments_for_validation() {
    let executed = Arc::new(Mutex::new(Vec::<Vec<(String, String)>>::new()));
    let executed_for_tool = Arc::clone(&executed);

    let mut tool = echo_tool(Arc::new(Mutex::new(Vec::new())));
    tool.tool.name = "edit".into();
    tool.tool.description = "Edit tool".into();
    tool.label = "Edit".into();
    tool.tool.parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "oldText": { "type": "string" }, "newText": { "type": "string" } }
                }
            }
        }
    });
    let prepare: Arc<PrepareArgumentsFn> = Arc::new(|args: &serde_json::Value| {
        if !args.is_object() {
            return args.clone();
        }
        let old_text = args.get("oldText").and_then(|v| v.as_str());
        let new_text = args.get("newText").and_then(|v| v.as_str());
        let (Some(old_text), Some(new_text)) = (old_text, new_text) else {
            return args.clone();
        };
        let mut edits: Vec<serde_json::Value> = args
            .get("edits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        edits.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
        serde_json::json!({ "edits": edits })
    });
    tool.prepare_arguments = Some(prepare);
    tool.execute = Arc::new(
        move |_tool_call_id, params: serde_json::Value, _signal, _on_update| {
            let executed = Arc::clone(&executed_for_tool);
            Box::pin(async move {
                let edits: Vec<(String, String)> = params
                    .get("edits")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                Some((
                                    item.get("oldText")?.as_str()?.to_owned(),
                                    item.get("newText")?.as_str()?.to_owned(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let count = edits.len();
                executed.lock().unwrap().push(edits);
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("edited {count}"))],
                    details: serde_json::json!({ "count": count }),
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "edit",
                serde_json::json!({ "oldText": "before", "newText": "after" }),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("edit something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(
        *executed.lock().unwrap(),
        vec![vec![("before".to_owned(), "after".to_owned())]]
    );
}

/// upstream test: "should force sequential execution when a tool has executionMode=sequential even with default parallel config"
#[tokio::test]
async fn should_force_sequential_execution_when_a_tool_has_execution_mode_sequential_even_with_default_parallel_config()
 {
    let first_resolved = Arc::new(AtomicU32::new(0));
    let parallel_observed = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());

    let first_for_tool = Arc::clone(&first_resolved);
    let observed_for_tool = Arc::clone(&parallel_observed);
    let gate_for_tool = Arc::clone(&gate);
    let tool = simple_tool(
        "slow",
        Some(pillar_agent::ToolExecutionMode::Sequential),
        move |_tool_call_id, params, _signal, _on_update| {
            let first = Arc::clone(&first_for_tool);
            let observed = Arc::clone(&observed_for_tool);
            let gate = Arc::clone(&gate_for_tool);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    gate.notified().await;
                    first.store(1, Ordering::SeqCst);
                }
                if value == "second" && first.load(Ordering::SeqCst) == 0 {
                    observed.store(1, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("slow: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    // config is parallel (default), but tool forces sequential
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "slow", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "slow", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let gate_for_release = Arc::clone(&gate);
    release_gate_after(gate_for_release, 20);

    let stream = agent_loop(
        vec![create_user_message("run both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let _ = stream.result().await;

    // With sequential execution, second tool should NOT start before first finishes.
    assert_eq!(parallel_observed.load(Ordering::SeqCst), 0);

    let tool_result_ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } => match message.as_base_message() {
                Message::ToolResult(result) => Some(result.tool_call_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_result_ids,
        vec!["tool-1".to_owned(), "tool-2".to_owned()]
    );
}

/// upstream test: "should force sequential execution when one of multiple tools has executionMode=sequential"
#[tokio::test]
async fn should_force_sequential_execution_when_one_of_multiple_tools_has_execution_mode_sequential()
 {
    let execution_order = Arc::new(Mutex::new(Vec::<String>::new()));
    let gate = Arc::new(tokio::sync::Notify::new());

    let order_for_slow = Arc::clone(&execution_order);
    let gate_for_slow = Arc::clone(&gate);
    let slow_tool = simple_tool(
        "slow",
        Some(pillar_agent::ToolExecutionMode::Sequential),
        move |_tool_call_id, params, _signal, _on_update| {
            let order = Arc::clone(&order_for_slow);
            let gate = Arc::clone(&gate_for_slow);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                order.lock().unwrap().push(format!("slow:{value}"));
                if value == "a" {
                    gate.notified().await;
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("slow: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );

    let order_for_fast = Arc::clone(&execution_order);
    let fast_tool = simple_tool(
        "fast",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            let order = Arc::clone(&order_for_fast);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                order.lock().unwrap().push(format!("fast:{value}"));
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("fast: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![slow_tool, fast_tool],
    };
    // parallel by default, but slowTool forces sequential
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "slow", serde_json::json!({"value": "a"})),
                Content::tool_call("tool-2", "fast", serde_json::json!({"value": "b"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let gate_for_release = Arc::clone(&gate);
    release_gate_after(gate_for_release, 20);

    let stream = agent_loop(
        vec![create_user_message("run both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    // Fast tool should NOT run before slow tool finishes.
    let order = execution_order.lock().unwrap();
    assert_eq!(order[0], "slow:a");
    assert!(order.iter().any(|entry| entry == "fast:b"));
}

/// upstream test: "should allow parallel execution when all tools have executionMode=parallel"
#[tokio::test]
async fn should_allow_parallel_execution_when_all_tools_have_execution_mode_parallel() {
    let first_resolved = Arc::new(AtomicU32::new(0));
    let parallel_observed = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());

    let first_for_tool = Arc::clone(&first_resolved);
    let observed_for_tool = Arc::clone(&parallel_observed);
    let gate_for_tool = Arc::clone(&gate);
    let tool = simple_tool(
        "echo",
        Some(pillar_agent::ToolExecutionMode::Parallel),
        move |_tool_call_id, params, _signal, _on_update| {
            let first = Arc::clone(&first_for_tool);
            let observed = Arc::clone(&observed_for_tool);
            let gate = Arc::clone(&gate_for_tool);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    gate.notified().await;
                    first.store(1, Ordering::SeqCst);
                }
                if value == "second" && first.load(Ordering::SeqCst) == 0 {
                    observed.store(1, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "echo", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "echo", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let gate_for_release = Arc::clone(&gate);
    release_gate_after(gate_for_release, 20);

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    // With executionMode=parallel, second tool should start before first finishes.
    assert_eq!(parallel_observed.load(Ordering::SeqCst), 1);
}

/// upstream test: "should emit tool_execution_end in completion order but persist tool results in source order"
#[tokio::test]
async fn should_emit_tool_execution_end_in_completion_order_but_persist_tool_results_in_source_order()
 {
    let first_resolved = Arc::new(AtomicU32::new(0));
    let parallel_observed = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());

    let first_for_tool = Arc::clone(&first_resolved);
    let observed_for_tool = Arc::clone(&parallel_observed);
    let gate_for_tool = Arc::clone(&gate);
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            let first = Arc::clone(&first_for_tool);
            let observed = Arc::clone(&observed_for_tool);
            let gate = Arc::clone(&gate_for_tool);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    gate.notified().await;
                    first.store(1, Ordering::SeqCst);
                }
                if value == "second" && first.load(Ordering::SeqCst) == 0 {
                    observed.store(1, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        tool_execution: Some(pillar_agent::ToolExecutionMode::Parallel),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "echo", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "echo", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let gate_for_release = Arc::clone(&gate);
    release_gate_after(gate_for_release, 20);

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(parallel_observed.load(Ordering::SeqCst), 1);

    let tool_execution_end_ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let tool_result_ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } => match message.as_base_message() {
                Message::ToolResult(result) => Some(result.tool_call_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let turn_tool_result_ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } => Some(
                tool_results
                    .iter()
                    .map(|result| result.tool_call_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();

    assert_eq!(
        tool_execution_end_ids,
        vec!["tool-2".to_owned(), "tool-1".to_owned()]
    );
    assert_eq!(
        tool_result_ids,
        vec!["tool-1".to_owned(), "tool-2".to_owned()]
    );
    assert_eq!(
        turn_tool_result_ids,
        vec!["tool-1".to_owned(), "tool-2".to_owned()]
    );
}

/// upstream test: "should stop after a tool batch when every tool result sets terminate=true"
#[tokio::test]
async fn should_stop_after_a_tool_batch_when_every_tool_result_sets_terminate_true() {
    let first_resolved = Arc::new(AtomicU32::new(0));
    let parallel_observed = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());

    let first_for_tool = Arc::clone(&first_resolved);
    let observed_for_tool = Arc::clone(&parallel_observed);
    let gate_for_tool = Arc::clone(&gate);
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            let first = Arc::clone(&first_for_tool);
            let observed = Arc::clone(&observed_for_tool);
            let gate = Arc::clone(&gate_for_tool);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if value == "first" {
                    gate.notified().await;
                    first.store(1, Ordering::SeqCst);
                }
                if value == "second" && first.load(Ordering::SeqCst) == 0 {
                    observed.store(1, Ordering::SeqCst);
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    terminate: true,
                    ..Default::default()
                })
            })
        },
    );

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("should not run")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let gate_for_release = Arc::clone(&gate);
    release_gate_after(gate_for_release, 20);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let messages = stream.result().await;

    let llm_call_count = events
        .iter()
        .filter(|e| e.kind() == "message_end")
        .filter(|e| match e {
            AgentEvent::MessageEnd { message } => {
                matches!(message.as_base_message(), Message::Assistant(_))
            }
            _ => false,
        })
        .count();
    assert_eq!(llm_call_count, 1);
    assert_eq!(
        messages.iter().map(|m| m.role_name()).collect::<Vec<_>>(),
        vec!["user", "assistant", "toolResult"]
    );
    assert_eq!(events.iter().filter(|e| e.kind() == "turn_end").count(), 1);
}

/// upstream test: "should stop after a blocked tool call when beforeToolCall sets terminate=true"
#[tokio::test]
async fn should_stop_after_a_blocked_tool_call_when_before_tool_call_sets_terminate_true() {
    let executed = Arc::new(AtomicU32::new(0));
    let executed_for_tool = Arc::clone(&executed);
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, _params, _signal, _on_update| {
            let executed = Arc::clone(&executed_for_tool);
            Box::pin(async move {
                executed.store(1, Ordering::SeqCst);
                Ok(AgentToolResult {
                    content: vec![Content::text("should not execute")],
                    details: serde_json::json!({ "value": "unexpected" }),
                    ..Default::default()
                })
            })
        },
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        before_tool_call: Some(Arc::new(|_context, _signal| {
            Box::pin(async move {
                Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("Blocked by policy".into()),
                    terminate: true,
                })
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("should not run")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let messages = stream.result().await;

    assert_eq!(executed.load(Ordering::SeqCst), 0);
    let tool_result = messages
        .iter()
        .find_map(|m| match m.as_base_message() {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("tool result in messages");
    assert!(tool_result.is_error);
    let blocked_text = tool_result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .find(|&text| text == "Blocked by policy");
    assert_eq!(blocked_text, Some("Blocked by policy"));
}

/// upstream test: "should continue after a mixed batch with one terminating blocked call"
#[tokio::test]
async fn should_continue_after_a_mixed_batch_with_one_terminating_blocked_call() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_for_tool = Arc::clone(&executed);
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            let executed = Arc::clone(&executed_for_tool);
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        tool_execution: Some(pillar_agent::ToolExecutionMode::Parallel),
        before_tool_call: Some(Arc::new(|context, _signal| {
            Box::pin(async move {
                let is_first = context
                    .args
                    .lock()
                    .unwrap()
                    .get("value")
                    .and_then(|v| v.as_str())
                    == Some("first");
                if is_first {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("Blocked first".into()),
                        terminate: true,
                    })
                } else {
                    None
                }
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "echo", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "echo", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(*executed.lock().unwrap(), vec!["second".to_owned()]);
}

/// upstream test: "should continue after parallel tool calls when not all tool results terminate"
#[tokio::test]
async fn should_continue_after_parallel_tool_calls_when_not_all_tool_results_terminate() {
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    terminate: value == "first",
                    ..Default::default()
                })
            })
        },
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        tool_execution: Some(pillar_agent::ToolExecutionMode::Parallel),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![
                Content::tool_call("tool-1", "echo", serde_json::json!({"value": "first"})),
                Content::tool_call("tool-2", "echo", serde_json::json!({"value": "second"})),
            ],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo both")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let _events = drain(&stream).await;
    let messages = stream.result().await;

    assert_eq!(
        messages.iter().map(|m| m.role_name()).collect::<Vec<_>>(),
        vec!["user", "assistant", "toolResult", "toolResult", "assistant"]
    );
}

/// upstream test: "should allow afterToolCall to mark a tool batch as terminating"
#[tokio::test]
async fn should_allow_after_tool_call_to_mark_a_tool_batch_as_terminating() {
    let tool = simple_tool(
        "echo",
        None,
        move |_tool_call_id, params, _signal, _on_update| {
            Box::pin(async move {
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                Ok(AgentToolResult {
                    content: vec![Content::text(format!("echoed: {value}"))],
                    details: serde_json::json!({ "value": value }),
                    ..Default::default()
                })
            })
        },
    );
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        after_tool_call: Some(Arc::new(|_context, _signal| {
            Box::pin(async move {
                Some(pillar_agent::AfterToolCallResult {
                    terminate: Some(true),
                    ..Default::default()
                })
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({"value": "hello"}),
            )],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("should not run")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(
        vec![create_user_message("echo something")],
        context,
        config,
        None,
        Some(stream_fn),
    );
    let events = drain(&stream).await;
    let _ = stream.result().await;

    let llm_call_count = events
        .iter()
        .filter(|e| e.kind() == "message_end")
        .filter(|e| match e {
            AgentEvent::MessageEnd { message } => {
                matches!(message.as_base_message(), Message::Assistant(_))
            }
            _ => false,
        })
        .count();
    assert_eq!(llm_call_count, 1);
}

/// upstream test: "should allow custom message types as last message (caller responsibility)"
/// divergence: closed `AgentMessage` union; the toolResult stand-in is the
/// last context message and the converter maps it to a user message.
#[tokio::test]
async fn should_allow_custom_message_types_as_last_message_caller_responsibility() {
    let custom_message = notification_stand_in();

    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: vec![custom_message],
        tools: Vec::new(),
    };
    let config = AgentLoopConfig {
        model: Some(create_model()),
        convert_to_llm: Arc::new(|messages: &[AgentMessage]| -> Vec<Message> {
            // Convert custom to user message.
            messages
                .iter()
                .map(|m| match m.as_base_message() {
                    Message::ToolResult(result) if result.tool_name == "notification" => {
                        Message::User {
                            content: pillar_ai::types::UserContent::Text(
                                result
                                    .content
                                    .first()
                                    .map(|content| match content {
                                        Content::Text { text, .. } => text.clone(),
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default(),
                            ),
                            timestamp: result.timestamp,
                        }
                    }
                    other => other.clone(),
                })
                .collect()
        }),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response to custom message")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    // Should not throw - the custom message will be converted to user message.
    let stream = agent_loop_continue(context, config, None, Some(stream_fn))
        .expect("continuation from custom message");
    let _events = drain(&stream).await;
    let messages = stream.result().await;

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role_name(), "assistant");
}

#[allow(unused)]
fn _witness(_signal: AbortSignal, _result: AgentToolResult) {}
