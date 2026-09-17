//! The shared core's tool-call contract: a declared argument schema is
//! enforced before `execute`, and a batch's results keep their source order.
//!
//! These are the behaviors the sbde1 comparison review showed were only
//! *declared* (`docs/PI-COMPARISON-sbde1.md` C1/C3, probes in
//! `crates/pillar-lmpc/tests/review_comparison_sbde1.rs`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::{
    AbortSignal, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, StreamFn, ToolExecutionMode, agent_loop,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessage, Content, Message, StopReason, Tool, Usage, UsageCost,
};

/// The tool schema under test: a required string plus facets that no coercion
/// can rescue (a numeric minimum, an enum, and a nested required property).
fn checked_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "value": { "type": "string" },
            "count": { "type": "number", "minimum": 5 },
            "mode": { "type": "string", "enum": ["fast", "slow"] },
            "nested": {
                "type": "object",
                "properties": { "flag": { "type": "boolean" } },
                "required": ["flag"],
                "additionalProperties": false
            }
        },
        "required": ["value"],
        "additionalProperties": false
    })
}

fn checked_tool(seen: Arc<Mutex<Vec<serde_json::Value>>>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "checked".into(),
            description: "schema-enforced tool".into(),
            parameters: checked_parameters(),
            constrained_sampling: None,
        },
        label: "checked".into(),
        prepare_arguments: None,
        execution_mode: Some(ToolExecutionMode::Parallel),
        execute: Arc::new(move |_id, args, _signal, _update| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                seen.lock().unwrap().push(args);
                Ok(AgentToolResult {
                    content: vec![Content::text("ran")],
                    details: serde_json::json!({}),
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                })
            })
        }),
    }
}

fn counting_tool(calls: Arc<AtomicUsize>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "checked".into(),
            description: "schema-enforced tool".into(),
            parameters: checked_parameters(),
            constrained_sampling: None,
        },
        label: "checked".into(),
        prepare_arguments: None,
        execution_mode: Some(ToolExecutionMode::Parallel),
        execute: Arc::new(move |_id, _args, _signal, _update| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult {
                    content: vec![Content::text("ran")],
                    details: serde_json::json!({}),
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                })
            })
        }),
    }
}

fn assistant_message(content: Vec<Content>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "mock".into(),
        response_model: None,
        usage: Usage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 0,
            cost: UsageCost::default(),
        },
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

/// Scripted stream fn: each call pops the next assistant message.
fn scripted_stream_fn(responses: Vec<AssistantMessage>) -> StreamFn {
    let responses = Arc::new(Mutex::new(responses));
    StreamFn::new(move |_context, _options| {
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

fn user_message(text: &str) -> AgentMessage {
    Message::User {
        content: pillar_ai::types::UserContent::Text(text.to_owned()),
        timestamp: 1,
    }
    .into()
}

/// Run one turn whose assistant message makes exactly these tool calls, and
/// return the events plus the session's message list.
async fn run_turn(
    tools: Vec<AgentTool>,
    tool_calls: Vec<Content>,
    config: AgentLoopConfig,
) -> (Vec<AgentEvent>, Vec<AgentMessage>) {
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools,
    };
    let responses = vec![
        assistant_message(tool_calls, StopReason::ToolUse),
        assistant_message(vec![Content::text("done")], StopReason::Stop),
    ];
    let stream = agent_loop(
        vec![user_message("go")],
        context,
        config,
        None,
        Some(scripted_stream_fn(responses)),
    );
    let events = drain(&stream).await;
    let messages = stream.result().await;
    (events, messages)
}

fn tool_result_texts(messages: &[AgentMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message.as_base_message() {
            Message::ToolResult(result) => Some(
                result
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// Both tools are named `checked`: one counts calls, one records arguments.
fn counting_case(calls: Arc<AtomicUsize>) -> Vec<AgentTool> {
    vec![counting_tool(calls)]
}

#[tokio::test]
async fn a_missing_required_argument_never_reaches_execute() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (_, messages) = run_turn(
        counting_case(Arc::clone(&calls)),
        vec![Content::tool_call("call-1", "checked", serde_json::json!({}))],
        AgentLoopConfig::default(),
    )
    .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an invalid call must not execute"
    );
    let texts = tool_result_texts(&messages);
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("Validation failed for tool \"checked\":"),
        "{}",
        texts[0]
    );
    assert!(
        texts[0].contains("  - value: expected a required property"),
        "{}",
        texts[0]
    );
}

#[tokio::test]
async fn a_facet_violation_never_reaches_execute() {
    for arguments in [
        // Below the minimum (a number, so coercion cannot change it).
        serde_json::json!({"value": "ok", "count": 1}),
        // Not in the enum.
        serde_json::json!({"value": "ok", "mode": "medium"}),
        // A nested required property is missing.
        serde_json::json!({"value": "ok", "nested": {}}),
        // An undeclared key.
        serde_json::json!({"value": "ok", "extra": true}),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_, messages) = run_turn(
            counting_case(Arc::clone(&calls)),
            vec![Content::tool_call("call-1", "checked", arguments.clone())],
            AgentLoopConfig::default(),
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{arguments} must not execute"
        );
        assert!(
            tool_result_texts(&messages)[0].contains("Validation failed for tool \"checked\":"),
            "{arguments}: {}",
            tool_result_texts(&messages)[0]
        );
    }
}

#[tokio::test]
async fn a_schema_we_cannot_interpret_is_fail_closed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tool = counting_tool(Arc::clone(&calls));
    tool.tool.parameters = serde_json::json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "patternProperties": { "^x": { "type": "string" } }
    });

    let (_, messages) = run_turn(
        vec![tool],
        vec![Content::tool_call(
            "call-1",
            "checked",
            serde_json::json!({"value": "ok"}),
        )],
        AgentLoopConfig::default(),
    )
    .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an uninterpretable schema must not execute"
    );
    assert!(
        tool_result_texts(&messages)[0].contains("`patternProperties` is not implemented"),
        "{}",
        tool_result_texts(&messages)[0]
    );
}

#[tokio::test]
async fn valid_arguments_are_coerced_before_execute() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (_, _) = run_turn(
        vec![checked_tool(Arc::clone(&seen))],
        vec![Content::tool_call(
            "call-1",
            "checked",
            serde_json::json!({"value": 7, "count": "6"}),
        )],
        AgentLoopConfig::default(),
    )
    .await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0],
        serde_json::json!({"value": "7", "count": 6.0}),
        "execute sees the coerced arguments the validator approved"
    );
}

#[tokio::test]
async fn a_batch_reports_its_results_in_source_order() {
    let (_, messages) = run_turn(
        vec![counting_tool(Arc::new(AtomicUsize::new(0)))],
        vec![
            Content::tool_call(
                "good",
                "checked",
                serde_json::json!({"value": "ok", "count": 5}),
            ),
            Content::tool_call("bad", "missing", serde_json::json!({})),
        ],
        AgentLoopConfig::default(),
    )
    .await;

    let order: Vec<String> = messages
        .iter()
        .filter_map(|message| match message.as_base_message() {
            Message::ToolResult(result) => Some(result.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["good".to_owned(), "bad".to_owned()],
        "the history keeps the assistant's call order, immediate failures included"
    );
}

#[tokio::test]
async fn after_tool_call_sees_the_executed_arguments_and_the_signal() {
    let observed: Arc<Mutex<Vec<(serde_json::Value, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_hook = Arc::clone(&observed);
    let config = AgentLoopConfig {
        after_tool_call: Some(Arc::new(move |context, signal| {
            let observed = Arc::clone(&observed_for_hook);
            Box::pin(async move {
                observed
                    .lock()
                    .unwrap()
                    .push((context.args.clone(), signal.is_some()));
                None
            })
        })),
        ..Default::default()
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (_, _) = run_turn(
        vec![checked_tool(Arc::clone(&seen))],
        vec![Content::tool_call(
            "call-1",
            "checked",
            serde_json::json!({"value": "ok", "count": "6"}),
        )],
        config,
    )
    .await;

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].0,
        serde_json::json!({"value": "ok", "count": 6.0}),
        "the hook receives the arguments the tool ran with, not the result details"
    );
}

#[tokio::test]
async fn the_signal_is_handed_to_after_tool_call_when_the_loop_has_one() {
    let observed: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_hook = Arc::clone(&observed);
    let config = AgentLoopConfig {
        after_tool_call: Some(Arc::new(move |_context, signal| {
            let observed = Arc::clone(&observed_for_hook);
            Box::pin(async move {
                observed.lock().unwrap().push(signal.is_some());
                None
            })
        })),
        ..Default::default()
    };

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![counting_tool(Arc::new(AtomicUsize::new(0)))],
    };
    let stream = agent_loop(
        vec![user_message("go")],
        context,
        config,
        Some(AbortSignal::new()),
        Some(scripted_stream_fn(vec![
            assistant_message(
                vec![Content::tool_call(
                    "call-1",
                    "checked",
                    serde_json::json!({"value": "ok"}),
                )],
                StopReason::ToolUse,
            ),
            assistant_message(vec![Content::text("done")], StopReason::Stop),
        ])),
    );
    let _ = drain(&stream).await;
    let _ = stream.result().await;

    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[true],
        "upstream passes the call's signal to the hook"
    );
}
