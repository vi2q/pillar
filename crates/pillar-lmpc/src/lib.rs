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
use pillar_ai::types::{AssistantMessageEvent, Content, StopReason, Tool, Usage, UsageCost};

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

/// The native default host services: run each body on its own thread with a
/// plain futures executor and resolve waits immediately. A host that has a
/// frame loop uses [`FrameHost`] instead.
pub fn thread_host_services() -> (SpawnFn, pillar_ai::SleepFn) {
    // The native default keeps the system clock.
    pillar_ai::set_default_now(None);
    (
        Arc::new(|body| {
            std::thread::spawn(move || futures::executor::block_on(body));
        }),
        Arc::new(|_| Box::pin(async {})),
    )
}

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
    // The demo drives itself: install the native defaults so the artifact runs
    // stand-alone regardless of what a previous call installed (a host with a
    // frame loop uses `demo_turn_on` or builds its own agent).
    let (spawn, sleep) = thread_host_services();
    install_host_services(spawn, Some(sleep));
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

/// The trace as the host prints it: the event kinds and the transcript, one
/// line each. The Wasm export and the native binary share this so their output
/// can be compared byte for byte (§5-7).
pub fn trace_text(trace: &TurnTrace) -> String {
    let mut text = format!("events: {}\n", trace.events.join(","));
    for (role, body) in &trace.messages {
        text.push_str(&format!("{role}: {body}\n"));
    }
    text
}

#[cfg(target_arch = "wasm32")]
mod wasm;

pub mod host_model;
pub use host_model::{HostModelSession, HostModelState};

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

/// A turn whose model answers come from a scripted host: the host calls the
/// `remember` tool, then answers with what it remembered.
///
/// This is the native twin of `scripts/wasm_host_model.mjs`: both drive
/// [`HostModelSession`] with the same replies, so their traces must match
/// (§5-7) and a real host has a worked example of the protocol.
pub fn host_model_demo_turn(prompt: &str) -> Result<TurnTrace, String> {
    let host = FrameHost::new();
    let mut session = HostModelSession::start(&host, prompt, demo_tools());
    let mut requests = 0usize;
    for _ in 0..10_000 {
        match session.poll(Duration::from_millis(1)) {
            HostModelState::NeedsModel => {
                let reply = if requests == 0 {
                    r#"[{"type":"toolCall","id":"remember-1","name":"remember","arguments":{"value":"the answer is 42"}}]"#
                } else {
                    r#"[{"type":"text","text":"the answer is 42"}]"#
                };
                session.reply(reply)?;
                requests += 1;
            }
            HostModelState::Running => {}
            HostModelState::Done => return Ok(session.trace()),
            HostModelState::Cancelled => {
                return Err("the host model turn was cancelled".to_string());
            }
            HostModelState::Failed => {
                return Err(session.error().unwrap_or("the host model turn failed").to_string());
            }
        }
    }
    Err("the host model turn did not finish".to_string())
}

/// The demo turn's host tool (`remember`), for hosts that script a tool call
/// (the Wasm ABI installs the same set).
pub fn demo_tools() -> Vec<AgentTool> {
    vec![remember_tool(DemoModel::new())]
}

/// The types a host needs to build its own turn, re-exported so an embedder
/// depends on this crate alone.
pub use pillar_agent::{
    AbortSignal, AgentEvent, AgentMessage, AgentToolUpdateCallback, SpawnFn as HostSpawnFn,
};
pub use pillar_ai::types::{Content as HostContent, ToolChoice as HostToolChoice};

// ============================================================================
// A frame-driven host (no threads, no tokio)
// ============================================================================

/// A host that drives the runtime from its own loop — a game frame, a browser
/// animation frame, a test pump.
///
/// The embedding targets have no threads and no timer driver, so this host
/// keeps the loop bodies and the timers in its own queue:
/// - [`install`](FrameHost::install) points the runtime's `SpawnFn` and the
///   provider clock at this host,
/// - [`pump`](FrameHost::pump) advances its (virtual) clock and polls what it
///   has queued.
///
/// It is a deliberate "poll everything each frame" executor: no waker has to
/// cross a thread or a JavaScript boundary, which is what makes it usable from
/// a Wasm host. Native hosts use it too — a frame-driven turn is deterministic
/// (no wall clock, no thread scheduling), which is what makes its trace
/// comparable with a Wasm host's (§5-7).
pub struct FrameHost {
    tasks: Mutex<Vec<pillar_agent::spawn::Spawned>>,
    sleeps: Mutex<Vec<Arc<SleepSlot>>>,
    now: Mutex<Duration>,
    spawns: AtomicUsize,
}

struct SleepSlot {
    deadline: Duration,
    done: std::sync::atomic::AtomicBool,
}

struct SleepFuture {
    slot: Arc<SleepSlot>,
}

impl std::future::Future for SleepFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        // No waker: `pump` re-polls every queued task each frame, and it flips
        // `done` before that poll.
        if self.slot.done.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

impl FrameHost {
    /// A host with an empty queue and a clock at zero.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(Vec::new()),
            sleeps: Mutex::new(Vec::new()),
            now: Mutex::new(Duration::ZERO),
            spawns: AtomicUsize::new(0),
        })
    }

    /// Point the runtime's host services at this host.
    pub fn install(self: &Arc<Self>) {
        let spawn_host = Arc::clone(self);
        let spawn: SpawnFn = Arc::new(move |body| spawn_host.spawn(body));
        let sleep_host = Arc::clone(self);
        let sleep: pillar_ai::SleepFn = Arc::new(move |duration| sleep_host.sleep(duration));
        install_host_services(spawn, Some(sleep));
        // The frame clock is also the wall clock: a Wasm host has no system
        // clock, and a frame time makes timestamps deterministic.
        let now_host = Arc::clone(self);
        pillar_ai::set_default_now(Some(Arc::new(move || now_host.now().as_millis() as i64)));
    }

    fn spawn(&self, body: pillar_agent::spawn::Spawned) {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        self.tasks.lock().expect("frame tasks lock").push(body);
    }

    /// Queue a body for the next pump (the runtime's `SpawnFn` uses this; a
    /// host that builds its own agent can too).
    pub fn enqueue(&self, body: pillar_agent::spawn::Spawned) {
        self.spawn(body);
    }

    /// How many backgrounds bodies this host started (a host-side check that
    /// the runtime really went through it).
    pub fn spawns(&self) -> usize {
        self.spawns.load(Ordering::SeqCst)
    }

    fn sleep(&self, duration: Duration) -> pillar_ai::clock::SleepFuture {
        let deadline = *self.now.lock().expect("frame clock lock") + duration;
        self.sleeps
            .lock()
            .expect("frame sleeps lock")
            .push(Arc::new(SleepSlot {
                deadline,
                done: std::sync::atomic::AtomicBool::new(false),
            }));
        let slot = Arc::clone(
            self.sleeps
                .lock()
                .expect("frame sleeps lock")
                .last()
                .expect("just pushed"),
        );
        Box::pin(SleepFuture { slot })
    }

    /// Advance the clock by `elapsed`, resolve the timers that came due, and
    /// poll everything that can progress.
    pub fn pump(&self, elapsed: Duration) {
        {
            let mut now = self.now.lock().expect("frame clock lock");
            *now += elapsed;
            let now = *now;
            let mut sleeps = self.sleeps.lock().expect("frame sleeps lock");
            for slot in sleeps.iter() {
                if slot.deadline <= now {
                    slot.done.store(true, Ordering::SeqCst);
                }
            }
            sleeps.retain(|slot| !slot.done.load(Ordering::SeqCst));
        }
        // Poll each queued task once; repeat while a task made progress (a
        // finished await may have unblocked another task in the same frame).
        let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        loop {
            let mut progressed = false;
            let mut pending = Vec::new();
            let queued: Vec<pillar_agent::spawn::Spawned> = {
                let mut tasks = self.tasks.lock().expect("frame tasks lock");
                std::mem::take(&mut *tasks)
            };
            for mut task in queued {
                match task.as_mut().poll(&mut context) {
                    std::task::Poll::Ready(()) => progressed = true,
                    std::task::Poll::Pending => pending.push(task),
                }
            }
            *self.tasks.lock().expect("frame tasks lock") = pending;
            if !progressed {
                break;
            }
        }
    }

    /// Whether the host still has work queued (a body or a timer).
    pub fn is_idle(&self) -> bool {
        self.tasks.lock().expect("frame tasks lock").is_empty()
            && self.sleeps.lock().expect("frame sleeps lock").is_empty()
    }

    /// The host's virtual clock.
    pub fn now(&self) -> Duration {
        *self.now.lock().expect("frame clock lock")
    }
}

impl Default for FrameHost {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            sleeps: Mutex::new(Vec::new()),
            now: Mutex::new(Duration::ZERO),
            spawns: AtomicUsize::new(0),
        }
    }
}

/// [`demo_turn`] on a caller-owned host: install `host`, run the turn, and let
/// the caller pump frames until it finishes.
///
/// The returned future completes when the turn does; the caller decides how
/// much time each frame advances.
pub fn demo_turn_on(
    host: &Arc<FrameHost>,
    prompt: &str,
    frame: Duration,
) -> Result<TurnTrace, String> {
    host.install();
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
        spawn: Some(
            HOST_SPAWN
                .read()
                .expect("host spawn lock")
                .clone()
                .ok_or_else(|| "install_host_services was not called".to_string())?,
        ),
        ..AgentOptions::new(StreamFn::new(|_, _| async {
            unreachable!("the host model is installed explicitly")
        }))
    });
    let _subscription = agent.subscribe(move |event, _signal| {
        recorded
            .lock()
            .expect("trace lock")
            .push(event.kind().to_string());
        Box::pin(async {})
    });

    // Drive the turn frame by frame: the prompt future is polled by hand so the
    // host's queue is what makes progress.
    let run = std::sync::Arc::new(std::sync::Mutex::new(Some(Box::pin(agent.prompt(prompt)))));
    let mut frames = 0usize;
    loop {
        {
            let mut slot = run.lock().expect("run lock");
            if let Some(future) = slot.as_mut() {
                let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
                if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut context) {
                    result.map_err(|error| error.to_string())?;
                    *slot = None;
                }
            } else {
                break;
            }
        }
        host.pump(frame);
        frames += 1;
        if frames > 100_000 {
            return Err("the turn did not finish in 100000 frames".to_string());
        }
    }

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
