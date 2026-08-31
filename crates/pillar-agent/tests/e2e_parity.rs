//! Port of packages/agent/test/e2e.test.ts (pi v0.84.3) — Agent integration
//! with the faux provider, plus Agent.continue() validation and
//! continuation cases.
//!
//! divergence: upstream routes through `registerFauxProvider` +
//! `streamSimple` (a global provider registry); the port wires
//! `FauxCore::stream` into an agent `StreamFn` directly. Upstream
//! `streamSimple` delegates to the faux provider's `stream`, so the two
//! are behaviorally identical. The port's `AbortSignal` bridges to the
//! faux provider's shared-abort flag via a forwarder task. Upstream's
//! `new Function` expression evaluator becomes a small recursive-descent
//! arithmetic parser (the calculate tool only ever receives arithmetic).

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pillar_agent::{
    Agent, AgentOptions, AgentState, AgentThinkingLevel, AgentTool, AgentToolResult, FauxModelRef,
    ListenerFuture, StreamFn, ToolExecuteError,
};
use pillar_ai::faux::{
    FauxContent, FauxCore, FauxMessageOptions, FauxModel, FauxModelDefinition, FauxProviderState,
    FauxResponseFactory, FauxResponseStep, FauxStreamOptions, RegisterFauxProviderOptions,
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call,
    tokio_util_abort::SharedAbort,
};
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, StopReason, Tool, Usage, UsageCost, UserContent,
};
use serde_json::{Value, json};

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn zero_usage() -> Usage {
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

// --- calculate tool (upstream test/utils/calculate.ts) -------------------

/// Recursive-descent arithmetic evaluator standing in for upstream
/// `new Function(`return ${expression}`)()`.
struct ExpressionParser {
    chars: Vec<char>,
    pos: usize,
}

impl ExpressionParser {
    fn parse(input: &str) -> Result<f64, String> {
        let mut parser = Self {
            chars: input.chars().collect(),
            pos: 0,
        };
        let value = parser.expression()?;
        parser.skip_whitespace();
        if parser.pos != parser.chars.len() {
            return Err(format!("Unexpected trailing input at index {}", parser.pos));
        }
        Ok(value)
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn expression(&mut self) -> Result<f64, String> {
        let mut value = self.term()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    value += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut value = self.factor()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    value *= self.factor()?;
                }
                Some('/') => {
                    self.pos += 1;
                    value /= self.factor()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn factor(&mut self) -> Result<f64, String> {
        self.skip_whitespace();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let value = self.expression()?;
                self.skip_whitespace();
                if self.peek() != Some(')') {
                    return Err("Expected closing parenthesis".to_owned());
                }
                self.pos += 1;
                Ok(value)
            }
            Some('-') => {
                self.pos += 1;
                self.factor()
            }
            Some('+') => {
                self.pos += 1;
                self.factor()
            }
            Some(character) if character.is_ascii_digit() => self.number(),
            Some(character) => Err(format!("Unexpected character: {character}")),
            None => Err("Unexpected end of expression".to_owned()),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_digit() || self.chars[self.pos] == '.')
        {
            self.pos += 1;
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map_err(|error| format!("Invalid number: {error}"))
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn calculate(expression: &str) -> Result<String, String> {
    ExpressionParser::parse(expression)
        .map(|result| format!("{expression} = {}", format_number(result)))
}

fn calculate_tool() -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "calculate".to_owned(),
            description: "Evaluate mathematical expressions".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "The mathematical expression to evaluate"
                    }
                }
            }),
            constrained_sampling: None,
        },
        label: "Calculator".to_owned(),
        prepare_arguments: None,
        execute: Arc::new(|_tool_call_id, args: Value, _signal, _on_update| {
            Box::pin(async move {
                let expression = args
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match calculate(&expression) {
                    Ok(text) => Ok(AgentToolResult {
                        content: vec![Content::text(text)],
                        details: json!({}),
                        ..AgentToolResult::default()
                    }),
                    Err(error) => Err(ToolExecuteError(error)),
                }
            })
        }),
        execution_mode: None,
    }
}

// --- faux bridging (upstream createFauxRegistration + streamSimple) ------

/// Wire `FauxCore::stream` into an agent `StreamFn` (upstream
/// `streamSimple` delegates to the faux provider's `stream`). Bridges the
/// agent abort signal to the faux provider's shared-abort flag.
fn faux_stream_fn(core: &Arc<FauxCore>, model: &FauxModel) -> StreamFn {
    let core = Arc::clone(core);
    let model = model.clone();
    StreamFn::new(
        move |context: Context, options: Option<pillar_agent::StreamCallOptions>| {
            let core = Arc::clone(&core);
            let model = model.clone();
            async move {
                let shared = SharedAbort::new();
                if let Some(signal) = options.as_ref().and_then(|options| options.abort.clone()) {
                    let shared_for_task = shared.clone();
                    tokio::spawn(async move {
                        signal.aborted().await;
                        shared_for_task.abort();
                    });
                }
                let faux_options = FauxStreamOptions {
                    session_id: options
                        .as_ref()
                        .and_then(|options| options.simple.session_id.clone()),
                    signal: Some(shared),
                    ..FauxStreamOptions::default()
                };
                core.stream(&model, context, Some(faux_options))
            }
        },
    )
}

fn faux_model_ref(model: &FauxModel) -> FauxModelRef {
    FauxModelRef::from_faux(model)
}

/// Placeholder stream fn for AgentOptions::new (each agent overrides it).
fn dummy_stream_fn() -> StreamFn {
    StreamFn::new(|_context, _options| async {
        unreachable!("create_agent overrides the stream fn")
    })
}

fn create_agent(
    stream_fn: StreamFn,
    system_prompt: &str,
    model: &FauxModel,
    tools: Vec<AgentTool>,
    thinking_level: AgentThinkingLevel,
) -> Arc<Agent> {
    Arc::new(Agent::new(AgentOptions {
        initial_state: Some(AgentState {
            system_prompt: system_prompt.to_owned(),
            model: faux_model_ref(model),
            thinking_level,
            tools,
            ..AgentState::default()
        }),
        stream_fn: Some(stream_fn),
        ..AgentOptions::new(dummy_stream_fn())
    }))
}

// --- message helpers ------------------------------------------------------

fn user_message(text: &str) -> pillar_agent::AgentMessage {
    Message::User {
        content: UserContent::Text(text.to_owned()),
        timestamp: now_millis(),
    }
    .into()
}

fn scripted_assistant_message(
    model: &FauxModel,
    content: Vec<Content>,
    stop_reason: StopReason,
) -> pillar_agent::AgentMessage {
    AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        usage: zero_usage(),
        stop_reason,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_millis(),
    }
    .into()
}

fn scripted_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    text: &str,
) -> pillar_agent::AgentMessage {
    pillar_ai::types::ToolResultMessage {
        tool_call_id: tool_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: now_millis(),
    }
    .into()
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Upstream `getTextContent`: join text blocks with newlines.
fn get_text_content(message: &pillar_agent::AgentMessage) -> String {
    let blocks: &[Content] = match message {
        pillar_agent::AgentMessage::Message(Message::Assistant(assistant)) => &assistant.content,
        pillar_agent::AgentMessage::Message(Message::ToolResult(result)) => &result.content,
        _ => &[],
    };
    blocks
        .iter()
        .filter_map(|block| match block {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_assistant(messages: &[pillar_agent::AgentMessage]) -> &pillar_agent::AgentMessage {
    messages
        .last()
        .filter(|message| {
            matches!(
                message,
                pillar_agent::AgentMessage::Message(Message::Assistant(_))
            )
        })
        .unwrap_or_else(|| panic!("Expected final assistant message"))
}

fn assistant_stop_reason(message: &pillar_agent::AgentMessage) -> StopReason {
    match message {
        pillar_agent::AgentMessage::Message(Message::Assistant(assistant)) => assistant.stop_reason,
        _ => panic!("Expected assistant message"),
    }
}

fn assistant_error_message(message: &pillar_agent::AgentMessage) -> Option<String> {
    match message {
        pillar_agent::AgentMessage::Message(Message::Assistant(assistant)) => {
            assistant.error_message.clone()
        }
        _ => None,
    }
}

// --- upstream scenario bodies ---------------------------------------------

async fn basic_prompt(model: &FauxModel, core: &Arc<FauxCore>) {
    let agent = create_agent(
        faux_stream_fn(core, model),
        "You are a helpful assistant. Keep your responses concise.",
        model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    agent
        .prompt("What is 2+2? Answer with just the number.")
        .await
        .expect("prompt");

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[0].role_name(), "user");
    assert_eq!(state.messages[1].role_name(), "assistant");
    assert!(get_text_content(&state.messages[1]).contains('4'));
}

async fn tool_execution(model: &FauxModel, core: &Arc<FauxCore>) {
    let agent = create_agent(
        faux_stream_fn(core, model),
        "You are a helpful assistant. Always use the calculator tool for math.",
        model,
        vec![calculate_tool()],
        AgentThinkingLevel::Off,
    );

    let pending_during_events = Arc::new(Mutex::new(Vec::new()));
    let agent_for_listener = Arc::clone(&agent);
    let pending_sink = Arc::clone(&pending_during_events);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let agent = Arc::clone(&agent_for_listener);
        let sink = Arc::clone(&pending_sink);
        Box::pin(async move {
            let kind = event.kind().to_owned();
            if kind == "tool_execution_start" || kind == "tool_execution_end" {
                sink.lock().expect("pending sink").push((
                    kind,
                    agent
                        .state()
                        .pending_tool_calls
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                ));
            }
        }) as ListenerFuture
    });

    agent
        .prompt("Calculate 123 * 456 using the calculator tool.")
        .await
        .expect("prompt");

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 4);
    let tool_result = state
        .messages
        .iter()
        .find(|message| message.role_name() == "toolResult")
        .expect("Expected tool result message");
    assert!(get_text_content(tool_result).contains("123 * 456 = 56088"));
    let final_message = last_assistant(&state.messages);
    assert!(get_text_content(final_message).contains("56088"));
    assert!(state.pending_tool_calls.is_empty());
    assert_eq!(
        *pending_during_events.lock().expect("pending sink"),
        vec![
            ("tool_execution_start".to_owned(), vec!["calc-1".to_owned()]),
            ("tool_execution_end".to_owned(), Vec::<String>::new()),
        ]
    );
}

async fn abort_execution(model: &FauxModel, core: &Arc<FauxCore>) {
    let agent = create_agent(
        faux_stream_fn(core, model),
        "You are a helpful assistant.",
        model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    let agent_for_abort = Arc::clone(&agent);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent_for_abort.abort();
    });

    agent
        .prompt("Count slowly from 1 to 20.")
        .await
        .expect("prompt");

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 2);
    let last_message = last_assistant(&state.messages);
    assert_eq!(assistant_stop_reason(last_message), StopReason::Aborted);
    let error_message = assistant_error_message(last_message).expect("errorMessage");
    assert_eq!(state.error_message, Some(error_message));
}

async fn state_updates(model: &FauxModel, core: &Arc<FauxCore>) {
    let agent = create_agent(
        faux_stream_fn(core, model),
        "You are a helpful assistant.",
        model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_sink = Arc::clone(&events);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let sink = Arc::clone(&events_sink);
        Box::pin(async move {
            sink.lock().expect("events sink").push(event.kind());
        }) as ListenerFuture
    });

    agent.prompt("Count from 1 to 5.").await.expect("prompt");

    let events = events.lock().expect("events sink").clone();
    for kind in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(events.contains(&kind), "missing {kind} in {events:?}");
    }
    let index_of = |kind: &str| events.iter().position(|event| *event == kind).unwrap();
    let last_index_of = |kind: &str| events.iter().rposition(|event| *event == kind).unwrap();
    assert!(index_of("agent_start") < index_of("message_start"));
    assert!(index_of("message_start") < index_of("message_end"));
    assert!(index_of("message_end") < last_index_of("agent_end"));

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
}

async fn multi_turn_conversation(model: &FauxModel, core: &Arc<FauxCore>) {
    let agent = create_agent(
        faux_stream_fn(core, model),
        "You are a helpful assistant.",
        model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    agent.prompt("My name is Alice.").await.expect("prompt");
    assert_eq!(agent.state().messages.len(), 2);

    agent.prompt("What is my name?").await.expect("prompt");
    let state = agent.state();
    assert_eq!(state.messages.len(), 4);

    let last_message = last_assistant(&state.messages);
    assert!(
        get_text_content(last_message)
            .to_lowercase()
            .contains("alice")
    );
}

#[tokio::test]
async fn handles_a_basic_text_prompt() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    core.set_responses([faux_assistant_message("4", FauxMessageOptions::default()).into()]);
    let model = core.get_model(None).expect("faux model");
    basic_prompt(&model, &core).await;
}

#[tokio::test]
async fn executes_tools_and_tracks_pending_tool_calls() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    core.set_responses([
        faux_assistant_message(
            FauxContent::Blocks(vec![
                faux_text("Let me calculate that."),
                faux_tool_call(
                    "calculate",
                    json!({ "expression": "123 * 456" }),
                    Some("calc-1"),
                ),
            ]),
            FauxMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..FauxMessageOptions::default()
            },
        )
        .into(),
        faux_assistant_message("The result is 56088.", FauxMessageOptions::default()).into(),
    ]);
    let model = core.get_model(None).expect("faux model");
    tool_execution(&model, &core).await;
}

#[tokio::test]
async fn handles_abort_during_streaming() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size_min: Some(2),
        token_size_max: Some(2),
        ..RegisterFauxProviderOptions::default()
    }));
    core.set_responses([faux_assistant_message(
        "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen",
        FauxMessageOptions::default(),
    )
    .into()]);
    let model = core.get_model(None).expect("faux model");
    abort_execution(&model, &core).await;
}

#[tokio::test]
async fn emits_lifecycle_updates_while_streaming() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions {
        token_size_min: Some(1),
        token_size_max: Some(1),
        ..RegisterFauxProviderOptions::default()
    }));
    core.set_responses([faux_assistant_message("1 2 3 4 5", FauxMessageOptions::default()).into()]);
    let model = core.get_model(None).expect("faux model");
    state_updates(&model, &core).await;
}

#[tokio::test]
async fn maintains_context_across_multiple_turns() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    let has_alice: Arc<FauxResponseFactory> = Arc::new(
        |context: &Context,
         _options: &FauxStreamOptions,
         _state: &FauxProviderState,
         _model: &FauxModel| {
            let has_alice = context.messages.iter().any(|message| match message {
                Message::User { content, .. } => user_content_text(content).contains("Alice"),
                _ => false,
            });
            faux_assistant_message(
                if has_alice {
                    "Your name is Alice."
                } else {
                    "I do not know your name."
                },
                FauxMessageOptions::default(),
            )
        },
    );
    core.set_responses([
        faux_assistant_message("Nice to meet you, Alice.", FauxMessageOptions::default()).into(),
        FauxResponseStep::Factory(has_alice),
    ]);
    let model = core.get_model(None).expect("faux model");
    multi_turn_conversation(&model, &core).await;
}

#[tokio::test]
async fn preserves_thinking_content_blocks() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions {
        models: vec![FauxModelDefinition {
            id: "faux-reasoning".to_owned(),
            reasoning: true,
            ..FauxModelDefinition::default()
        }],
        ..RegisterFauxProviderOptions::default()
    }));
    core.set_responses([faux_assistant_message(
        FauxContent::Blocks(vec![faux_thinking("step by step"), faux_text("4")]),
        FauxMessageOptions::default(),
    )
    .into()]);
    let model = core.get_model(None).expect("faux model");

    let agent = create_agent(
        faux_stream_fn(&core, &model),
        "You are a helpful assistant.",
        &model,
        Vec::new(),
        AgentThinkingLevel::Low,
    );

    agent.prompt("What is 2+2?").await.expect("prompt");

    let state = agent.state();
    let assistant = last_assistant(&state.messages);
    match assistant {
        pillar_agent::AgentMessage::Message(Message::Assistant(message)) => {
            assert_eq!(
                message.content,
                vec![Content::thinking("step by step"), Content::text("4")]
            );
        }
        other => panic!("Expected assistant message, got {other:?}"),
    }
}

// --- Agent.continue() -------------------------------------------------------

#[tokio::test]
async fn continue_throws_when_no_messages_in_context() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    let model = core.get_model(None).expect("faux model");
    let agent = create_agent(
        faux_stream_fn(&core, &model),
        "Test",
        &model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    let error = agent.continue_run().await.expect_err("should refuse");
    assert_eq!(error.0, "No messages to continue from");
}

#[tokio::test]
async fn continue_throws_when_last_message_is_assistant() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    let model = core.get_model(None).expect("faux model");
    let agent = create_agent(
        faux_stream_fn(&core, &model),
        "Test",
        &model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    agent.set_messages(vec![scripted_assistant_message(
        &model,
        vec![Content::text("Hello")],
        StopReason::Stop,
    )]);

    let error = agent.continue_run().await.expect_err("should refuse");
    assert_eq!(error.0, "Cannot continue from message role: assistant");
}

#[tokio::test]
async fn continue_from_user_message_gets_a_response() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    core.set_responses([
        faux_assistant_message("HELLO WORLD", FauxMessageOptions::default()).into(),
    ]);
    let model = core.get_model(None).expect("faux model");
    let agent = create_agent(
        faux_stream_fn(&core, &model),
        "You are a helpful assistant. Follow instructions exactly.",
        &model,
        Vec::new(),
        AgentThinkingLevel::Off,
    );

    agent.set_messages(vec![user_message("Say exactly: HELLO WORLD")]);
    agent.continue_run().await.expect("continue");

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[0].role_name(), "user");
    assert_eq!(state.messages[1].role_name(), "assistant");
    assert!(
        get_text_content(&state.messages[1])
            .to_uppercase()
            .contains("HELLO WORLD")
    );
}

#[tokio::test]
async fn continue_from_tool_result_processes_tool_results() {
    let core = Arc::new(FauxCore::new(RegisterFauxProviderOptions::default()));
    core.set_responses([
        faux_assistant_message("The answer is 8.", FauxMessageOptions::default()).into(),
    ]);
    let model = core.get_model(None).expect("faux model");
    let agent = create_agent(
        faux_stream_fn(&core, &model),
        "You are a helpful assistant. After getting a calculation result, state the answer clearly.",
        &model,
        vec![calculate_tool()],
        AgentThinkingLevel::Off,
    );

    agent.set_messages(vec![
        user_message("What is 5 + 3?"),
        scripted_assistant_message(
            &model,
            vec![
                Content::text("Let me calculate that."),
                Content::tool_call("calc-1", "calculate", json!({ "expression": "5 + 3" })),
            ],
            StopReason::ToolUse,
        ),
        scripted_tool_result("calc-1", "calculate", "5 + 3 = 8"),
    ]);

    agent.continue_run().await.expect("continue");

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 4);
    let last_message = last_assistant(&state.messages);
    assert!(get_text_content(last_message).contains('8'));
}
