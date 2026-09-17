//! The LMPC minimum as an artifact: the runtime core with host-provided
//! services and no development surface.
//!
//! What this crate is for (docs/DEVELOPMENT-STRATEGY.md §4/§5-2): the profile
//! that embeds pillar in a game host. It depends on `pillar-agent` and
//! `pillar-ai` with `default-features = false`, so the build has
//! - no provider catalog (the host supplies the model as a `StreamFn`),
//! - no coding-agent scaffold (tools / session runtime / compaction / search),
//! - no terminal layer, and
//! - no extension VM.
//!
//! It also shows the two services a host has to provide, because tokio's
//! reactor and timer driver are not available on the embedding target: where
//! the loop's background body runs (`SpawnFn`) and where waiting happens
//! (`pillar_ai::set_default_sleep`). [`DemoModel`] stands in for the host's
//! model so the turn is deterministic and offline.

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
    AssistantMessageEvent, Content, StopReason, Tool, Usage, UsageCost,
};

/// What one demo turn produced: the events the host observed and the final
/// transcript. The host decides what to do with them (a game reads the answer;
/// a test asserts the shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTrace {
    /// Event kinds in the order the agent emitted them.
    pub events: Vec<String>,
    /// `(role, text)` per message in the final state.
    pub messages: Vec<(String, String)>,
}

/// Install the host services this profile needs.
///
/// Call it once at host startup. `spawn` runs the loop's background body (a
/// game frame loop, a `spawn_local` queue, a thread pool); `sleep` resolves a
/// delay (a frame timer, a `setTimeout`, or even "immediately" for a
/// deterministic demo).
pub fn install_host_services(spawn: SpawnFn, sleep: Option<pillar_ai::SleepFn>) {
    pillar_ai::set_default_sleep(sleep);
    // The runtime takes the spawner through its options, so remember it for
    // `demo_turn` and any host that builds agents through this crate.
    *HOST_SPAWN.write().expect("host spawn lock") = Some(spawn);
}

static HOST_SPAWN: std::sync::RwLock<Option<SpawnFn>> = std::sync::RwLock::new(None);

fn host_spawn() -> SpawnFn {
    HOST_SPAWN
        .read()
        .expect("host spawn lock")
        .clone()
        .unwrap_or_else(|| {
            // A usable default for a native host: drive the body on its own
            // thread with a plain futures executor (no tokio reactor).
            Arc::new(|body| {
                std::thread::spawn(move || futures::executor::block_on(body));
            })
        })
}

/// The host's model stand-in: answers the prompt, calls `remember` once, then
/// answers with the recalled value.
///
/// A real host implements `StreamFn` over its own model (in-process, a local
/// server, an engine-provided NPC brain); this one exists so the artifact can
/// be built, run, and gated offline.
pub struct DemoModel {
    calls: AtomicUsize,
    memory: Mutex<Vec<String>>,
}

impl DemoModel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            memory: Mutex::new(Vec::new()),
        })
    }

    /// The stream function this model serves.
    pub fn stream_fn(self: &Arc<Self>) -> StreamFn {
        let model = Arc::clone(self);
        StreamFn::new(move |_context, _options| {
            let model = Arc::clone(&model);
            async move {
                // Waiting goes through the host clock, never tokio's timer.
                pillar_ai::sleep(Duration::from_millis(1)).await;
                let stream = assistant_message_event_stream();
                let call = model.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    stream.push(AssistantMessageEvent::Done {
                        reason: StopReason::ToolUse,
                        message: assistant(
                            vec![Content::tool_call(
                                "remember-1",
                                "remember",
                                serde_json::json!({ "value": "the answer is 42" }),
                            )],
                            StopReason::ToolUse,
                        ),
                    });
                } else {
                    let recalled = model
                        .memory
                        .lock()
                        .expect("memory lock")
                        .last()
                        .cloned()
                        .unwrap_or_default();
                    stream.push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: assistant(vec![Content::text(recalled)], StopReason::Stop),
                    });
                }
                stream
            }
        })
    }

    fn remember(&self, value: String) {
        self.memory.lock().expect("memory lock").push(value);
    }
}

impl Default for DemoModel {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            memory: Mutex::new(Vec::new()),
        }
    }
}

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

/// The host's tool: a game-side action (narrate, move an NPC, update state).
fn remember_tool(model: Arc<DemoModel>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "remember".to_string(),
            description: "Keep a fact for the next turn".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
            }),
            constrained_sampling: None,
        },
        label: "Remember".to_string(),
        prepare_arguments: None,
        execute: Arc::new(
            move |_id: String,
                  args: serde_json::Value,
                  _signal: Option<pillar_agent::AbortSignal>,
                  _on_update: Option<pillar_agent::AgentToolUpdateCallback>|
                  -> ToolExecuteFuture {
                let model = Arc::clone(&model);
                Box::pin(async move {
                    let value = args
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    model.remember(value.clone());
                    Ok(AgentToolResult {
                        content: vec![Content::text(format!("remembered: {value}"))],
                        details: serde_json::json!({ "value": value }),
                        usage: None,
                        added_tool_names: None,
                        terminate: false,
                    })
                })
            },
        ),
        execution_mode: Some(ToolExecutionMode::Parallel),
    }
}

/// Run one demo turn and report what the host saw.
///
/// The whole path is driven by the installed host services: the spawner starts
/// the loop body and the clock serves the model's wait, so this works where
/// tokio's reactor and timer driver do not exist.
pub fn demo_turn(prompt: &str) -> Result<TurnTrace, String> {
    // A host installs its own clock; the demo resolves waits immediately so the
    // artifact runs stand-alone (a real host's clock is the frame timer).
    if pillar_ai::get_default_sleep().is_none() {
        pillar_ai::set_default_sleep(Some(Arc::new(|_| Box::pin(async {}))));
    }
    let model = DemoModel::new();
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = Arc::clone(&events);
    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentState {
            system_prompt: "You are an NPC with a memory.".to_string(),
            model: pillar_agent::FauxModelRef {
                id: "host".into(),
                name: "host".into(),
                api: "host".into(),
                provider: "host".into(),
                base_url: "local".into(),
                reasoning: false,
                input: vec!["text".into()],
                cost: UsageCost::default(),
                context_window: 8192,
                max_tokens: 2048,
            },
            tools: vec![remember_tool(Arc::clone(&model))],
            ..Default::default()
        }),
        stream_fn: Some(model.stream_fn()),
        spawn: Some(host_spawn()),
        ..AgentOptions::new(StreamFn::new(|_, _| async {
            unreachable!("the host model is installed explicitly")
        }))
    });
    // The unsubscribe handle is dropped on purpose: this turn is the listener's
    // whole lifetime.
    let _subscription = agent.subscribe(move |event, _signal| {
        recorded
            .lock()
            .expect("trace lock")
            .push(event.kind().to_string());
        Box::pin(async {})
    });

    futures::executor::block_on(agent.prompt(prompt)).map_err(|error| error.to_string())?;

    let state = agent.state();
    let messages = state
        .messages
        .iter()
        .map(|message| (message.role_name().to_string(), text_of(message)))
        .collect();
    Ok(TurnTrace {
        events: events.lock().expect("trace lock").clone(),
        messages,
    })
}

fn text_of(message: &pillar_agent::AgentMessage) -> String {
    use pillar_agent::AgentMessage;
    match message {
        AgentMessage::Message(pillar_ai::types::Message::User { content, .. }) => match content {
            pillar_ai::types::UserContent::Text(text) => text.clone(),
            pillar_ai::types::UserContent::Blocks(blocks) => {
                pillar_ai::text::content_text(blocks, " ")
            }
        },
        AgentMessage::Message(pillar_ai::types::Message::Assistant(message)) => {
            pillar_ai::text::content_text(&message.content, " ")
        }
        AgentMessage::Message(pillar_ai::types::Message::ToolResult(message)) => {
            pillar_ai::text::content_text(&message.content, " ")
        }
        other => format!("{other:?}"),
    }
}

/// The types a host needs to build its own turn, re-exported so an embedder
/// depends on this crate alone.
pub use pillar_agent::{
    AbortSignal, AgentEvent, AgentMessage, AgentToolUpdateCallback, SpawnFn as HostSpawnFn,
};
pub use pillar_ai::types::{Content as HostContent, ToolChoice as HostToolChoice};
