//! Port of packages/agent/src/types.ts (pi v0.84.3).
//!
//! Agent runtime types: stream function contract, tool definitions, loop
//! configuration hooks, and the agent event vocabulary.
//!
//! divergence: `CustomAgentMessages` declaration merging has no Rust
//! equivalent; the four harness custom messages (harness/messages.ts) are
//! folded into the `AgentMessage` enum as `Custom` variants. The typebox
//! schema validation surfaces as JSON-Schema validation over
//! `serde_json::Value`.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, Message, SimpleStreamOptionsLike, StopReason,
    Tool, ToolResultMessage, Usage,
};

/// Stream function used by the agent loop. `Models::stream_simple` satisfies
/// this contract.
///
/// Contract:
/// - Must not panic or return an error for request/model/runtime failures.
/// - Failures are encoded in the returned stream via protocol events and a
///   final `AssistantMessage` with stopReason "error" or "aborted".
#[derive(Clone)]
pub struct StreamFn {
    f: Arc<StreamFnInner>,
}

type StreamFnInner = dyn Fn(Context, Option<StreamCallOptions>) -> StreamFuture + Send + Sync;

/// Options passed across the stream-fn boundary. Mirrors the upstream
/// `AgentLoopConfig extends SimpleStreamOptions` spread plus the caller's
/// abort signal: `BaseStreamOptions` fields the loop threads through, and
/// the simple-level additions the Agent configures.
#[derive(Clone, Default)]
pub struct StreamCallOptions {
    pub simple: SimpleStreamOptionsLike,
    pub abort: Option<crate::abort::AbortSignal>,
    /// Upstream `onPayload`, forwarded to the provider layer.
    pub on_payload: Option<pillar_ai::api::OnPayloadFn>,
    /// Upstream `onResponse`, forwarded to the provider layer.
    pub on_response: Option<pillar_ai::api::OnResponseFn>,
    /// Preferred transport for providers that support multiple transports.
    pub transport: Option<pillar_ai::types::Transport>,
    /// Custom per-level thinking token budgets.
    pub thinking_budgets: Option<pillar_ai::types::ThinkingBudgets>,
    /// Optional cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Session identifier forwarded to providers for cache-aware backends.
    pub session_id: Option<String>,
}

pub type StreamFuture =
    std::pin::Pin<Box<dyn Future<Output = pillar_ai::AssistantMessageEventStream> + Send>>;

impl StreamFn {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(Context, Option<StreamCallOptions>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = pillar_ai::AssistantMessageEventStream> + Send + 'static,
    {
        Self {
            f: Arc::new(move |context, options| Box::pin(f(context, options)) as StreamFuture),
        }
    }

    pub async fn call(
        &self,
        context: Context,
        options: Option<StreamCallOptions>,
    ) -> pillar_ai::AssistantMessageEventStream {
        (self.f)(context, options).await
    }
}

impl std::fmt::Debug for StreamFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamFn").finish()
    }
}

/// How tool calls from a single assistant message are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    /// Each tool call is prepared, executed, and finalized before the next.
    Sequential,
    /// Tool calls are prepared sequentially, then allowed tools run concurrently.
    #[default]
    Parallel,
}

/// How many queued user messages are injected at a queue drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

/// The toolCall content block from an assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl AgentToolCall {
    /// Extracts the toolCall block from an assistant content block.
    pub fn from_content(block: &pillar_ai::types::Content) -> Option<Self> {
        match block {
            pillar_ai::types::Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(Self {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        }
    }

    pub fn to_content(&self) -> pillar_ai::types::Content {
        pillar_ai::types::Content::tool_call(
            self.id.clone(),
            self.name.clone(),
            self.arguments.clone(),
        )
    }
}

/// Result returned from `before_tool_call`. `block: true` prevents execution.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    /// Hint to stop after the current tool batch (all results must set it).
    pub terminate: bool,
}

/// Partial override returned from `after_tool_call`. Field-by-field merge:
/// provided fields replace; omitted fields keep the executed result's values.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<pillar_ai::types::Content>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// Final or partial result produced by a tool.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<pillar_ai::types::Content>,
    /// Arbitrary structured details for logs or UI rendering.
    pub details: Value,
    /// Usage from the tool execution itself; not part of LLM context accounting.
    pub usage: Option<Usage>,
    /// Names of tools introduced by this result, available from this point on.
    pub added_tool_names: Option<Vec<String>>,
    /// Hint that the agent should stop after the current tool batch.
    pub terminate: bool,
}

/// Callback used by tools to stream partial execution updates. Calls after
/// the tool future settles are ignored.
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Tool definition used by the agent runtime. Extends the wire `Tool` with
/// an execute closure and execution-mode override.
#[derive(Clone)]
pub struct AgentTool {
    /// Wire-level tool identity and JSON-Schema parameters.
    pub tool: Tool,
    /// Human-readable label for UI display.
    pub label: String,
    /// Compatibility shim for raw tool-call arguments before validation.
    pub prepare_arguments: Option<Arc<PrepareArgumentsFn>>,
    /// Execute the tool call. Return `Err` on failure instead of encoding
    /// errors in `content`.
    pub execute: Arc<ToolExecuteFn>,
    /// Per-tool execution-mode override.
    pub execution_mode: Option<ToolExecutionMode>,
}

pub type ToolExecuteFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<AgentToolResult, ToolExecuteError>> + Send>>;

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool")
            .field("name", &self.tool.name)
            .field("label", &self.label)
            .field("execution_mode", &self.execution_mode)
            .finish()
    }
}

/// Argument-preparation shim type.
pub type PrepareArgumentsFn = dyn Fn(&Value) -> Value + Send + Sync;
/// Tool execute closure type.
pub type ToolExecuteFn = dyn Fn(
        String,
        Value,
        Option<crate::abort::AbortSignal>,
        Option<AgentToolUpdateCallback>,
    ) -> ToolExecuteFuture
    + Send
    + Sync;
/// Convert-to-LLM closure type.
pub type ConvertToLlmFn = dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync;
/// Context transform closure type.
pub type TransformContextFn =
    dyn Fn(Vec<AgentMessage>, Option<crate::abort::AbortSignal>) -> TransformFuture + Send + Sync;
/// API-key resolver closure type.
pub type GetApiKeyFn = dyn Fn(&str) -> ApiKeyFuture + Send + Sync;
/// Stop-after-turn closure type.
pub type ShouldStopFn = dyn Fn(&ShouldStopAfterTurnContext) -> StopFuture + Send + Sync;
/// Prepare-next-turn closure type.
pub type PrepareNextFn = dyn Fn(&ShouldStopAfterTurnContext) -> PrepareNextFuture + Send + Sync;
/// Stop-after-turn closure type carrying the active run's abort signal
/// (upstream `AgentOptions.shouldStopAfterTurn(context, signal?)`).
pub type ShouldStopWithSignalFn = dyn Fn(&ShouldStopAfterTurnContext, Option<crate::abort::AbortSignal>) -> StopFuture
    + Send
    + Sync;
/// Prepare-next-turn closure type carrying the active run's abort signal
/// (upstream `AgentOptions.prepareNextTurn(signal?)` variants).
pub type PrepareNextWithSignalFn = dyn Fn(&ShouldStopAfterTurnContext, Option<crate::abort::AbortSignal>) -> PrepareNextFuture
    + Send
    + Sync;
/// Message-poller closure type.
pub type MessagesFn = dyn Fn() -> MessagesFuture + Send + Sync;
/// Before-tool-call closure type.
pub type BeforeToolFn = dyn Fn(BeforeToolCallContext, Option<crate::abort::AbortSignal>) -> BeforeToolFuture
    + Send
    + Sync;
/// After-tool-call closure type.
pub type AfterToolFn = dyn Fn(AfterToolCallContext, Option<crate::abort::AbortSignal>) -> AfterToolFuture
    + Send
    + Sync;

/// Tool execution failure; `message` becomes the error tool result text.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolExecuteError(pub String);

impl AgentTool {
    pub fn name(&self) -> &str {
        &self.tool.name
    }
}

/// Context snapshot passed into the low-level agent loop.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// System prompt included with the request.
    pub system_prompt: String,
    /// Transcript visible to the model.
    pub messages: Vec<AgentMessage>,
    /// Tools available for this run.
    pub tools: Vec<AgentTool>,
}

/// Agent transcript entries: LLM messages + harness custom messages.
/// divergence: pi extends this union via `CustomAgentMessages` declaration
/// merging; the Rust port folds the four harness custom messages
/// (harness/messages.ts) into the enum as `Custom` variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    Message(Message),
    /// Upstream `BashExecutionMessage` (role "bashExecution").
    BashExecution(Box<BashExecutionMessage>),
    /// Upstream `CustomMessage` (role "custom").
    Custom(Box<CustomMessage>),
    /// Upstream `BranchSummaryMessage` (role "branchSummary").
    BranchSummary(Box<BranchSummaryMessage>),
    /// Upstream `CompactionSummaryMessage` (role "compactionSummary").
    CompactionSummary(Box<CompactionSummaryMessage>),
}

/// Upstream `BashExecutionMessage`: one shell command run by the harness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_from_context: bool,
}

/// Upstream `CustomMessage<T>`: an application-defined UI message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    /// Bare string or content blocks (upstream `string | (TextContent | ImageContent)[]`).
    pub content: pillar_ai::types::UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// Upstream `BranchSummaryMessage`: summary of a branch the conversation
/// returned from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// Upstream `CompactionSummaryMessage`: summary replacing compacted
/// history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

impl AgentMessage {
    pub fn role_name(&self) -> &'static str {
        match self {
            AgentMessage::Message(message) => match message {
                Message::User { .. } => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
            },
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
        }
    }

    /// The LLM-compatible message, if this entry is one (upstream narrows
    /// by role).
    pub fn as_message(&self) -> Option<&Message> {
        match self {
            AgentMessage::Message(message) => Some(message),
            _ => None,
        }
    }

    /// The LLM-compatible message; panics for custom entries. Kept for
    /// existing call sites that only handle base messages — migrate to
    /// [`as_message`](Self::as_message).
    #[track_caller]
    pub fn as_base_message(&self) -> &Message {
        self.as_message()
            .unwrap_or_else(|| panic!("as_message() on custom agent message {}", self.role_name()))
    }
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        AgentMessage::Message(message)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(assistant: AssistantMessage) -> Self {
        AgentMessage::Message(Message::Assistant(Box::new(assistant)))
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(result: ToolResultMessage) -> Self {
        AgentMessage::Message(Message::ToolResult(Box::new(result)))
    }
}

/// Events emitted by the agent loop for UI updates. `agent_end` is last.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Box<AgentMessage>,
    },
    MessageUpdate {
        message: Box<AgentMessage>,
        assistant_message_event: Box<AssistantMessageEvent>,
    },
    MessageEnd {
        message: Box<AgentMessage>,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: Value,
        is_error: bool,
    },
}

impl AgentEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}

/// Context passed to `before_tool_call`. `args` is a shared handle so an
/// in-place hook mutation (upstream mutates the object) propagates to
/// execution without revalidation.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: std::sync::Arc<std::sync::Mutex<Value>>,
}

/// Context passed to `after_tool_call`.
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: Value,
    pub result: AgentToolResult,
    pub is_error: bool,
}

/// Context passed to `should_stop_after_turn` and `prepare_next_turn`.
#[derive(Debug, Clone)]
pub struct ShouldStopAfterTurnContext {
    /// The assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool result messages passed to the preceding `turn_end`.
    pub tool_results: Vec<ToolResultMessage>,
    /// Current agent context after the turn's assistant message and tool
    /// results have been appended.
    pub context: AgentContext,
    /// Messages this loop invocation returns if it exits here.
    pub new_messages: Vec<AgentMessage>,
}

/// Replacement runtime state used before starting another provider request.
#[derive(Debug, Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Context for the next provider request.
    pub context: Option<AgentContext>,
    /// Model for the next provider request.
    pub model: Option<FauxModelRef>,
    /// Thinking level for the next provider request (`None` = off/absent).
    pub thinking_level: Option<thinking::AgentThinkingLevel>,
}

/// Lightweight model reference the loop carries between turns. The Rust
/// port keeps the model fields the loop and stream fn need; providers own
/// the full catalog metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct FauxModelRef {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: pillar_ai::types::UsageCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

impl FauxModelRef {
    /// Upstream `DEFAULT_MODEL` placeholder used when no model is configured.
    pub fn unknown() -> Self {
        Self {
            id: "unknown".into(),
            name: "unknown".into(),
            api: "unknown".into(),
            provider: "unknown".into(),
            base_url: String::new(),
            reasoning: false,
            input: Vec::new(),
            cost: pillar_ai::types::UsageCost::default(),
            context_window: 0,
            max_tokens: 0,
        }
    }
}

impl FauxModelRef {
    pub fn from_faux(model: &pillar_ai::faux::FauxModel) -> Self {
        Self {
            id: model.id.clone(),
            name: model.name.clone(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            base_url: model.base_url.clone(),
            reasoning: model.reasoning,
            input: model.input.clone(),
            cost: model.cost,
            context_window: model.context_window,
            max_tokens: model.max_tokens,
        }
    }

    /// Build the loop's lightweight model ref from a registry model
    /// (upstream carries the full `Model` in agent state; the port keeps the
    /// subset the loop and stream fn read).
    pub fn from_model(model: &pillar_ai::types::Model) -> Self {
        Self {
            id: model.id.clone(),
            name: model.name.clone(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            base_url: model.base_url.clone(),
            reasoning: model.reasoning,
            input: model.input.clone(),
            cost: pillar_ai::types::UsageCost {
                input: model.cost.rates.input,
                output: model.cost.rates.output,
                cache_read: model.cost.rates.cache_read,
                cache_write: model.cost.rates.cache_write,
                total: 0.0,
            },
            context_window: model.context_window,
            max_tokens: model.max_tokens,
        }
    }

    /// Rebuild the registry model this ref was derived from (the reverse of
    /// [`FauxModelRef::from_model`]). Fields the ref does not carry
    /// (thinking-level map, sampling params, compat, headers) are left unset.
    pub fn to_model(&self) -> pillar_ai::types::Model {
        pillar_ai::types::Model {
            id: self.id.clone(),
            name: self.name.clone(),
            api: self.api.clone(),
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            reasoning: self.reasoning,
            thinking_level_map: None,
            input: self.input.clone(),
            cost: pillar_ai::types::ModelCost {
                rates: pillar_ai::types::ModelCostRates {
                    input: self.cost.input,
                    output: self.cost.output,
                    cache_read: self.cost.cache_read,
                    cache_write: self.cost.cache_write,
                },
                tiers: None,
            },
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }
}

/// Loop configuration hooks. Closures are stored as boxed trait objects;
/// each mirrors an upstream optional hook. `None` = hook absent.
impl std::fmt::Debug for StreamCallOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCallOptions")
            .field("simple", &self.simple)
            .field("abort", &self.abort)
            .field("transport", &self.transport)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Option<FauxModelRef>,
    /// Requested reasoning level for future turns (`None` = off/absent).
    /// Authoritative over `stream_options.reasoning`; prepared by the Agent
    /// from its thinking level and updated by `prepare_next_turn` results.
    pub reasoning: Option<pillar_ai::types::ThinkingLevel>,
    /// Custom per-level thinking token budgets forwarded to the stream.
    pub thinking_budgets: Option<pillar_ai::types::ThinkingBudgets>,
    /// Optional cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Converts `AgentMessage[]` to LLM-compatible `Message[]` before each
    /// LLM call. Must not panic; return a safe fallback instead.
    pub convert_to_llm: Arc<ConvertToLlmFn>,
    /// Transform applied to the context before `convert_to_llm`.
    pub transform_context: Option<Arc<TransformContextFn>>,
    /// Resolves an API key dynamically for each LLM call.
    pub get_api_key: Option<Arc<GetApiKeyFn>>,
    /// Called after each completed turn; `true` stops the loop gracefully.
    pub should_stop_after_turn: Option<Arc<ShouldStopFn>>,
    /// Called before the next turn starts; may replace context/model/thinking.
    pub prepare_next_turn: Option<Arc<PrepareNextFn>>,
    /// Returns steering messages to inject mid-run. `[]` when none.
    pub get_steering_messages: Option<Arc<MessagesFn>>,
    /// Returns follow-up messages processed after the agent would stop.
    pub get_follow_up_messages: Option<Arc<MessagesFn>>,
    /// Tool execution mode. Default: parallel.
    pub tool_execution: Option<ToolExecutionMode>,
    /// Called before a tool executes, after argument validation.
    pub before_tool_call: Option<Arc<BeforeToolFn>>,
    /// Called after a tool finishes, before result events are emitted.
    pub after_tool_call: Option<Arc<AfterToolFn>>,
    /// Base stream options forwarded with each request.
    pub stream_options: SimpleStreamOptionsLike,
    /// Upstream `onPayload`, forwarded through stream call options.
    pub on_payload: Option<pillar_ai::api::OnPayloadFn>,
    /// Upstream `onResponse`, forwarded through stream call options.
    pub on_response: Option<pillar_ai::api::OnResponseFn>,
    /// Preferred transport forwarded through stream call options.
    pub transport: Option<pillar_ai::types::Transport>,
    /// How the run body is started. `None` = the platform default
    /// (`tokio::spawn` on native; a Wasm host must supply one, see
    /// [`crate::spawn`]).
    pub spawn: Option<crate::spawn::SpawnFn>,
}

pub type TransformFuture = std::pin::Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>>;
pub type ApiKeyFuture = std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send>>;
pub type StopFuture = std::pin::Pin<Box<dyn Future<Output = bool> + Send>>;
pub type PrepareNextFuture =
    std::pin::Pin<Box<dyn Future<Output = Option<AgentLoopTurnUpdate>> + Send>>;
pub type MessagesFuture = std::pin::Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>>;
pub type BeforeToolFuture =
    std::pin::Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>;
pub type AfterToolFuture =
    std::pin::Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>;

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            model: None,
            reasoning: None,
            thinking_budgets: None,
            max_retry_delay_ms: None,
            convert_to_llm: Self::identity_converter(),
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_options: Default::default(),
            on_payload: None,
            on_response: None,
            transport: None,
            spawn: None,
        }
    }
}

impl AgentLoopConfig {
    /// Identity converter pass-through (upstream test helper parity).
    pub fn identity_converter() -> Arc<ConvertToLlmFn> {
        Arc::new(|messages: &[AgentMessage]| {
            messages
                .iter()
                .filter_map(|m| m.as_message().cloned())
                .collect()
        })
    }

    pub fn tool_execution_mode(&self) -> ToolExecutionMode {
        self.tool_execution.unwrap_or_default()
    }
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model)
            .field("tool_execution", &self.tool_execution)
            .finish()
    }
}

/// Thinking-level vocabulary used by loop state ("off" included).
pub mod thinking {
    use pillar_ai::types::ThinkingLevel;

    /// Agent-level thinking level including "off".
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum AgentThinkingLevel {
        #[default]
        Off,
        Minimal,
        Low,
        Medium,
        High,
        Xhigh,
        Max,
    }

    impl AgentThinkingLevel {
        pub fn as_str(self) -> &'static str {
            match self {
                AgentThinkingLevel::Off => "off",
                AgentThinkingLevel::Minimal => "minimal",
                AgentThinkingLevel::Low => "low",
                AgentThinkingLevel::Medium => "medium",
                AgentThinkingLevel::High => "high",
                AgentThinkingLevel::Xhigh => "xhigh",
                AgentThinkingLevel::Max => "max",
            }
        }

        /// Upstream mapping: "off" maps to `undefined` on the loop config;
        /// every other level maps to the provider-level thinking level.
        pub fn to_thinking_level(self) -> Option<ThinkingLevel> {
            match self {
                AgentThinkingLevel::Off => None,
                AgentThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
                AgentThinkingLevel::Low => Some(ThinkingLevel::Low),
                AgentThinkingLevel::Medium => Some(ThinkingLevel::Medium),
                AgentThinkingLevel::High => Some(ThinkingLevel::High),
                AgentThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
                AgentThinkingLevel::Max => Some(ThinkingLevel::Max),
            }
        }

        /// Parse a pi thinking-level string; unknown values fall back to
        /// `off` (upstream reads the string union directly).
        pub fn parse(level: &str) -> Self {
            match level {
                "minimal" => AgentThinkingLevel::Minimal,
                "low" => AgentThinkingLevel::Low,
                "medium" => AgentThinkingLevel::Medium,
                "high" => AgentThinkingLevel::High,
                "xhigh" => AgentThinkingLevel::Xhigh,
                "max" => AgentThinkingLevel::Max,
                _ => AgentThinkingLevel::Off,
            }
        }
    }
}

/// BTreeSet alias used by validation helpers.
pub type StringSet = BTreeSet<String>;

/// Shared terminal stop-reason check used by the loop.
pub fn is_terminal_failure(stop_reason: StopReason) -> bool {
    matches!(stop_reason, StopReason::Error | StopReason::Aborted)
}
