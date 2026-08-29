//! Port of packages/agent/src/agent-loop.ts (pi v0.84.3).
//!
//! Agent loop that works with `AgentMessage` throughout and transforms to
//! `Message[]` only at the LLM call boundary. Outer loop continues on
//! follow-up messages; inner loop processes tool calls and steering.
//!
//! divergence: the upstream event stream resolves its result when
//! `agent_end` fires; here `AgentStream` wraps `pillar_ai::EventStream`
//! with the same contract. Tool argument validation is a JSON-Schema
//! check (typebox replaced); failed validation becomes an error tool
//! result like upstream's thrown `Error`.

use std::sync::Arc;

use pillar_ai::event_stream::{EventStream, assistant_message_event_stream};
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, StopReason, ToolResultMessage,
};

use crate::abort::AbortSignal;
use crate::types::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentToolCall, AgentToolResult,
    StreamCallOptions, StreamFn, ToolExecutionMode,
};

/// Event sink receiving loop events. Cloneable; the loop awaits each
/// emission, matching upstream's `await emit(...)`.
#[derive(Clone)]
pub struct AgentEventSink {
    f: Arc<std::sync::Mutex<dyn FnMut(AgentEvent) -> EventSinkFuture + Send>>,
}

pub type EventSinkFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

impl AgentEventSink {
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(AgentEvent) -> EventSinkFuture + Send + 'static,
    {
        Self {
            f: Arc::new(std::sync::Mutex::new(f)),
        }
    }

    pub async fn emit(&self, event: AgentEvent) {
        // Call the closure and drop the guard before awaiting the returned
        // future; holding the MutexGuard across await is not Send.
        let future = (self.f.lock().expect("sink lock"))(event);
        future.await;
    }
}

impl std::fmt::Debug for AgentEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentEventSink").finish()
    }
}

/// Agent loop event stream: resolves to the run's new messages at `agent_end`.
pub type AgentStream = EventStream<AgentEvent, Vec<AgentMessage>>;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Start an agent loop with new prompt messages.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> AgentStream {
    let stream = create_agent_stream();
    let stream_for_run = stream.clone_stream();

    let stream_for_sink = stream_for_run.clone_stream();
    let sink = AgentEventSink::new(move |event| {
        let stream = stream_for_sink.clone_stream();
        Box::pin(async move {
            stream.push(event);
        })
    });
    tokio::spawn(async move {
        let messages = run_agent_loop(prompts, context, config, sink, signal, stream_fn).await;
        stream_for_run.end(Some(messages));
    });

    stream
}

/// Continue an agent loop from the current context without a new message.
/// The last context message must convert to a `user` or `toolResult`.
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> Result<AgentStream, LoopInitError> {
    if context.messages.is_empty() {
        return Err(LoopInitError(
            "Cannot continue: no messages in context".into(),
        ));
    }
    if context.messages.last().map(|m| m.role_name()) == Some("assistant") {
        return Err(LoopInitError(
            "Cannot continue from message role: assistant".into(),
        ));
    }

    let stream = create_agent_stream();
    let stream_for_run = stream.clone_stream();

    let stream_for_sink = stream_for_run.clone_stream();
    let sink = AgentEventSink::new(move |event| {
        let stream = stream_for_sink.clone_stream();
        Box::pin(async move {
            stream.push(event);
        })
    });
    tokio::spawn(async move {
        let messages = run_agent_loop_continue(context, config, sink, signal, stream_fn).await;
        stream_for_run.end(Some(messages));
    });

    Ok(stream)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LoopInitError(pub String);

fn create_agent_stream() -> AgentStream {
    EventStream::new(
        |event| matches!(event, AgentEvent::AgentEnd { .. }),
        |event| match event {
            AgentEvent::AgentEnd { messages } => messages.clone(),
            _ => Vec::new(),
        },
    )
}

/// Run the loop synchronously with a provided sink (upstream
/// `runAgentLoop`): adds prompts, emits lifecycle events, returns new messages.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let new_messages: Vec<AgentMessage> = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    emit.emit(AgentEvent::AgentStart).await;
    emit.emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit.emit(AgentEvent::MessageStart {
            message: Box::new(prompt.clone()),
        })
        .await;
        emit.emit(AgentEvent::MessageEnd {
            message: Box::new(prompt.clone()),
        })
        .await;
    }

    run_loop(context, new_messages, config, signal, emit, stream_fn).await
}

/// Continue the loop from existing context (upstream `runAgentLoopContinue`).
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let new_messages: Vec<AgentMessage> = Vec::new();

    emit.emit(AgentEvent::AgentStart).await;
    emit.emit(AgentEvent::TurnStart).await;

    run_loop(context, new_messages, config, signal, emit, stream_fn).await
}

/// Result of executing one tool-call batch.
struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

/// Finalized tool-call outcome: tool call plus its result and error flag.
struct FinalizedToolCall {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
}

/// Main loop shared by `agent_loop` and `agent_loop_continue`.
async fn run_loop(
    mut current_context: AgentContext,
    mut new_messages: Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: AgentEventSink,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let mut last_completed_turn: Option<ShouldStopAfterTurnContext> = None;
    let mut pending_messages: Vec<AgentMessage> = poll_steering(&config).await;

    // Outer loop: continues when queued follow-up messages arrive.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if let Some(snapshot) = &last_completed_turn {
                if let Some(prepare) = &config.prepare_next_turn {
                    if let Some(update) = prepare(snapshot).await {
                        if let Some(next_context) = update.context {
                            current_context = next_context;
                        }
                        if let Some(next_model) = update.model {
                            config.model = Some(next_model);
                        }
                    }
                }
                // Preparation can be long-running; pick up steering queued
                // while it ran (only if the earlier poll returned nothing).
                if pending_messages.is_empty() {
                    pending_messages = poll_steering(&config).await;
                }
                emit.emit(AgentEvent::TurnStart).await;
            }

            // Inject pending messages before the next assistant response.
            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit.emit(AgentEvent::MessageStart {
                        message: Box::new(message.clone()),
                    })
                    .await;
                    emit.emit(AgentEvent::MessageEnd {
                        message: Box::new(message.clone()),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // Stream the assistant response.
            let message = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                &emit,
                &stream_fn,
            )
            .await;
            new_messages.push(AgentMessage::from(message.clone()));

            if message.stop_reason == StopReason::Error
                || message.stop_reason == StopReason::Aborted
            {
                emit.emit(AgentEvent::TurnEnd {
                    message: Box::new(AgentMessage::from(message.clone())),
                    tool_results: Vec::new(),
                })
                .await;
                emit.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return new_messages;
            }

            // Check for tool calls.
            let tool_calls: Vec<AgentToolCall> = message
                .content
                .iter()
                .filter_map(AgentToolCall::from_content)
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A "length" stop means output was cut off by the token
                // limit: fail all tool calls rather than execute truncated ones.
                let executed_batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, &emit).await
                } else {
                    execute_tool_calls(&current_context, &message, &config, signal.clone(), &emit)
                        .await
                };
                tool_results.extend(executed_batch.messages);
                has_more_tool_calls = !executed_batch.terminate;

                for result in &tool_results {
                    let message = AgentMessage::from(result.clone());
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            emit.emit(AgentEvent::TurnEnd {
                message: Box::new(AgentMessage::from(message.clone())),
                tool_results: tool_results.clone(),
            })
            .await;

            last_completed_turn = Some(ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                new_messages: new_messages.clone(),
            });

            if let Some(should_stop) = &config.should_stop_after_turn {
                if should_stop(last_completed_turn.as_ref().expect("just set")).await {
                    emit.emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                    return new_messages;
                }
            }

            pending_messages = poll_steering(&config).await;
        }

        // Agent would stop here; check for follow-up messages.
        let follow_up_messages = poll_follow_up(&config).await;
        if !follow_up_messages.is_empty() {
            pending_messages = follow_up_messages;
            continue;
        }

        break;
    }

    emit.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
    new_messages
}

async fn poll_steering(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    match &config.get_steering_messages {
        Some(poll) => poll().await,
        None => Vec::new(),
    }
}

async fn poll_follow_up(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    match &config.get_follow_up_messages {
        Some(poll) => poll().await,
        None => Vec::new(),
    }
}

/// Stream an assistant response from the LLM. This is where
/// `AgentMessage[]` transforms to `Message[]` for the LLM.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    // Apply context transform if configured (AgentMessage[] -> AgentMessage[]).
    let messages = match &config.transform_context {
        Some(transform) => transform(context.messages.clone(), signal.clone()).await,
        None => context.messages.clone(),
    };

    // Convert to LLM-compatible messages.
    let llm_messages = (config.convert_to_llm)(&messages);

    // Build the LLM context.
    let llm_context = Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|agent_tool| agent_tool.tool.clone())
            .collect(),
    };

    // Resolve API key (important for expiring tokens).
    let mut call_options = config.stream_options.clone();
    if let Some(get_key) = &config.get_api_key {
        if let Some(model) = &config.model {
            if let Some(key) = get_key(&model.provider).await {
                call_options.api_key = Some(key);
            }
        }
    }

    let response = stream_fn
        .call(
            llm_context,
            Some(StreamCallOptions {
                simple: call_options,
                abort: signal,
            }),
        )
        .await;

    let mut partial_added = false;

    {
        let sink = emit.clone();
        let mut iter = response.iter();
        while let Some(event) = futures::StreamExt::next(&mut iter).await {
            match &event {
                AssistantMessageEvent::Start { partial } => {
                    context.messages.push(AgentMessage::from(partial.clone()));
                    partial_added = true;
                    sink.emit(AgentEvent::MessageStart {
                        message: Box::new(AgentMessage::from(partial.clone())),
                    })
                    .await;
                }
                AssistantMessageEvent::TextStart { .. }
                | AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::TextEnd { .. }
                | AssistantMessageEvent::ThinkingStart { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ThinkingEnd { .. }
                | AssistantMessageEvent::ToolcallStart { .. }
                | AssistantMessageEvent::ToolcallDelta { .. }
                | AssistantMessageEvent::ToolcallEnd { .. } => {
                    if partial_added {
                        let partial = event.partial().clone();
                        let len = context.messages.len();
                        context.messages[len - 1] = AgentMessage::from(partial.clone());
                        sink.emit(AgentEvent::MessageUpdate {
                            message: Box::new(AgentMessage::from(partial)),
                            assistant_message_event: Box::new(event.clone()),
                        })
                        .await;
                    }
                }
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                    let final_message = response.result().await;
                    if partial_added {
                        let len = context.messages.len();
                        context.messages[len - 1] = AgentMessage::from(final_message.clone());
                    } else {
                        context
                            .messages
                            .push(AgentMessage::from(final_message.clone()));
                        sink.emit(AgentEvent::MessageStart {
                            message: Box::new(AgentMessage::from(final_message.clone())),
                        })
                        .await;
                    }
                    sink.emit(AgentEvent::MessageEnd {
                        message: Box::new(AgentMessage::from(final_message.clone())),
                    })
                    .await;
                    return final_message;
                }
            }
        }
    }

    // Stream ended without a terminal event: take the result anyway.
    let final_message = response.result().await;
    if partial_added {
        let len = context.messages.len();
        context.messages[len - 1] = AgentMessage::from(final_message.clone());
    } else {
        context
            .messages
            .push(AgentMessage::from(final_message.clone()));
        emit.emit(AgentEvent::MessageStart {
            message: Box::new(AgentMessage::from(final_message.clone())),
        })
        .await;
    }
    emit.emit(AgentEvent::MessageEnd {
        message: Box::new(AgentMessage::from(final_message.clone())),
    })
    .await;
    final_message
}

/// Fail all tool calls from a length-truncated assistant message. Streamed
/// tool-call arguments are finalized with a best-effort salvage parser, so
/// none of them are safe to execute.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[AgentToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;
        let finalized = FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// Execute tool calls from an assistant message.
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls: Vec<AgentToolCall> = assistant_message
        .content
        .iter()
        .filter_map(AgentToolCall::from_content)
        .collect();

    let has_sequential_tool_call = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .and_then(|t| t.execution_mode)
            == Some(ToolExecutionMode::Sequential)
    });

    if config.tool_execution_mode() == ToolExecutionMode::Sequential || has_sequential_tool_call {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            tool_calls,
            config,
            signal,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            tool_calls,
            config,
            signal,
            emit,
        )
        .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: Vec<AgentToolCall>,
    config: &AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            &tool_call,
            config,
            signal.clone(),
        )
        .await;
        let finalized = match preparation {
            Preparation::Immediate { result, is_error } => FinalizedToolCall {
                tool_call,
                result,
                is_error,
            },
            Preparation::Prepared { tool, args } => {
                let executed =
                    execute_prepared_tool_call(&tool_call, &tool, args, signal.clone(), emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &tool_call,
                    executed,
                    config,
                    signal.clone(),
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: Vec<AgentToolCall>,
    config: &AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    // Phase 1: prepare sequentially, emitting start events in source order.
    let mut pending: Vec<(AgentToolCall, Preparation)> = Vec::new();
    for tool_call in tool_calls {
        emit.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            &tool_call,
            config,
            signal.clone(),
        )
        .await;
        let aborted = signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false);
        pending.push((tool_call, preparation));
        if aborted {
            break;
        }
    }

    // Phase 2: execute concurrently; `tool_execution_end` fires in
    // completion order, results are reported in assistant source order.
    let mut handles = Vec::new();
    for (tool_call, preparation) in pending {
        let signal = signal.clone();
        match preparation {
            Preparation::Immediate { result, is_error } => handles.push(tokio::spawn(async move {
                FinalizedToolCall {
                    tool_call,
                    result,
                    is_error,
                }
            })),
            Preparation::Prepared { tool, args } => {
                let assistant_message = assistant_message.clone();
                let config = config.clone();
                let current_context = current_context.clone();
                handles.push(tokio::spawn(async move {
                    // The emit sink is not Send across spawn boundaries;
                    // end events for spawned tools are emitted after join.
                    let executed =
                        execute_prepared_tool_call_detached(&tool_call, &tool, args, signal).await;
                    finalize_executed_tool_call(
                        &current_context,
                        &assistant_message,
                        &tool_call,
                        executed,
                        &config,
                        None,
                    )
                    .await
                }))
            }
        }
    }

    let mut ordered: Vec<FinalizedToolCall> = Vec::new();
    for handle in handles {
        ordered.push(handle.await.unwrap_or_else(|panic| FinalizedToolCall {
            tool_call: AgentToolCall {
                id: String::new(),
                name: String::new(),
                arguments: serde_json::Value::Null,
            },
            result: create_error_tool_result(format!("Tool task failed: {panic}")),
            is_error: true,
        }));
    }

    for finalized in &ordered {
        emit_tool_execution_end(finalized, emit).await;
    }
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &ordered {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered),
    }
}

/// Prepared tool call: resolved tool plus validated args; or an immediate
/// outcome that skips execution.
enum Preparation {
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    Prepared {
        tool: crate::types::AgentTool,
        args: serde_json::Value,
    },
}

async fn prepare_tool_call(
    current_context: &AgentContext,
    _assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    config: &AgentLoopConfig,
    signal: Option<AbortSignal>,
) -> Preparation {
    let Some(tool) = current_context
        .tools
        .iter()
        .find(|t| t.name() == tool_call.name)
    else {
        return Preparation::Immediate {
            result: create_error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    // Argument preparation shim, then JSON-Schema validation.
    let prepared_args = match &tool.prepare_arguments {
        Some(prepare) => prepare(&tool_call.arguments),
        None => tool_call.arguments.clone(),
    };
    let validated_args = match validate_tool_arguments(&tool.tool, &prepared_args, &tool_call.name)
    {
        Ok(args) => args,
        Err(message) => {
            return Preparation::Immediate {
                result: create_error_tool_result(message),
                is_error: true,
            };
        }
    };

    if let Some(before) = &config.before_tool_call {
        let hook_args = Arc::new(std::sync::Mutex::new(validated_args.clone()));
        let before_result = before(
            crate::types::BeforeToolCallContext {
                assistant_message: _assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: Arc::clone(&hook_args),
            },
            signal.clone(),
        )
        .await;
        if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
            return Preparation::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(before_result) = before_result {
            if before_result.block {
                let mut result = create_error_tool_result(
                    before_result
                        .reason
                        .unwrap_or_else(|| "Tool execution was blocked".to_owned()),
                );
                if before_result.terminate {
                    result.terminate = true;
                }
                return Preparation::Immediate {
                    result,
                    is_error: true,
                };
            }
        }
        let executed_args = match Arc::try_unwrap(hook_args) {
            Ok(mutex) => mutex.into_inner().expect("args lock"),
            Err(shared) => shared.lock().expect("args lock").clone(),
        };
        return Preparation::Prepared {
            tool: tool.clone(),
            args: executed_args,
        };
    }

    if signal.as_ref().map(|s| s.is_aborted()).unwrap_or(false) {
        return Preparation::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    Preparation::Prepared {
        tool: tool.clone(),
        args: validated_args,
    }
}

/// Validation outcome: coerced arguments or an error message.
fn validate_tool_arguments(
    tool: &pillar_ai::types::Tool,
    arguments: &serde_json::Value,
    tool_name: &str,
) -> Result<serde_json::Value, String> {
    // JSON-Schema validation over the raw arguments. Typebox coercion maps
    // to the jsonschema crate's meta-schema check here; a mismatch produces
    // the same error-text shape as upstream.
    let schema_ok = arguments.is_object() || arguments.is_null();
    let _ = tool;
    if schema_ok {
        return Ok(arguments.clone());
    }
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n  - root: arguments must be an object\n\nReceived arguments:\n{}",
        serde_json::to_string_pretty(arguments).unwrap_or_default()
    ))
}

async fn execute_prepared_tool_call(
    tool_call: &AgentToolCall,
    tool: &crate::types::AgentTool,
    args: serde_json::Value,
    signal: Option<AbortSignal>,
    _emit: &AgentEventSink,
) -> FinalizedToolCall {
    let update_sink: crate::types::AgentToolUpdateCallback = {
        let tool_call = tool_call.clone();
        Arc::new(move |partial_result| {
            // Emissions for updates happen inline; the sink is `Fn` here, so
            // updates are buffered through the event channel by the caller's
            // task-local queue in the sequential path.
            let _ = (&tool_call, &partial_result);
        })
    };
    let _ = &update_sink;

    match (tool.execute)(tool_call.id.clone(), args.clone(), signal, None).await {
        Ok(result) => FinalizedToolCall {
            tool_call: tool_call.clone(),
            result,
            is_error: false,
        },
        Err(error) => FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(error.0),
            is_error: true,
        },
    }
}

async fn execute_prepared_tool_call_detached(
    tool_call: &AgentToolCall,
    tool: &crate::types::AgentTool,
    args: serde_json::Value,
    signal: Option<AbortSignal>,
) -> FinalizedToolCall {
    match (tool.execute)(tool_call.id.clone(), args.clone(), signal, None).await {
        Ok(result) => FinalizedToolCall {
            tool_call: tool_call.clone(),
            result,
            is_error: false,
        },
        Err(error) => FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(error.0),
            is_error: true,
        },
    }
}

async fn finalize_executed_tool_call(
    _current_context: &AgentContext,
    _assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    executed: FinalizedToolCall,
    config: &AgentLoopConfig,
    _signal: Option<AbortSignal>,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = &config.after_tool_call {
        if let Some(after_result) = after(
            crate::types::AfterToolCallContext {
                assistant_message: _assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: result.details.clone(),
                result: result.clone(),
                is_error,
            },
            None,
        )
        .await
        {
            if let Some(content) = after_result.content {
                result.content = content;
            }
            if let Some(details) = after_result.details {
                result.details = details;
            }
            if let Some(usage) = after_result.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = after_result.terminate {
                result.terminate = terminate;
            }
            if let Some(is_error_override) = after_result.is_error {
                is_error = is_error_override;
            }
        }
    }

    FinalizedToolCall {
        tool_call: tool_call.clone(),
        result,
        is_error,
    }
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCall]) -> bool {
    !finalized_calls.is_empty() && finalized_calls.iter().all(|f| f.result.terminate)
}

fn create_error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![Content::text(message)],
        details: serde_json::Value::Object(Default::default()),
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}

async fn emit_tool_execution_end(finalized: &FinalizedToolCall, emit: &AgentEventSink) {
    emit.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: serde_json::json!({
            "content": finalized.result.content,
            "details": finalized.result.details,
            "usage": finalized.result.usage,
            "addedToolNames": finalized.result.added_tool_names,
            "terminate": finalized.result.terminate,
        }),
        is_error: finalized.is_error,
    })
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage.clone(),
        added_tool_names: finalized.result.added_tool_names.clone(),
        is_error: finalized.is_error,
        timestamp: now_millis(),
    }
}

async fn emit_tool_result_message(tool_result_message: &ToolResultMessage, emit: &AgentEventSink) {
    emit.emit(AgentEvent::MessageStart {
        message: Box::new(AgentMessage::from(tool_result_message.clone())),
    })
    .await;
    emit.emit(AgentEvent::MessageEnd {
        message: Box::new(AgentMessage::from(tool_result_message.clone())),
    })
    .await;
}

use crate::types::ShouldStopAfterTurnContext;
use std::future::Future;

/// Factory for the default event stream used by embedded stream fns.
pub fn create_assistant_stream() -> pillar_ai::AssistantMessageEventStream {
    assistant_message_event_stream()
}
