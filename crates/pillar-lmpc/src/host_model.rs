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

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pillar_agent::{
    Agent, AgentOptions, AgentState, AgentToolResult, SpawnFn, StreamFn, ToolExecuteError,
};
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
    /// The guest called a host tool; the host must run it and answer.
    NeedsTool,
    /// The host cancelled the turn; the trace shows how far it got.
    Cancelled,
}

/// Identifies one published host-facing request: a model request or a tool call.
///
/// A host answer names the ticket it answers, so an answer that arrives after a
/// cancel, after the next turn began, or for a *different* parallel tool call is
/// rejected instead of being applied to whatever happens to be pending.
pub type Ticket = u64;

/// A tool call the host has to run (the engine's action).
#[derive(Debug, Clone)]
pub struct HostToolRequest {
    /// The ticket the host echoes back with
    /// [`HostModelSession::tool_result_to`].
    pub ticket: Ticket,
    /// The tool call as JSON: `{"id":…,"name":…,"arguments":…}`.
    pub call_json: String,
}

/// The host's answer to a tool call: the result JSON.
#[derive(Debug, Clone)]
pub struct HostToolOutcome {
    /// `{"content":[…],"details":…,"error":null}`; a non-null `error` becomes a
    /// tool error in the guest.
    pub result_json: String,
}

/// One guest-side model request the host has to answer.
#[derive(Debug, Clone)]
pub struct HostModelRequest {
    /// The request context as JSON (the messages the model would see).
    pub context_json: String,
}

#[derive(Default)]
struct ModelSlot {
    /// The ticket handed out most recently (0 is reserved for "none").
    last_ticket: Ticket,
    /// The request waiting for an answer, with the ticket it was published as.
    request: Option<(Ticket, HostModelRequest)>,
    /// The answer the host supplied, for the request with that ticket.
    reply: Option<(Ticket, Vec<Content>)>,
    /// The host cancelled the turn: the guest stops waiting for an answer.
    cancelled: bool,
    /// The tool calls waiting for the host, by ticket: one assistant message can
    /// call several, and they run in parallel.
    tool_requests: BTreeMap<Ticket, HostToolRequest>,
    /// Their publication order, so the host is offered the oldest one first.
    tool_order: VecDeque<Ticket>,
    /// The results the host supplied, by ticket.
    tool_results: BTreeMap<Ticket, HostToolOutcome>,
    /// The stream the published request answers into, so the host can stream
    /// partial text before its final reply.
    stream: Option<pillar_ai::event_stream::AssistantMessageEventStream>,
    /// The partial message the streamed deltas build up (upstream's
    /// `partial`).
    partial: Option<pillar_ai::AssistantMessage>,
}

impl ModelSlot {
    /// Hand out the next ticket. Tickets are not reused when a turn is
    /// cancelled or the next turn starts, so an answer for an old request can
    /// never name a new one.
    fn ticket(&mut self) -> Ticket {
        self.last_ticket += 1;
        self.last_ticket
    }

    /// The oldest tool call still waiting for its result.
    fn pending_tool(&self) -> Option<&HostToolRequest> {
        self.tool_order
            .iter()
            .find_map(|ticket| self.tool_requests.get(ticket))
    }

    /// Forget every pending request (a cancel, or the next turn).
    fn clear_pending(&mut self) {
        self.request = None;
        self.reply = None;
        self.tool_requests.clear();
        self.tool_order.clear();
        self.tool_results.clear();
        self.stream = None;
        self.partial = None;
    }
}

struct HostModelFuture {
    slot: Arc<Mutex<ModelSlot>>,
    /// The request this future answers: it only takes its own reply.
    ticket: Ticket,
}

/// Waits for the host to run a tool the guest called.
struct HostToolFuture {
    slot: Arc<Mutex<ModelSlot>>,
    /// The call this future answers: it only takes its own result, however many
    /// calls are in flight.
    ticket: Ticket,
}

impl std::future::Future for HostToolFuture {
    type Output = Result<AgentToolResult, ToolExecuteError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut slot = self.slot.lock().expect("model slot lock");
        let ticket = self.ticket;
        if let Some(outcome) = slot.tool_results.remove(&ticket) {
            return std::task::Poll::Ready(parse_tool_result(&outcome.result_json));
        }
        if slot.cancelled {
            return std::task::Poll::Ready(Err(ToolExecuteError("cancelled".to_string())));
        }
        std::task::Poll::Pending
    }
}

/// The host's tool result JSON into the guest's tool result.
fn parse_tool_result(json: &str) -> Result<AgentToolResult, ToolExecuteError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| ToolExecuteError(format!("bad tool result JSON: {error}")))?;
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return Err(ToolExecuteError(error.to_string()));
    }
    let content = value
        .get("content")
        .and_then(|content| serde_json::from_value::<Vec<Content>>(content.clone()).ok())
        .unwrap_or_default();
    Ok(AgentToolResult {
        content,
        details: value
            .get("details")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        usage: None,
        added_tool_names: None,
        terminate: value
            .get("terminate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The agent tool that hands a call to the host (an engine action).
fn host_action_tool(slot: &Arc<Mutex<ModelSlot>>) -> pillar_agent::AgentTool {
    let tool_slot = Arc::clone(slot);
    pillar_agent::AgentTool {
        tool: pillar_ai::types::Tool {
            name: HOST_ACTION_TOOL.to_string(),
            description: "Ask the host to perform an action in its world".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "do": { "type": "string" },
                    "value": { "type": "string" },
                },
            }),
            constrained_sampling: None,
        },
        label: "Host action".to_string(),
        prepare_arguments: None,
        execute: Arc::new(
            move |id: String, args: serde_json::Value, _signal, _update| {
                let slot = Arc::clone(&tool_slot);
                Box::pin(async move {
                    let ticket = {
                        let mut guard = slot.lock().expect("model slot lock");
                        if guard.cancelled {
                            return Err(ToolExecuteError("cancelled".to_string()));
                        }
                        let ticket = guard.ticket();
                        guard.tool_requests.insert(
                            ticket,
                            HostToolRequest {
                                ticket,
                                call_json: serde_json::json!({
                                    "id": id,
                                    "name": HOST_ACTION_TOOL,
                                    "arguments": args,
                                })
                                .to_string(),
                            },
                        );
                        guard.tool_order.push_back(ticket);
                        ticket
                    };
                    HostToolFuture { slot, ticket }.await
                })
            },
        ),
        execution_mode: Some(pillar_agent::ToolExecutionMode::Parallel),
    }
}

/// The name a host tool call uses in the demo scripts.
pub const HOST_ACTION_TOOL: &str = "host_action";

impl std::future::Future for HostModelFuture {
    type Output = Vec<Content>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Vec<Content>> {
        // No waker: the frame host re-polls each frame, which is when the host
        // will have answered (or cancelled).
        let mut slot = self.slot.lock().expect("model slot lock");
        let ticket = self.ticket;
        if let Some((answered, content)) = slot.reply.take() {
            if answered == ticket {
                return std::task::Poll::Ready(content);
            }
            // Not this future's answer: put it back for the request it names.
            slot.reply = Some((answered, content));
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
    /// Whether the current turn has been polled (its first request published).
    started: bool,
}

impl HostModelSession {
    /// Start a turn. The host services (spawner, clock) are pointed at `host`,
    /// so every step happens in a frame the host controls.
    pub fn start(host: &Arc<FrameHost>, prompt: &str, tools: Vec<pillar_agent::AgentTool>) -> Self {
        Self::start_with(host, prompt, tools, false)
    }

    /// [`HostModelSession::start`] with the guest's tool set extended by
    /// [`host_action_tool`], so the host runs the engine's actions.
    pub fn start_with_host_tools(
        host: &Arc<FrameHost>,
        prompt: &str,
        tools: Vec<pillar_agent::AgentTool>,
    ) -> Self {
        let mut session = Self::prepare(host, tools, true);
        session
            .begin_turn(prompt)
            .expect("a fresh session has no turn");
        session
    }

    /// Build a session with **no turn started**, so the host can restore a
    /// stored conversation before the first request is published (the ABI's
    /// create → import → start order, policy review sb39f R3).
    pub fn prepare(
        host: &Arc<FrameHost>,
        tools: Vec<pillar_agent::AgentTool>,
        host_tools: bool,
    ) -> Self {
        let mut session = Self::build(host, tools, host_tools);
        session.state = HostModelState::Running;
        session
    }

    fn start_with(
        host: &Arc<FrameHost>,
        prompt: &str,
        tools: Vec<pillar_agent::AgentTool>,
        host_tools: bool,
    ) -> Self {
        let mut session = Self::build(host, tools, host_tools);
        session
            .begin_turn(prompt)
            .expect("a fresh session has no turn");
        session
    }

    fn build(
        host: &Arc<FrameHost>,
        mut tools: Vec<pillar_agent::AgentTool>,
        host_tools: bool,
    ) -> Self {
        host.install();
        let slot = Arc::new(Mutex::new(ModelSlot::default()));
        let model_slot = Arc::clone(&slot);
        let stream_fn = StreamFn::new(move |context, _options| {
            let slot = Arc::clone(&model_slot);
            async move {
                let stream = assistant_message_event_stream();
                // A real provider opens the message before streaming into it;
                // the loop only relays partials after a `Start` (upstream the
                // same order).
                stream.push(AssistantMessageEvent::Start {
                    partial: assistant(Vec::new(), StopReason::Pending),
                });
                let context_json = serde_json::to_string(&context).unwrap_or_else(|_| "{}".into());
                let ticket = {
                    let mut guard = slot.lock().expect("model slot lock");
                    let ticket = guard.ticket();
                    guard.request = Some((ticket, HostModelRequest { context_json }));
                    guard.reply = None;
                    guard.stream = Some(stream.clone_stream());
                    guard.partial = None;
                    ticket
                };
                let content = HostModelFuture {
                    slot: Arc::clone(&slot),
                    ticket,
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

        if host_tools {
            tools.push(host_action_tool(&slot));
        }
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

        Self {
            agent,
            host: Arc::clone(host),
            slot,
            events,
            run: None,
            state: HostModelState::Running,
            error: None,
            cancelled: false,
            started: false,
        }
    }

    /// Start another turn on the same session: the agent keeps its state, so an
    /// NPC remembers the conversation (the trace then holds every turn so far).
    pub fn say(&mut self, prompt: &str) -> Result<(), String> {
        self.begin_turn(prompt)
    }

    /// Begin a turn on this session (the same as [`HostModelSession::say`]): the
    /// agent keeps its state, so an NPC remembers the conversation.
    pub fn begin_turn(&mut self, prompt: &str) -> Result<(), String> {
        if self.run.is_some() {
            return Err("the previous turn is still running".to_string());
        }
        {
            let mut slot = self.slot.lock().expect("model slot lock");
            slot.cancelled = false;
            // Tickets keep increasing across turns, so a late answer for the
            // previous turn cannot name anything pending in this one.
            slot.clear_pending();
        }
        self.cancelled = false;
        self.error = None;
        self.start_run(prompt);
        Ok(())
    }

    /// Queue the prompt future for one turn.
    fn start_run(&mut self, prompt: &str) {
        let agent = Arc::clone(&self.agent);
        let prompt = prompt.to_string();
        self.run = Some(Box::pin(async move {
            agent
                .prompt(prompt)
                .await
                .map_err(|error| error.to_string())
        }));
        self.state = HostModelState::Running;
        self.started = false;
    }

    /// Advance one frame. Returns the state the host should act on.
    pub fn poll(&mut self, frame: Duration) -> HostModelState {
        if matches!(
            self.state,
            HostModelState::Done | HostModelState::Failed | HostModelState::Cancelled
        ) {
            return self.state;
        }
        // Drive the prompt future once, then let the guest's queue run. From
        // here on the turn's context is fixed: a restore would no longer reach
        // the request that is being built.
        self.started = true;
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
        // A pending request means the guest is waiting on the host: a tool call
        // first (the model asked for an action), then the model itself.
        let (tool_pending, model_pending) = {
            let slot = self.slot.lock().expect("model slot lock");
            (slot.pending_tool().is_some(), slot.request.is_some())
        };
        self.state = if tool_pending {
            HostModelState::NeedsTool
        } else if model_pending {
            HostModelState::NeedsModel
        } else {
            HostModelState::Running
        };
        self.state
    }

    /// The tool call the host has to run (`None` unless the state is
    /// [`HostModelState::NeedsTool`]). When one assistant message calls several
    /// tools (they run in parallel), this is the oldest one still unanswered;
    /// [`HostModelSession::tool_requests`] lists them all.
    pub fn tool_request_json(&self) -> Option<String> {
        self.slot
            .lock()
            .expect("model slot lock")
            .pending_tool()
            .map(|request| request.call_json.clone())
    }

    /// The ticket of the tool call [`HostModelSession::tool_request_json`]
    /// returned: the host names it when it answers.
    pub fn tool_ticket(&self) -> Option<Ticket> {
        self.slot
            .lock()
            .expect("model slot lock")
            .pending_tool()
            .map(|request| request.ticket)
    }

    /// Every tool call waiting for the host, oldest first: `(ticket, call
    /// JSON)`. Parallel calls are answered independently and in any order.
    pub fn tool_requests(&self) -> Vec<(Ticket, String)> {
        let slot = self.slot.lock().expect("model slot lock");
        slot.tool_order
            .iter()
            .filter_map(|ticket| {
                slot.tool_requests
                    .get(ticket)
                    .map(|request| (*ticket, request.call_json.clone()))
            })
            .collect()
    }

    /// Answer the oldest pending tool call with a result JSON:
    /// `{"content":[{"type":"text","text":"…"}],"details":{}}`. A non-null
    /// `error` string becomes a tool error in the guest.
    pub fn tool_result(&self, json: &str) -> Result<(), String> {
        let ticket = self
            .tool_ticket()
            .ok_or_else(|| "no tool call is pending".to_string())?;
        self.tool_result_to(ticket, json)
    }

    /// Answer the tool call with `ticket` (the one
    /// [`HostModelSession::tool_ticket`] handed out). An answer for a call that
    /// is not pending any more — already answered, cancelled, or from an earlier
    /// turn — is rejected instead of being given to another call.
    pub fn tool_result_to(&self, ticket: Ticket, json: &str) -> Result<(), String> {
        let mut slot = self.slot.lock().expect("model slot lock");
        if slot.tool_requests.remove(&ticket).is_none() {
            return Err(format!("no tool call is pending with ticket {ticket}"));
        }
        slot.tool_order.retain(|pending| *pending != ticket);
        slot.tool_results.insert(
            ticket,
            HostToolOutcome {
                result_json: json.to_string(),
            },
        );
        Ok(())
    }

    /// Whether the guest is waiting for a host tool result.
    pub fn needs_tool(&self) -> bool {
        self.slot
            .lock()
            .expect("model slot lock")
            .pending_tool()
            .is_some()
    }

    /// The model request the host has to answer (`None` unless the state is
    /// [`HostModelState::NeedsModel`]).
    pub fn request_json(&self) -> Option<String> {
        self.slot
            .lock()
            .expect("model slot lock")
            .request
            .as_ref()
            .map(|(_, request)| request.context_json.clone())
    }

    /// The ticket of the pending model request: the host names it when it
    /// answers, so a late answer cannot be applied to a later request.
    pub fn request_ticket(&self) -> Option<Ticket> {
        self.slot
            .lock()
            .expect("model slot lock")
            .request
            .as_ref()
            .map(|(ticket, _)| *ticket)
    }

    /// Stream a partial answer for the pending request: the guest forwards it
    /// as `message_update` events, so a host can show text as it arrives and
    /// send the final message with [`HostModelSession::reply`].
    pub fn stream_delta(&self, delta: &str) -> Result<(), String> {
        let ticket = self
            .request_ticket()
            .ok_or_else(|| "no model request is pending".to_string())?;
        self.stream_delta_to(ticket, delta)
    }

    /// [`HostModelSession::stream_delta`] for the request with `ticket`: a delta
    /// for a request that is not pending any more is rejected.
    pub fn stream_delta_to(&self, ticket: Ticket, delta: &str) -> Result<(), String> {
        let mut slot = self.slot.lock().expect("model slot lock");
        match slot.request.as_ref() {
            Some((pending, _)) if *pending == ticket => {}
            Some((pending, _)) => {
                return Err(format!(
                    "the pending model request is {pending}, not {ticket}"
                ));
            }
            None => return Err("no model request is pending".to_string()),
        }
        let Some(stream) = slot.stream.as_ref().map(|stream| stream.clone_stream()) else {
            return Err("the model request has no stream".to_string());
        };
        let mut partial = slot.partial.take().unwrap_or_else(|| {
            let mut message = assistant(Vec::new(), StopReason::Pending);
            message.content.push(Content::Text {
                text: String::new(),
                text_signature: None,
            });
            message
        });
        let index = partial.content.len().saturating_sub(1);
        if let Some(Content::Text { text, .. }) = partial.content.get_mut(index) {
            text.push_str(delta);
        }
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: delta.to_string(),
            partial: partial.clone(),
        });
        slot.partial = Some(partial);
        Ok(())
    }

    /// Answer the pending request with an assistant message JSON:
    /// `{"content":[{"type":"text","text":"…"}], "stopReason":"stop"}`. The
    /// content parts are [`pillar_ai::types::Content`] values.
    pub fn reply(&self, json: &str) -> Result<(), String> {
        let ticket = self
            .request_ticket()
            .ok_or_else(|| "no model request is pending".to_string())?;
        self.reply_to(ticket, json)
    }

    /// Answer the request with `ticket` (the one
    /// [`HostModelSession::request_ticket`] handed out). An answer naming
    /// anything else — an older request, a cancelled turn, another session's
    /// request — is rejected.
    pub fn reply_to(&self, ticket: Ticket, json: &str) -> Result<(), String> {
        let content: Vec<Content> =
            serde_json::from_str(json).map_err(|error| format!("bad reply JSON: {error}"))?;
        let mut slot = self.slot.lock().expect("model slot lock");
        match slot.request.as_ref() {
            Some((pending, _)) if *pending == ticket => {}
            Some((pending, _)) => {
                return Err(format!(
                    "the pending model request is {pending}, not {ticket}"
                ));
            }
            None => return Err("no model request is pending".to_string()),
        }
        slot.request = None;
        slot.reply = Some((ticket, content));
        slot.stream = None;
        slot.partial = None;
        Ok(())
    }

    /// The conversation so far, as JSON (the messages the agent holds). A host
    /// stores this and hands it back with [`HostModelSession::restore`] to
    /// resume an NPC in a later run.
    pub fn messages_json(&self) -> Result<String, String> {
        serde_json::to_string(&self.agent.state().messages)
            .map_err(|error| format!("cannot serialize the conversation: {error}"))
    }

    /// Resume a stored conversation: the messages are installed before the turn
    /// is driven, so the next model request carries the old context *and* the new
    /// prompt.
    ///
    /// Once the turn has been polled at least once its first request is already
    /// published with the context of that moment, so a restore can no longer
    /// reach it: that is refused here rather than silently ignored (the ABI bug
    /// in policy review sb39f R3 — the host imported the stored conversation and
    /// the model still saw an empty history).
    pub fn restore(&self, json: &str) -> Result<usize, String> {
        if self.started {
            return Err(
                "the turn has already been polled: restore before driving it (or after it finishes)"
                    .to_string(),
            );
        }
        let messages: Vec<pillar_agent::AgentMessage> = serde_json::from_str(json)
            .map_err(|error| format!("bad conversation JSON: {error}"))?;
        let count = messages.len();
        self.agent.set_messages(messages);
        Ok(count)
    }

    /// Whether [`HostModelSession::restore`] is still possible (no request has
    /// been published for the current turn).
    pub fn can_restore(&self) -> bool {
        !self.started
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
            // Void every published request: the host must not answer them, and
            // an answer that arrives anyway names a ticket that is gone.
            slot.clear_pending();
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
