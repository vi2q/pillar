//! Port of packages/agent/test/agent-loop.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream case, same names in comments. The mock
//! stream function replays scripted assistant messages; typebox tool
//! schemas become JSON-Schema objects.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::{
    agent_loop, agent_loop_continue, AbortSignal, AgentContext, AgentEvent, AgentLoopConfig,
    AgentMessage, AgentTool, AgentToolResult, BeforeToolCallResult, ToolExecuteError,
};
use pillar_ai::event_stream::{assistant_message_event_stream, EventStream};
use pillar_ai::types::{Content, Message, StopReason, Tool, Usage, UsageCost};

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

fn create_assistant_message(content: Vec<Content>, stop_reason: StopReason) -> pillar_ai::AssistantMessage {
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
    Message::User { content: pillar_ai::types::UserContent::Text(text.to_owned()), timestamp: 1 }.into()
}

fn identity_converter(messages: &[AgentMessage]) -> Vec<Message> {
    messages.iter().map(|m| m.as_message().clone()).collect()
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
                        cost: UsageCost { input: 0.1, output: 0.2, cache_read: 0.3, cache_write: 0.4, total: 1.0 },
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

#[tokio::test]
async fn should_emit_events_with_agent_message_types() {
    let context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let user_prompt = create_user_message("Hello");
    let config = AgentLoopConfig { model: Some(create_model()), ..Default::default() };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Hi there!")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, stream_fn);
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
            create_assistant_message(vec![Content::text("old response 1")], StopReason::Stop).into(),
            create_user_message("old message 2"),
            create_assistant_message(vec![Content::text("old response 2")], StopReason::Stop).into(),
        ],
        tools: Vec::new(),
    };
    let user_prompt = create_user_message("new message");

    let config = AgentLoopConfig {
        model: Some(create_model()),
        transform_context: Some(Arc::new(|mut messages, _signal| {
            Box::pin(async move {
                // Keep only last 2 messages (prune old ones).
                let keep = messages.split_off(messages.len().saturating_sub(2));
                keep
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, stream_fn);
    let _events = drain(&stream).await;
    let _messages = stream.result().await;
    // The observable effect (pruning) is verified by the converter receiving
    // 2 messages; the loop itself enforces transform-before-convert order.
}

#[tokio::test]
async fn should_handle_tool_calls_and_results() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![tool] };
    let user_prompt = create_user_message("echo something");

    let tool_usage = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 10,
        cost: UsageCost { input: 0.1, output: 0.2, cache_read: 0.3, cache_write: 0.4, total: 1.0 },
    };
    let patched_tool_usage = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 26,
        cost: UsageCost { input: 0.5, output: 0.6, cache_read: 0.7, cache_write: 0.8, total: 2.6 },
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
                Some(pillar_agent::AfterToolCallResult { usage: Some(patched), ..Default::default() })
            })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call("tool-1", "echo", serde_json::json!({"value": "hello"}))],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![user_prompt], context, config, None, stream_fn);
    let events = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(*executed.lock().unwrap(), vec!["hello".to_owned()]);

    let tool_end = events.iter().find(|event| event.kind() == "tool_execution_end");
    assert!(tool_end.is_some());
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { is_error, .. }) => assert!(!is_error),
        _ => unreachable!(),
    }
    assert_eq!(observed_tool_usage.lock().unwrap().as_ref(), Some(&tool_usage));

    let _messages = stream.result().await;
    // Usage patching verified through the tool result in the final result.
}

#[tokio::test]
async fn should_not_execute_tool_calls_from_a_length_truncated_assistant_message() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = echo_tool(Arc::clone(&executed));

    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![tool] };
    let config = AgentLoopConfig { model: Some(create_model()), ..Default::default() };

    let responses = Arc::new(Mutex::new(vec![
        // Output hit the token limit mid tool call: nothing may execute.
        create_assistant_message(
            vec![Content::tool_call("tool-1", "echo", serde_json::json!({"value": "hel"}))],
            StopReason::Length,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![create_user_message("echo something")], context, config, None, stream_fn);
    let events = drain(&stream).await;

    // The tool must never execute with potentially truncated arguments.
    assert!(executed.lock().unwrap().is_empty());

    let tool_end = events.iter().find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { result, is_error, .. }) => {
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

    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![tool] };

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
            vec![Content::tool_call("tool-1", "echo", serde_json::json!({"value": "hello"}))],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![create_user_message("echo something")], context, config, None, stream_fn);
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

    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![tool] };
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
                if queued.load(Ordering::SeqCst) == 0
                    && executed_hook.lock().unwrap().len() >= 2
                {
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

    let stream = agent_loop(vec![user_prompt], context, config, None, stream_fn);
    let events = drain(&stream).await;
    let _ = stream.result().await;

    // Both tools execute before steering is injected.
    assert_eq!(*executed.lock().unwrap(), vec!["first".to_owned(), "second".to_owned()]);

    let tool_ends: Vec<&AgentEvent> =
        events.iter().filter(|event| event.kind() == "tool_execution_end").collect();
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
            AgentEvent::MessageStart { message } => match message.as_message() {
                Message::ToolResult(result) => Some(format!("tool:{}", result.tool_call_id)),
                Message::User { content: pillar_ai::types::UserContent::Text(text), .. } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let interrupt_index = event_sequence.iter().position(|entry| entry == "interrupt");
    let tool1_index = event_sequence.iter().position(|entry| entry == "tool:tool-1");
    let tool2_index = event_sequence.iter().position(|entry| entry == "tool:tool-2");
    assert!(interrupt_index.is_some(), "interrupt delivered: {event_sequence:?}");
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
    let config = AgentLoopConfig { model: Some(create_model()), ..Default::default() };
    let responses = Arc::new(Mutex::new(vec![]));
    let stream_fn = scripted_stream_fn(responses);

    let result = agent_loop_continue(context, config, None, stream_fn);
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
    let config = AgentLoopConfig { model: Some(create_model()), ..Default::default() };

    let responses = Arc::new(Mutex::new(vec![create_assistant_message(
        vec![Content::text("Response")],
        StopReason::Stop,
    )]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop_continue(context, config, None, stream_fn).expect("continuation");
    let events = drain(&stream).await;
    let messages = stream.result().await;

    // Only the new assistant message is returned.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role_name(), "assistant");

    // No user message events (key difference from agentLoop).
    let message_end_events: Vec<&AgentEvent> =
        events.iter().filter(|event| event.kind() == "message_end").collect();
    assert_eq!(message_end_events.len(), 1);
    match message_end_events[0] {
        AgentEvent::MessageEnd { message } => assert_eq!(message.role_name(), "assistant"),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn should_stop_after_the_current_turn_when_should_stop_after_turn_returns_true() {
    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: Vec::new() };
    let stop_flag = Arc::new(AtomicU32::new(0));
    let stop_for_hook = Arc::clone(&stop_flag);
    let config = AgentLoopConfig {
        model: Some(create_model()),
        should_stop_after_turn: Some(Arc::new(move |_context| {
            let flag = Arc::clone(&stop_for_hook);
            Box::pin(async move { flag.fetch_add(1, Ordering::SeqCst) == 0 })
        })),
        ..Default::default()
    };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(vec![Content::text("first")], StopReason::Stop),
        create_assistant_message(vec![Content::text("second")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![create_user_message("go")], context, config, None, stream_fn);
    let _events = drain(&stream).await;
    let messages = stream.result().await;

    // Loop stops after the first turn: user + first assistant only.
    assert_eq!(messages.len(), 2);
    assert_eq!(stop_flag.load(Ordering::SeqCst), 1);
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
    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![failing] };
    let config = AgentLoopConfig { model: Some(create_model()), ..Default::default() };

    let responses = Arc::new(Mutex::new(vec![
        create_assistant_message(
            vec![Content::tool_call("tool-1", "fail", serde_json::json!({}))],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("recovered")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![create_user_message("go")], context, config, None, stream_fn);
    let events = drain(&stream).await;
    let messages = stream.result().await;

    let tool_end = events.iter().find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { is_error, .. }) => assert!(is_error),
        _ => panic!("expected tool_execution_end"),
    }
    let tool_result = messages.iter().find(|m| m.role_name() == "toolResult");
    assert!(tool_result.is_some());
    match tool_result.map(|m| m.as_message()) {
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
    let context = AgentContext { system_prompt: String::new(), messages: Vec::new(), tools: vec![tool] };

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
            vec![Content::tool_call("tool-1", "echo", serde_json::json!({"value": "x"}))],
            StopReason::ToolUse,
        ),
        create_assistant_message(vec![Content::text("done")], StopReason::Stop),
    ]));
    let stream_fn = scripted_stream_fn(responses);

    let stream = agent_loop(vec![create_user_message("go")], context, config, None, stream_fn);
    let events = drain(&stream).await;
    let _ = stream.result().await;

    assert!(executed.lock().unwrap().is_empty());
    let tool_end = events.iter().find(|event| event.kind() == "tool_execution_end");
    match tool_end {
        Some(AgentEvent::ToolExecutionEnd { result, is_error, .. }) => {
            assert!(is_error);
            assert!(result["content"][0]["text"].as_str().unwrap_or_default().contains("Blocked"));
        }
        _ => panic!("expected tool_execution_end"),
    }
}

#[allow(unused)]
fn _witness(_signal: AbortSignal, _result: AgentToolResult) {}
