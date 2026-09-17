//! A host-driven model for the embedding profiles: the *host* answers the
//! model requests (an engine's NPC brain, a page's fetch, a test script).
//!
//! This is the shape a real embedder needs (docs/DEVELOPMENT-STRATEGY.md §4:
//! "host model"): the guest runs the turn, and whenever it needs a model
//! response it publishes the request and waits — no provider catalog, no HTTP,
//! no filesystem. The protocol is small enough for a C ABI:
//!
//! 1. the host starts a turn ([`HostModelSession::start`]),
//! 2. it pumps frames ([`HostModelSession::poll`]) until the session reports
//!    [`HostModelState::NeedsModel`],
//! 3. it reads the request ([`HostModelSession::request_json`]), decides, and
//!    answers ([`HostModelSession::reply`]) with an assistant message JSON,
//! 4. repeat until [`HostModelState::Done`], then read the trace
//!    ([`HostModelSession::trace`]).
//!
//! [`crate::wasm`] exposes exactly this over the C ABI, and
//! `scripts/wasm_host_model.mjs` is a JavaScript host that speaks it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pillar_agent::{Agent, AgentOptions, AgentState, SpawnFn, StreamFn};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{AssistantMessageEvent, Content, StopReason, Usage, UsageCost};

use crate::{FrameHost, TurnTrace, text_of};

/// Where a host-driven session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostModelState {
    /// The turn is running inside the guest (no host input needed).
    Running,
    /// The guest published a model request; the host must answer it.
    NeedsModel,
    /// The turn finished; the trace is ready.
    Done,
    /// The turn failed; [`HostModelSession::trace`] holds the reason.
    Failed,
    /// The host cancelled the turn; the trace shows how far it got.
    Cancelled,
}

/// One guest-side model request the host has to answer.
#[derive(Debug, Clone)]
pub struct HostModelRequest {
    /// The request context as JSON (the messages the model would see).
    pub context_json: String,
}

#[derive(Default)]
struct ModelSlot {
    /// The request waiting for an answer, if any.
    request: Option<HostModelRequest>,
    /// The answer the host supplied.
    reply: Option<Vec<Content>>,
    /// The host cancelled the turn: the guest stops waiting for an answer.
    cancelled: bool,
}

struct HostModelFuture {
    slot: Arc<Mutex<ModelSlot>>,
}

impl std::future::Future for HostModelFuture {
    type Output = Vec<Content>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Vec<Content>> {
        // No waker: the frame host re-polls each frame, which is when the host
        // will have answered (or cancelled).
        let mut slot = self.slot.lock().expect("model slot lock");
        if let Some(content) = slot.reply.take() {
            return std::task::Poll::Ready(content);
        }
        if slot.cancelled {
            // An empty answer: the run then observes the aborted signal and
            // stops instead of waiting for a model that will not answer.
            return std::task::Poll::Ready(Vec::new());
        }
        std::task::Poll::Pending
    }
}

/// The turn's prompt future, driven by the host's frames.
type RunFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// A turn whose model answers come from the host.
pub struct HostModelSession {
    agent: Arc<Agent>,
    host: Arc<FrameHost>,
    slot: Arc<Mutex<ModelSlot>>,
    events: Arc<Mutex<Vec<String>>>,
    run: Option<RunFuture>,
    state: HostModelState,
    error: Option<String>,
    cancelled: bool,
}

impl HostModelSession {
    /// Start a turn. The host services (spawner, clock) are pointed at `host`,
    /// so every step happens in a frame the host controls.
    pub fn start(host: &Arc<FrameHost>, prompt: &str, tools: Vec<pillar_agent::AgentTool>) -> Self {
        host.install();
        let slot = Arc::new(Mutex::new(ModelSlot::default()));
        let model_slot = Arc::clone(&slot);
        let stream_fn = StreamFn::new(move |context, _options| {
            let slot = Arc::clone(&model_slot);
            async move {
                let stream = assistant_message_event_stream();
                let context_json = serde_json::to_string(&context).unwrap_or_else(|_| "{}".into());
                {
                    let mut guard = slot.lock().expect("model slot lock");
                    guard.request = Some(HostModelRequest { context_json });
                    guard.reply = None;
                }
                let content = HostModelFuture {
                    slot: Arc::clone(&slot),
                }
                .await;
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
        });

        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = Arc::clone(&events);
        let spawn: SpawnFn = Arc::new({
            let host = Arc::clone(host);
            move |body| {
                // The frame host owns the queue; `spawn` is private, so hand the
                // body over through the public pump protocol.
                host.enqueue(body);
            }
        });
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: Some(AgentState {
                system_prompt: "You are an NPC whose model lives in the host.".to_string(),
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
                tools,
                ..Default::default()
            }),
            stream_fn: Some(stream_fn),
            spawn: Some(spawn),
            ..AgentOptions::new(StreamFn::new(|_, _| async {
                unreachable!("the host drives the model")
            }))
        }));
        let _subscription = agent.subscribe(move |event, _signal| {
            recorded
                .lock()
                .expect("trace lock")
                .push(event.kind().to_string());
            Box::pin(async {})
        });

        let agent_for_run = Arc::clone(&agent);
        let prompt = prompt.to_string();
        let run = Box::pin(async move {
            agent_for_run
                .prompt(prompt)
                .await
                .map_err(|error| error.to_string())
        });

        Self {
            agent,
            host: Arc::clone(host),
            slot,
            events,
            run: Some(run),
            state: HostModelState::Running,
            error: None,
            cancelled: false,
        }
    }

    /// Advance one frame. Returns the state the host should act on.
    pub fn poll(&mut self, frame: Duration) -> HostModelState {
        if matches!(
            self.state,
            HostModelState::Done | HostModelState::Failed | HostModelState::Cancelled
        ) {
            return self.state;
        }
        // Drive the prompt future once, then let the guest's queue run.
        let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        if let Some(run) = self.run.as_mut()
            && let std::task::Poll::Ready(result) = run.as_mut().poll(&mut context)
        {
            self.run = None;
            match result {
                // A cancelled run ends cleanly (the abort is not an error), but
                // the host asked to stop, so report that.
                Ok(()) => {
                    self.state = if self.cancelled {
                        HostModelState::Cancelled
                    } else {
                        HostModelState::Done
                    }
                }
                Err(error) => {
                    self.state = HostModelState::Failed;
                    self.error = Some(error);
                }
            }
        }
        self.host.pump(frame);
        if matches!(self.state, HostModelState::Done | HostModelState::Failed) {
            return self.state;
        }
        if self.cancelled {
            // The host asked to stop; the run reports the cancellation once it
            // has drained (the trace is final then).
            self.state = if self.run.is_none() {
                HostModelState::Cancelled
            } else {
                HostModelState::Running
            };
            return self.state;
        }
        // A pending request means the guest is waiting on the host.
        let waiting = self.slot.lock().expect("model slot lock").request.is_some();
        self.state = if waiting {
            HostModelState::NeedsModel
        } else {
            HostModelState::Running
        };
        self.state
    }

    /// The model request the host has to answer (`None` unless the state is
    /// [`HostModelState::NeedsModel`]).
    pub fn request_json(&self) -> Option<String> {
        self.slot
            .lock()
            .expect("model slot lock")
            .request
            .as_ref()
            .map(|request| request.context_json.clone())
    }

    /// Answer the pending request with an assistant message JSON:
    /// `{"content":[{"type":"text","text":"…"}], "stopReason":"stop"}`. The
    /// content parts are [`pillar_ai::types::Content`] values.
    pub fn reply(&self, json: &str) -> Result<(), String> {
        let content: Vec<Content> =
            serde_json::from_str(json).map_err(|error| format!("bad reply JSON: {error}"))?;
        let mut slot = self.slot.lock().expect("model slot lock");
        if slot.request.is_none() {
            return Err("no model request is pending".to_string());
        }
        slot.request = None;
        slot.reply = Some(content);
        Ok(())
    }

    /// Whether the guest is waiting for the host.
    pub fn needs_model(&self) -> bool {
        self.slot.lock().expect("model slot lock").request.is_some()
    }

    /// Cancel the running turn: stop at the next await instead of waiting for
    /// another model answer (a game cancels an NPC's turn when the scene
    /// changes). The session then reports [`HostModelState::Cancelled`].
    ///
    /// A model request that was already published is voided: the guest is no
    /// longer waiting, so [`HostModelSession::request_json`] goes back to
    /// `None` and the host should not answer.
    pub fn cancel(&mut self) {
        if matches!(
            self.state,
            HostModelState::Done | HostModelState::Failed | HostModelState::Cancelled
        ) {
            return;
        }
        self.agent.abort();
        {
            let mut slot = self.slot.lock().expect("model slot lock");
            slot.cancelled = true;
            slot.request = None;
        }
        self.cancelled = true;
    }

    /// Whether the host cancelled this session.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// The finished turn's trace (empty until [`HostModelState::Done`]).
    pub fn trace(&self) -> TurnTrace {
        let state = self.agent.state();
        TurnTrace {
            events: self.events.lock().expect("trace lock").clone(),
            messages: state
                .messages
                .iter()
                .map(|message| (message.role_name().to_string(), text_of(message)))
                .collect(),
        }
    }

    /// The failure reason, when the state is [`HostModelState::Failed`].
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
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
