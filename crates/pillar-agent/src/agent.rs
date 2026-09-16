//! Port of packages/agent/src/agent.ts (pi v0.84.3).
//!
//! Stateful wrapper around the low-level agent loop. `Agent` owns the
//! current transcript, emits lifecycle events, executes tools, and exposes
//! queueing APIs for steering and follow-up messages.
//!
//! divergence: the JS class exposes mutable public fields and accessors
//! (`state`, `steeringMode`, ...). The Rust port uses explicit getter/setter
//! methods and interior mutability guarded by a std Mutex; hook setters are
//! plain fields. `prompt`/`continue` resolve only after all awaited
//! `agent_end` listeners settle, like upstream.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use pillar_ai::types::{Content, Message, Usage, UsageCost};

use crate::abort::AbortSignal;
use crate::agent_loop::{AgentEventSink, agent_loop, agent_loop_continue};
use crate::types::{
    AfterToolFn, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, BeforeToolFn,
    ConvertToLlmFn, GetApiKeyFn, PrepareNextFn, PrepareNextFuture, PrepareNextWithSignalFn,
    ShouldStopAfterTurnContext, ShouldStopFn, ShouldStopWithSignalFn, StopFuture, StreamFn,
    ToolExecutionMode, TransformContextFn, thinking::AgentThinkingLevel,
};

pub use crate::types::QueueMode;

fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| message.as_message().cloned())
        .collect()
}

/// Upstream `EMPTY_USAGE` used for failure messages.
#[allow(dead_code)]
fn empty_usage() -> Usage {
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

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mutable agent state (upstream `MutableAgentState`). Tools and messages
/// setters copy the provided top-level vector like upstream's `slice()`.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: crate::types::FauxModelRef,
    pub thinking_level: AgentThinkingLevel,
    pub tools: Vec<AgentTool>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: BTreeSet<String>,
    pub error_message: Option<String>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: crate::types::FauxModelRef::unknown(),
            thinking_level: AgentThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
        }
    }
}

/// Options for constructing an [`Agent`]. Hook closures mirror the upstream
/// `AgentOptions` fields; `None` = hook absent.
pub struct AgentOptions {
    pub initial_state: Option<AgentState>,
    pub convert_to_llm: Option<Arc<ConvertToLlmFn>>,
    pub transform_context: Option<Arc<TransformContextFn>>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<Arc<GetApiKeyFn>>,
    pub on_payload: Option<pillar_ai::api::OnPayloadFn>,
    pub on_response: Option<pillar_ai::api::OnResponseFn>,
    pub before_tool_call: Option<Arc<BeforeToolFn>>,
    pub after_tool_call: Option<Arc<AfterToolFn>>,
    pub should_stop_after_turn: Option<Arc<ShouldStopWithSignalFn>>,
    pub prepare_next_turn: Option<Arc<PrepareNextWithSignalFn>>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<pillar_ai::types::ThinkingBudgets>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: Option<ToolExecutionMode>,
}

impl AgentOptions {
    /// Options with just a stream function (upstream `new Agent({ streamFn })`).
    pub fn new(stream_fn: StreamFn) -> Self {
        Self {
            stream_fn: Some(stream_fn),
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            initial_state: None,
            convert_to_llm: None,
            transform_context: None,
            stream_fn: None,
            get_api_key: None,
            on_payload: None,
            on_response: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            steering_mode: None,
            follow_up_mode: None,
            session_id: None,
            thinking_budgets: None,
            max_retry_delay_ms: None,
            tool_execution: None,
        }
    }
}

/// Shared completion state for the active run (done flag + waiters).
type RunCompletion = (
    Arc<std::sync::Mutex<bool>>,
    Arc<std::sync::Mutex<Vec<std::task::Waker>>>,
);

/// One-shot run record: resolves when the run and all awaited listeners
/// have finished.
struct ActiveRun {
    /// Notified when the run fully completes (upstream `promise.resolve()`).
    done: Arc<std::sync::Mutex<bool>>,
    wakers: Arc<std::sync::Mutex<Vec<std::task::Waker>>>,
    abort: AbortSignal,
}

impl ActiveRun {
    #[allow(dead_code)]
    fn notify_done(&self) {
        let mut done = self.done.lock().expect("active run lock");
        *done = true;
        let mut wakers = self.wakers.lock().expect("active run wakers lock");
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

/// Future resolving when the active run completes.
struct WaitForIdleFuture {
    shared: Option<RunCompletion>,
}

impl std::future::Future for WaitForIdleFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let Some((done, wakers)) = &self.shared else {
            return std::task::Poll::Ready(());
        };
        if *done.lock().expect("active run lock") {
            return std::task::Poll::Ready(());
        }
        wakers
            .lock()
            .expect("active run wakers lock")
            .push(cx.waker().clone());
        std::task::Poll::Pending
    }
}

/// Queue of pending messages for steering or follow-up injection.
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => std::mem::take(&mut self.messages),
            QueueMode::OneAtATime => {
                if self.messages.is_empty() {
                    Vec::new()
                } else {
                    vec![self.messages.remove(0)]
                }
            }
        }
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Stateful wrapper around the low-level agent loop.
pub struct Agent {
    state: Arc<Mutex<AgentState>>,
    listeners: Arc<Mutex<Vec<ListenerEntry>>>,
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Arc<Mutex<Option<ActiveRun>>>,

    pub convert_to_llm: Arc<ConvertToLlmFn>,
    pub transform_context: Option<Arc<TransformContextFn>>,
    pub stream_function: Option<StreamFn>,
    pub get_api_key: Option<Arc<GetApiKeyFn>>,
    pub on_payload: Option<pillar_ai::api::OnPayloadFn>,
    pub on_response: Option<pillar_ai::api::OnResponseFn>,
    /// Tool interception hooks, interior-mutable so session runtimes can
    /// install extension interception after construction (upstream
    /// assignment on the agent instance).
    before_tool_call: Arc<std::sync::Mutex<Option<Arc<BeforeToolFn>>>>,
    after_tool_call: Arc<std::sync::Mutex<Option<Arc<AfterToolFn>>>>,
    pub should_stop_after_turn: Option<Arc<ShouldStopWithSignalFn>>,
    /// Prepare-next-turn hook, interior-mutable so session runtimes can
    /// install or replace it after construction (upstream assignment of
    /// `agent.prepareNextTurnWithContext`). Read when a run builds its loop
    /// config.
    prepare_next_turn: Arc<std::sync::Mutex<Option<Arc<PrepareNextWithSignalFn>>>>,
    /// Session identifier forwarded to providers for cache-aware backends.
    session_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Optional per-level thinking token budgets forwarded to the stream fn.
    pub thinking_budgets: Option<pillar_ai::types::ThinkingBudgets>,
    /// Optional cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool execution strategy for messages with multiple tool calls.
    pub tool_execution: ToolExecutionMode,
}

struct ListenerEntry {
    id: u64,
    f: Box<ListenerFn>,
}

static NEXT_LISTENER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type ListenerFn = dyn Fn(AgentEvent, Option<AbortSignal>) -> ListenerFuture + Send;
pub type ListenerFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Error returned by [`Agent::prompt`] and [`Agent::continue_run`] when the
/// agent is already processing (upstream throws).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AgentBusyError(pub String);

/// Error returned by [`Agent::continue_run`] when the transcript cannot be
/// continued (upstream throws).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ContinueError(pub String);

/// Error returned by [`Agent::reset`] while a run is active.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ResetError(pub String);

/// Image attachment for text prompts (upstream `ImageContent`).
pub struct PromptImage {
    /// Base64 encoded image data.
    pub data: String,
    pub mime_type: String,
}

pub enum PromptInput {
    Text {
        text: String,
        images: Vec<PromptImage>,
    },
    Message(AgentMessage),
    Messages(Vec<AgentMessage>),
}

impl Agent {
    pub fn new(mut options: AgentOptions) -> Self {
        let initial = options.initial_state.take().unwrap_or_default();
        let state = AgentState {
            system_prompt: initial.system_prompt,
            model: initial.model,
            thinking_level: initial.thinking_level,
            tools: initial.tools,
            messages: initial.messages,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
        };
        // Upstream resolves `streamFn ?? getDefaultStreamFn()` inside the
        // Agent constructor; the loop entry points apply the same fallback
        // for their own legacy callers.
        let stream_function = options.stream_fn.take();
        Self {
            state: Arc::new(Mutex::new(state)),
            listeners: Arc::new(Mutex::new(Vec::new())),
            // Upstream queues default to "one-at-a-time".
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(
                options.steering_mode.unwrap_or(QueueMode::OneAtATime),
            ))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(
                options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
            ))),
            active_run: Arc::new(Mutex::new(None)),
            convert_to_llm: options
                .convert_to_llm
                .unwrap_or_else(|| Arc::new(default_convert_to_llm)),
            transform_context: options.transform_context,
            stream_function,
            get_api_key: options.get_api_key,
            on_payload: options.on_payload,
            on_response: options.on_response,
            before_tool_call: Arc::new(std::sync::Mutex::new(options.before_tool_call)),
            after_tool_call: Arc::new(std::sync::Mutex::new(options.after_tool_call)),
            should_stop_after_turn: options.should_stop_after_turn,
            prepare_next_turn: Arc::new(std::sync::Mutex::new(options.prepare_next_turn)),
            session_id: Arc::new(std::sync::Mutex::new(options.session_id)),
            thinking_budgets: options.thinking_budgets,
            max_retry_delay_ms: options.max_retry_delay_ms,
            tool_execution: options.tool_execution.unwrap_or_default(),
        }
    }

    /// Subscribe to agent lifecycle events. Returns an unsubscribe closure
    /// (upstream `subscribe` returns a disposer). Listener futures are
    /// awaited in subscription order and are included in the current run's
    /// settlement; listeners receive the active abort signal.
    pub fn subscribe<F>(&self, listener: F) -> impl Fn() + Send + 'static
    where
        F: Fn(AgentEvent, Option<AbortSignal>) -> ListenerFuture + Send + 'static,
    {
        let next_id = {
            let mut listeners = self.listeners.lock().expect("listeners lock");
            let id = NEXT_LISTENER_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            listeners.push(ListenerEntry {
                id,
                f: Box::new(listener),
            });
            id
        };
        let listeners = Arc::clone(&self.listeners);
        move || {
            listeners
                .lock()
                .expect("listeners lock")
                .retain(|entry| entry.id != next_id);
        }
    }

    /// Snapshot of the current state.
    pub fn state(&self) -> AgentState {
        self.state.lock().expect("agent state lock").clone()
    }

    // --- State mutators -------------------------------------------------

    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) {
        self.state.lock().expect("agent state lock").system_prompt = system_prompt.into();
    }

    pub fn set_model(&self, model: crate::types::FauxModelRef) {
        self.state.lock().expect("agent state lock").model = model;
    }

    pub fn set_thinking_level(&self, level: AgentThinkingLevel) {
        self.state.lock().expect("agent state lock").thinking_level = level;
    }

    /// Replaces the tool list (copies the top-level vector).
    pub fn set_tools(&self, tools: Vec<AgentTool>) {
        self.state.lock().expect("agent state lock").tools = tools;
    }

    /// Replaces the transcript (copies the top-level vector).
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.state.lock().expect("agent state lock").messages = messages;
    }

    /// Appends a message to the transcript (upstream
    /// `state.messages.push(message)`).
    pub fn append_message(&self, message: AgentMessage) {
        self.state
            .lock()
            .expect("agent state lock")
            .messages
            .push(message);
    }

    /// Install the before-tool-call interception hook (upstream assigning
    /// `agent.beforeToolCall`). Applies to the next run.
    pub fn set_before_tool_call(&self, hook: Arc<BeforeToolFn>) {
        *self.before_tool_call.lock().expect("before hook lock") = Some(hook);
    }

    /// The installed before-tool-call hook (upstream reads it to chain a new
    /// hook onto the previous one).
    pub fn before_tool_call_hook(&self) -> Option<Arc<BeforeToolFn>> {
        self.before_tool_call
            .lock()
            .expect("before hook lock")
            .clone()
    }

    /// Remove the installed tool-call and next-turn hooks: a disposed session
    /// must not be called back by a later event (upstream session disposal).
    pub fn clear_agent_hooks(&self) {
        *self.before_tool_call.lock().expect("before hook lock") = None;
        *self.after_tool_call.lock().expect("after hook lock") = None;
        *self.prepare_next_turn.lock().expect("prepare hook lock") = None;
    }

    /// Install the after-tool-call interception hook (upstream assigning
    /// `agent.afterToolCall`). Applies to the next run.
    pub fn set_after_tool_call(&self, hook: Arc<AfterToolFn>) {
        *self.after_tool_call.lock().expect("after hook lock") = Some(hook);
    }

    /// The installed prepare-next-turn hook (upstream reading
    /// `agent.prepareNextTurnWithContext`). Used by session runtimes that
    /// chain their own compaction refresh on top of a previously installed
    /// hook.
    pub fn prepare_next_turn_hook(&self) -> Option<Arc<PrepareNextWithSignalFn>> {
        self.prepare_next_turn
            .lock()
            .expect("prepare next turn lock")
            .clone()
    }

    /// Install or replace the prepare-next-turn hook (upstream assigning
    /// `agent.prepareNextTurnWithContext`). Applies to the next run.
    pub fn set_prepare_next_turn(&self, hook: Option<Arc<PrepareNextWithSignalFn>>) {
        *self
            .prepare_next_turn
            .lock()
            .expect("prepare next turn lock") = hook;
    }

    pub fn clear_messages(&self) {
        self.state.lock().expect("agent state lock").messages = Vec::new();
    }

    /// Sets the session identifier forwarded to providers (upstream
    /// `agent.sessionId = ...`).
    pub fn set_session_id(&self, session_id: impl Into<String>) {
        *self.session_id.lock().expect("session id lock") = Some(session_id.into());
    }

    /// Current session identifier.
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().expect("session id lock").clone()
    }

    // --- Queue API ------------------------------------------------------

    /// Controls how queued steering messages are drained.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.steering_queue.lock().expect("queue lock").mode = mode;
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.steering_queue.lock().expect("queue lock").mode
    }

    /// Controls how queued follow-up messages are drained.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.follow_up_queue.lock().expect("queue lock").mode = mode;
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_queue.lock().expect("queue lock").mode
    }

    /// Queue a message to be injected after the current assistant turn
    /// finishes.
    pub fn steer(&self, message: AgentMessage) {
        self.steering_queue
            .lock()
            .expect("queue lock")
            .enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up_queue
            .lock()
            .expect("queue lock")
            .enqueue(message);
    }

    pub fn clear_steering_queue(&self) {
        self.steering_queue.lock().expect("queue lock").clear();
    }

    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue.lock().expect("queue lock").clear();
    }

    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Returns true when either queue still contains pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.lock().expect("queue lock").has_items()
            || self.follow_up_queue.lock().expect("queue lock").has_items()
    }

    // --- Run lifecycle --------------------------------------------------

    /// Active abort signal for the current run, if any.
    pub fn signal(&self) -> Option<AbortSignal> {
        self.active_run
            .lock()
            .expect("active run lock")
            .as_ref()
            .map(|run| run.abort.clone())
    }

    /// Abort the current run, if one is active.
    pub fn abort(&self) {
        if let Some(run) = self.active_run.lock().expect("active run lock").as_ref() {
            run.abort.abort();
        }
    }

    /// The active run's abort signal, if a run is in flight (upstream the
    /// context's `signal` / `run.abort`).
    pub fn abort_signal(&self) -> Option<crate::abort::AbortSignal> {
        self.active_run
            .lock()
            .expect("active run lock")
            .as_ref()
            .map(|run| run.abort.clone())
    }

    /// Resolves when the current run and all awaited event listeners have
    /// finished (after `agent_end` listeners settle).
    pub async fn wait_for_idle(&self) {
        let shared = {
            let active = self.active_run.lock().expect("active run lock");
            match active.as_ref() {
                Some(run) => Some((Arc::clone(&run.done), Arc::clone(&run.wakers))),
                None => None,
            }
        };
        WaitForIdleFuture { shared }.await;
    }

    /// Clear transcript state, runtime state, and queued messages.
    pub fn reset(&self) -> Result<(), ResetError> {
        if self.active_run.lock().expect("active run lock").is_some() {
            return Err(ResetError(
                "Agent is already processing. Wait for completion before resetting.".into(),
            ));
        }

        let mut state = self.state.lock().expect("agent state lock");
        state.messages = Vec::new();
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls = BTreeSet::new();
        state.error_message = None;
        drop(state);
        self.clear_follow_up_queue();
        self.clear_steering_queue();
        Ok(())
    }

    /// Start a new prompt from text (optionally with images), a single
    /// message, or a batch of messages.
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<(), AgentBusyError> {
        if self.active_run.lock().expect("active run lock").is_some() {
            return Err(AgentBusyError(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion.".into(),
            ));
        }
        let messages: Vec<AgentMessage> = input.into().into();
        self.run_prompt_messages(messages, false).await;
        Ok(())
    }

    /// Continue from the current transcript. The last message must be a
    /// user or tool-result message.
    pub async fn continue_run(&self) -> Result<(), ContinueError> {
        if self.active_run.lock().expect("active run lock").is_some() {
            return Err(ContinueError(
                "Agent is already processing. Wait for completion before continuing.".into(),
            ));
        }

        let last_role = self
            .state
            .lock()
            .expect("agent state lock")
            .messages
            .last()
            .map(|message| message.role_name().to_owned());
        let Some(last_role) = last_role else {
            return Err(ContinueError("No messages to continue from".into()));
        };

        if last_role == "assistant" {
            let queued_steering = self.steering_queue.lock().expect("queue lock").drain();
            if !queued_steering.is_empty() {
                self.run_prompt_messages(queued_steering, true).await;
                return Ok(());
            }

            let queued_follow_ups = self.follow_up_queue.lock().expect("queue lock").drain();
            if !queued_follow_ups.is_empty() {
                self.run_prompt_messages(queued_follow_ups, false).await;
                return Ok(());
            }

            return Err(ContinueError(
                "Cannot continue from message role: assistant".into(),
            ));
        }

        self.run_continuation().await;
        Ok(())
    }

    // --- Internals --------------------------------------------------------

    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    ) {
        let context = self.create_context_snapshot();
        let config = self.create_loop_config(skip_initial_steering_poll);
        let stream_fn = self.stream_function.clone();

        self.run_with_lifecycle(move |signal, emit| async move {
            let stream = agent_loop(messages, context, config, signal, stream_fn);
            drive_agent_stream(stream, emit).await;
        })
        .await;
    }

    async fn run_continuation(&self) {
        let context = self.create_context_snapshot();
        let config = self.create_loop_config(false);
        let stream_fn = self.stream_function.clone();

        self.run_with_lifecycle(move |signal, emit| async move {
            // Continuation validation errors were already produced
            // synchronously by `continue_run`; the loop call here cannot
            // fail (upstream would throw before lifecycle setup too).
            if let Ok(stream) = agent_loop_continue(context, config, signal, stream_fn) {
                drive_agent_stream(stream, emit).await;
            }
        })
        .await;
    }

    fn create_context_snapshot(&self) -> AgentContext {
        let state = self.state.lock().expect("agent state lock");
        AgentContext {
            system_prompt: state.system_prompt.clone(),
            messages: state.messages.clone(),
            tools: state.tools.clone(),
        }
    }

    fn create_loop_config(&self, skip_initial_steering_poll: bool) -> AgentLoopConfig {
        let state = self.state.lock().expect("agent state lock");
        let steering_queue = Arc::clone(&self.steering_queue);
        let follow_up_queue = Arc::clone(&self.follow_up_queue);
        let skip = std::sync::atomic::AtomicBool::new(skip_initial_steering_poll);
        let skip = Arc::new(skip);

        // Upstream binds `this.signal` into the signal-taking AgentOptions
        // hooks when building the loop config. The active run is looked up
        // at call time, matching upstream reading `this.signal` lazily.
        let should_stop_after_turn = self.should_stop_after_turn.as_ref().map(|hook| {
            let active = Arc::clone(&self.active_run);
            let hook = Arc::clone(hook);
            Arc::new(move |context: &ShouldStopAfterTurnContext| -> StopFuture {
                let hook = Arc::clone(&hook);
                let active = Arc::clone(&active);
                let context = context.clone();
                Box::pin(async move {
                    let signal = active
                        .lock()
                        .expect("active run lock")
                        .as_ref()
                        .map(|run| run.abort.clone());
                    hook(&context, signal).await
                }) as StopFuture
            }) as Arc<ShouldStopFn>
        });
        let prepare_next_turn_hook = self
            .prepare_next_turn
            .lock()
            .expect("prepare next turn lock")
            .clone();
        let prepare_next_turn = prepare_next_turn_hook.as_ref().map(|hook| {
            let active = Arc::clone(&self.active_run);
            let hook = Arc::clone(hook);
            Arc::new(
                move |context: &ShouldStopAfterTurnContext| -> PrepareNextFuture {
                    let hook = Arc::clone(&hook);
                    let active = Arc::clone(&active);
                    let context = context.clone();
                    Box::pin(async move {
                        let signal = active
                            .lock()
                            .expect("active run lock")
                            .as_ref()
                            .map(|run| run.abort.clone());
                        hook(&context, signal).await
                    }) as PrepareNextFuture
                },
            ) as Arc<PrepareNextFn>
        });

        AgentLoopConfig {
            model: Some(state.model.clone()),
            reasoning: state.thinking_level.to_thinking_level(),
            thinking_budgets: self.thinking_budgets,
            max_retry_delay_ms: self.max_retry_delay_ms,
            convert_to_llm: Arc::clone(&self.convert_to_llm),
            transform_context: self.transform_context.clone(),
            get_api_key: self.get_api_key.clone(),
            should_stop_after_turn,
            prepare_next_turn,
            get_steering_messages: Some(Arc::new(move || {
                let queue = Arc::clone(&steering_queue);
                let skip = Arc::clone(&skip);
                Box::pin(async move {
                    if skip
                        .compare_exchange(
                            true,
                            false,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return Vec::new();
                    }
                    queue.lock().expect("queue lock").drain()
                })
            })),
            get_follow_up_messages: Some(Arc::new(move || {
                let queue = Arc::clone(&follow_up_queue);
                Box::pin(async move { queue.lock().expect("queue lock").drain() })
            })),
            tool_execution: Some(self.tool_execution),
            before_tool_call: self
                .before_tool_call
                .lock()
                .expect("before hook lock")
                .clone(),
            after_tool_call: self
                .after_tool_call
                .lock()
                .expect("after hook lock")
                .clone(),
            // Read at config-build time (each prompt/continue run); upstream
            // spreads `this.sessionId` the same way, so setter changes apply
            // to the next run.
            stream_options: pillar_ai::types::SimpleStreamOptionsLike {
                session_id: self.session_id.lock().expect("session id lock").clone(),
                ..Default::default()
            },
            on_payload: self.on_payload.clone(),
            on_response: self.on_response.clone(),
            transport: None,
        }
    }

    /// Drive one run with the lifecycle wrapper: sets streaming state, wires
    /// event processing + listener dispatch, and clears runtime state after.
    async fn run_with_lifecycle<F, Fut>(&self, executor: F)
    where
        F: FnOnce(Option<AbortSignal>, AgentEventSink) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        // runWithLifecycle throws when a run is already active; callers
        // check first, so the error path is unreachable here.
        if self.active_run.lock().expect("active run lock").is_some() {
            return;
        }

        let abort = AbortSignal::new();
        let done = Arc::new(std::sync::Mutex::new(false));
        let wakers = Arc::new(std::sync::Mutex::new(Vec::new()));
        self.active_run
            .lock()
            .expect("active run lock")
            .replace(ActiveRun {
                done: Arc::clone(&done),
                wakers: Arc::clone(&wakers),
                abort: abort.clone(),
            });

        {
            let mut state = self.state.lock().expect("agent state lock");
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        }

        // Event processing: reduce state for a loop event, then await
        // listeners (upstream `processEvents`).
        let state_for_sink = Arc::clone(&self.state);
        let listeners_for_sink = Arc::clone(&self.listeners);
        let active_for_sink = Arc::clone(&self.active_run);
        let sink = AgentEventSink::new(move |event| {
            let state = Arc::clone(&state_for_sink);
            let listeners = Arc::clone(&listeners_for_sink);
            let active = Arc::clone(&active_for_sink);
            Box::pin(async move {
                process_event(&state, &listeners, &active, event).await;
            })
        });

        executor(Some(abort.clone()), sink).await;

        self.finish_run(done, wakers);
    }

    fn finish_run(
        &self,
        done: Arc<std::sync::Mutex<bool>>,
        wakers: Arc<std::sync::Mutex<Vec<std::task::Waker>>>,
    ) {
        {
            let mut state = self.state.lock().expect("agent state lock");
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = BTreeSet::new();
        }
        self.active_run.lock().expect("active run lock").take();
        let mut done = done.lock().expect("active run lock");
        *done = true;
        drop(done);
        let mut wakers = wakers.lock().expect("active run wakers lock");
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

impl From<PromptInput> for Vec<AgentMessage> {
    fn from(input: PromptInput) -> Self {
        match input {
            PromptInput::Messages(messages) => messages,
            PromptInput::Message(message) => vec![message],
            PromptInput::Text { text, images } => {
                let mut content: Vec<Content> = vec![Content::text(text)];
                for image in images {
                    content.push(Content::Image {
                        data: image.data,
                        mime_type: image.mime_type,
                    });
                }
                vec![
                    Message::User {
                        content: pillar_ai::types::UserContent::Blocks(content),
                        timestamp: now_millis(),
                    }
                    .into(),
                ]
            }
        }
    }
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        PromptInput::Text {
            text: text.to_owned(),
            images: Vec::new(),
        }
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        PromptInput::Text {
            text,
            images: Vec::new(),
        }
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(message: AgentMessage) -> Self {
        PromptInput::Message(message)
    }
}

impl From<Vec<AgentMessage>> for PromptInput {
    fn from(messages: Vec<AgentMessage>) -> Self {
        PromptInput::Messages(messages)
    }
}

/// Reduce state for a loop event, then await listeners.
async fn process_event(
    state: &Arc<Mutex<AgentState>>,
    listeners: &Arc<Mutex<Vec<ListenerEntry>>>,
    active: &Arc<Mutex<Option<ActiveRun>>>,
    event: AgentEvent,
) {
    // Reduce internal state first.
    {
        let mut agent_state = state.lock().expect("agent state lock");
        match &event {
            AgentEvent::MessageStart { message } => {
                agent_state.streaming_message = Some((**message).clone());
            }
            AgentEvent::MessageUpdate { message, .. } => {
                agent_state.streaming_message = Some((**message).clone());
            }
            AgentEvent::MessageEnd { message } => {
                agent_state.streaming_message = None;
                agent_state.messages.push((**message).clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                agent_state.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                agent_state.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Message(Message::Assistant(assistant)) = message.as_ref() {
                    if let Some(error) = &assistant.error_message {
                        agent_state.error_message = Some(error.clone());
                    }
                }
            }
            AgentEvent::AgentEnd { .. } => {
                agent_state.streaming_message = None;
            }
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::ToolExecutionUpdate { .. } => {}
        }
    }

    // Listener dispatch requires an active run signal.
    let signal = active
        .lock()
        .expect("active run lock")
        .as_ref()
        .map(|run| run.abort.clone());
    let Some(signal) = signal else {
        // Upstream throws "Agent listener invoked outside active run";
        // finish_run clears the active run only after all events, so this
        // is unreachable in practice.
        return;
    };
    let listener_list: Vec<ListenerEntry> = {
        let mut guard = listeners.lock().expect("listeners lock");
        std::mem::take(&mut *guard)
    };
    for entry in listener_list {
        (entry.f)(event.clone(), Some(signal.clone())).await;
        listeners.lock().expect("listeners lock").push(entry);
    }
}

/// Drain an [`AgentStream`] through the agent's event processor. The loop's
/// spawned task pushes events; this awaits them in order (upstream awaits
/// `emit(...)` inline inside the loop). Returns once the stream ends at
/// `agent_end`.
async fn drive_agent_stream(stream: crate::agent_loop::AgentStream, emit: AgentEventSink) {
    let mut iter = stream.iter();
    while let Some(event) = futures::StreamExt::next(&mut iter).await {
        emit.emit(event).await;
    }
    // Stream ended (agent_end resolved the result); nothing further to do.
    let _ = stream.result().await;
}
