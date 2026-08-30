//! Port of packages/agent/src/harness/agent-harness.ts (pi v0.84.3) — the
//! AgentHarness scaffold: construction over a record-free session,
//! scaffold-safe configuration accessors, and explicit rejection of every
//! unfinished public operation.
//!
//! divergence: upstream `TaggedError` classes and plain `Error`s map to
//! typed error enums (`HarnessError::NotImplemented`/`HarnessError::Closed`
//! /`HarnessError::Fault`). Upstream async methods reject with those
//! errors; the port's operations are sync functions returning
//! `Result<_, HarnessError>` (the async run plumbing lands with the
//! operation implementations). `Hooks.on`/`Events.on` return `Result`
//! instead of throwing.

use pillar_ai::retry::RetryPolicy;
use pillar_ai::types::{Model, SimpleStreamOptionsLike, Tool};

use crate::harness::compaction::compaction::{CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
use crate::harness::session::memory::{ProvisionedEntry, ProvisionedRecord, Session};
use crate::harness::types::{PromptTemplate, Skill};
use crate::types::QueueMode;
use pillar_ai::types::Usage;

/// Upstream `HarnessNotImplemented`: a scaffold operation has no
/// implementation yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessNotImplemented {
    /// Upstream operation name, e.g. `create.restore` or `prompt`.
    pub operation: String,
}

impl std::fmt::Display for HarnessNotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness.{} is not implemented yet", self.operation)
    }
}

impl std::error::Error for HarnessNotImplemented {}

/// Upstream `HarnessClosed`: the harness was closed while the operation was
/// active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessClosed;

impl std::fmt::Display for HarnessClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness was closed while the operation was active")
    }
}

impl std::error::Error for HarnessClosed {}

/// Upstream `HarnessFault`: an internal fault with a cause. The port keeps
/// the message; Rust errors carry their cause through the error chain, so
/// no separate field is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessFault {
    pub message: String,
}

impl std::fmt::Display for HarnessFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HarnessFault {}

/// The harness error vocabulary (upstream error classes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    NotImplemented(HarnessNotImplemented),
    Closed(HarnessClosed),
    Fault(HarnessFault),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented(error) => write!(f, "{error}"),
            Self::Closed(error) => write!(f, "{error}"),
            Self::Fault(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<HarnessNotImplemented> for HarnessError {
    fn from(value: HarnessNotImplemented) -> Self {
        Self::NotImplemented(value)
    }
}

impl From<HarnessClosed> for HarnessError {
    fn from(value: HarnessClosed) -> Self {
        Self::Closed(value)
    }
}

impl From<HarnessFault> for HarnessError {
    fn from(value: HarnessFault) -> Self {
        Self::Fault(value)
    }
}

/// The tag/error-name strings the typed errors carry (upstream `name`).
pub const HARNESS_NOT_IMPLEMENTED_NAME: &str = "HarnessNotImplemented";
pub const HARNESS_CLOSED_NAME: &str = "HarnessClosed";
pub const HARNESS_FAULT_NAME: &str = "HarnessFault";

/// A tool registered with the harness (upstream `HarnessTool = AgentTool &
/// { replay? }`).
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessTool {
    pub tool: Tool,
    /// `never` | `safe`. Replay policy on run restore.
    pub replay: Option<String>,
}

/// Upstream `AgentHarnessResources`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resources {
    /// Prompt templates available for explicit invocation.
    pub prompt_templates: Vec<PromptTemplate>,
    /// Skills available to the model and explicit skill invocation.
    pub skills: Vec<Skill>,
}

/// Harness construction options (upstream `AgentHarnessOptions`).
///
/// divergence: upstream `models`, `model`, and the typebox `context` fields
/// are threaded through the run plumbing; `models` and `model` are kept,
/// the telemetry context lands with telemetry. `toolContext`,
/// `systemPrompt`, `toProviderMessages`, and `entryProjectors` are async
/// callables whose plumbing starts with the operation implementations; the
/// fields exist here so the surface matches upstream.
#[derive(Clone)]
pub struct AgentHarnessOptions {
    pub models: Option<pillar_ai::models::Models>,
    /// Required upstream; the port takes it via [`AgentHarness::create`]
    /// options and keeps it here.
    pub model: Model,
    pub thinking_level: Option<crate::types::thinking::AgentThinkingLevel>,
    pub active_tool_names: Vec<String>,
    pub tools: Vec<HarnessTool>,
    pub resources: Resources,
    pub stream_options: SimpleStreamOptionsLike,
    pub retry: Option<RetryPolicy>,
    pub compaction: Option<CompactionSettings>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub tool_execution: Option<crate::types::ToolExecutionMode>,
}

/// Why an operation is suspended (upstream `SuspendedOperation.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendedReason {
    Crash,
    Deferred,
}

/// An operation that outlived its process or awaits a deferred fetch
/// (upstream `SuspendedOperation`).
#[derive(Debug, Clone, PartialEq)]
pub struct SuspendedOperation {
    pub lane: String,
    /// `run` | `compaction` | `navigation`.
    pub kind: String,
    pub id: String,
    pub started_at: u64,
    pub reason: SuspendedReason,
    pub prompt: Vec<crate::types::AgentMessage>,
    pub deferred: Option<pillar_ai::types::DeferredHandle>,
    pub aborting_steer: Vec<crate::types::AgentMessage>,
    pub aborting_follow_up: Vec<crate::types::AgentMessage>,
    pub missing_tools: Vec<String>,
    pub missing_models: Vec<String>,
}

/// The operation status visible on a lane (upstream
/// `LaneInfo.operation.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Running,
    Suspended,
    Aborting,
}

/// The operation visible on a lane (upstream `LaneInfo.operation`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneOperationInfo {
    pub id: String,
    /// `run` | `compaction` | `navigation`.
    pub kind: String,
    pub status: OperationStatus,
}

/// Lane summary (upstream `LaneInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneInfo {
    pub name: String,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
}

/// A queued message item (upstream `QueuedItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedItem {
    pub entry_id: String,
    pub message: crate::types::AgentMessage,
}

/// Per-lane snapshot (upstream `LaneSnapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneSnapshot {
    pub lane: String,
    pub transcript: Vec<crate::harness::session::types::Entry>,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
    pub queues: HarnessQueues,
    pub pending_writes: Vec<PendingWrite>,
    pub faulted: bool,
}

/// Queue snapshot (upstream `LaneSnapshot.queues`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HarnessQueues {
    pub steer: Vec<QueuedItem>,
    pub follow_up: Vec<QueuedItem>,
    pub next_run: Vec<QueuedItem>,
}

/// A pending deferred write (upstream `pendingWrites`).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWrite {
    pub id: String,
    pub entry: ProvisionedEntry,
}

/// Session-wide snapshot (upstream `SessionSnapshot`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSnapshot {
    pub lanes: Vec<(LaneInfo, Option<SuspendedOperation>)>,
    pub faulted: bool,
}

/// The agent-visible hook names (upstream `HookName`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookName {
    BeforeRun,
    BeforeResume,
    BeforeRunEnd,
    TransformContext,
    BeforeRequest,
    BeforePayload,
    AfterResponse,
    BeforeTool,
    AfterTool,
    BeforeCompaction,
    BeforeNavigation,
}

impl HookName {
    /// Upstream camelCase name string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforeRun => "before_run",
            Self::BeforeResume => "before_resume",
            Self::BeforeRunEnd => "before_run_end",
            Self::TransformContext => "transform_context",
            Self::BeforeRequest => "before_request",
            Self::BeforePayload => "before_payload",
            Self::AfterResponse => "after_response",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::BeforeCompaction => "before_compaction",
            Self::BeforeNavigation => "before_navigation",
        }
    }
}

/// Upstream `Hooks`/`Events` registries. The scaffold rejects
/// registration; real registries land with the operation implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnavailableRegistry {
    operation: &'static str,
}

impl UnavailableRegistry {
    /// Registering on the scaffold errors (upstream throws).
    pub fn on(&self, is_closed: bool) -> Result<(), HarnessError> {
        if is_closed {
            Err(HarnessClosed.into())
        } else {
            Err(HarnessNotImplemented {
                operation: self.operation.to_owned(),
            }
            .into())
        }
    }
}

/// The default steering/follow-up queue mode (upstream constructor
/// defaults to "one-at-a-time").
const DEFAULT_QUEUE_MODE: QueueMode = QueueMode::OneAtATime;

/// The agent harness scaffold (upstream `AgentHarness`).
pub struct AgentHarness {
    /// Upstream `readonly name = "main"`.
    pub name: &'static str,
    /// Upstream `session` (both the durable session and the `SessionTree`
    /// view; the port exposes the facade).
    pub session: Session,
    pub hooks: UnavailableRegistry,
    pub events: UnavailableRegistry,
    model: Model,
    thinking_level: crate::types::thinking::AgentThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<HarnessTool>,
    resources: Resources,
    stream_options: SimpleStreamOptionsLike,
    retry_policy: RetryPolicy,
    compaction_settings: CompactionSettings,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    closed: bool,
    models: Option<pillar_ai::models::Models>,
    tool_execution: Option<crate::types::ToolExecutionMode>,
}

impl AgentHarness {
    /// Upstream `AgentHarness.create`: opens only record-free sessions
    /// before restore is implemented. Returns the harness plus the list of
    /// suspended operations found at open (empty until restore exists).
    pub fn create(
        options: AgentHarnessOptions,
        session: Session,
    ) -> Result<(Self, Vec<SuspendedOperation>), HarnessError> {
        let record = session
            .find_records(&crate::harness::session::types::RecordQuery {
                limit: Some(1),
                ..Default::default()
            })
            .map_err(|error| HarnessFault {
                message: error.message,
            })?;
        if !record.is_empty() {
            return Err(HarnessNotImplemented {
                operation: "create.restore".to_owned(),
            }
            .into());
        }
        let harness = Self::new(options, session);
        Ok((harness, Vec::new()))
    }

    fn new(options: AgentHarnessOptions, session: Session) -> Self {
        let AgentHarnessOptions {
            models,
            model,
            thinking_level,
            active_tool_names,
            tools,
            resources,
            stream_options,
            retry,
            compaction,
            steering_mode,
            follow_up_mode,
            tool_execution,
        } = options;
        Self {
            name: "main",
            session,
            hooks: UnavailableRegistry {
                operation: "hooks.on",
            },
            events: UnavailableRegistry {
                operation: "events.on",
            },
            model,
            thinking_level: thinking_level
                .unwrap_or(crate::types::thinking::AgentThinkingLevel::Off),
            active_tool_names: if !active_tool_names.is_empty() {
                active_tool_names
            } else {
                tools.iter().map(|tool| tool.tool.name.clone()).collect()
            },
            tools,
            resources,
            stream_options,
            retry_policy: retry.unwrap_or(RetryPolicy {
                enabled: false,
                max_retries: 0,
                base_delay_ms: 1000,
            }),
            compaction_settings: compaction.unwrap_or(DEFAULT_COMPACTION_SETTINGS),
            steering_mode: steering_mode.unwrap_or(DEFAULT_QUEUE_MODE),
            follow_up_mode: follow_up_mode.unwrap_or(DEFAULT_QUEUE_MODE),
            closed: false,
            models,
            tool_execution,
        }
    }

    /// Upstream `unavailable<T>`: every unfinished operation rejects with
    /// `HarnessNotImplemented` (or `HarnessClosed` after close).
    fn unavailable<T>(&self, operation: &str) -> Result<T, HarnessError> {
        if self.closed {
            Err(HarnessClosed.into())
        } else {
            Err(HarnessNotImplemented {
                operation: operation.to_owned(),
            }
            .into())
        }
    }

    pub fn get_leaf_id(&self) -> Result<Option<String>, HarnessError> {
        self.durable_leaf_id()
    }

    fn durable_leaf_id(&self) -> Result<Option<String>, HarnessError> {
        self.session.get_leaf_id().map_err(|error| {
            HarnessFault {
                message: error.message,
            }
            .into()
        })
    }

    // --- Unfinished operations (upstream scaffold rejections) ------------

    pub fn prompt(&self, _input: crate::types::AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("prompt")
    }

    pub fn skill(
        &self,
        _name: &str,
        _additional_instructions: Option<&str>,
    ) -> Result<(), HarnessError> {
        self.unavailable("skill")
    }

    pub fn prompt_from_template(
        &self,
        _name: &str,
        _args: Option<&[String]>,
    ) -> Result<(), HarnessError> {
        self.unavailable("promptFromTemplate")
    }

    pub fn compact(&self, _options: Option<&str>) -> Result<(), HarnessError> {
        self.unavailable("compact")
    }

    pub fn navigate_tree(
        &self,
        _target_id: Option<&str>,
        _options: Option<NavigateOptions>,
    ) -> Result<(), HarnessError> {
        self.unavailable("navigateTree")
    }

    pub fn resume(&self) -> Result<(), HarnessError> {
        self.unavailable("resume")
    }

    pub fn abort(&self) -> Result<(), HarnessError> {
        self.unavailable("abort")
    }

    pub fn steer(&self, _input: crate::types::AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("steer")
    }

    pub fn follow_up(&self, _input: crate::types::AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("followUp")
    }

    pub fn next_run(&self, _input: crate::types::AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("nextRun")
    }

    pub fn cancel_queued(&self, _entry_id: &str) -> Result<(), HarnessError> {
        self.unavailable("cancelQueued")
    }

    pub fn record_usage(
        &self,
        _usage: Usage,
        _options: Option<RecordUsageOptions>,
    ) -> Result<(), HarnessError> {
        self.unavailable("recordUsage")
    }

    pub fn wait_for_idle(&self) -> Result<(), HarnessError> {
        self.unavailable("waitForIdle")
    }

    pub fn run_when_idle(&self, _callback: Box<dyn FnOnce() + Send>) -> Result<(), HarnessError> {
        self.unavailable("runWhenIdle")
    }

    pub fn peek_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        self.unavailable("peekAction")
    }

    pub fn execute_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        self.unavailable("executeAction")
    }

    pub fn run_to_completion(&self) -> Result<(), HarnessError> {
        self.unavailable("runToCompletion")
    }

    pub fn watch(&self) -> Result<WatchHandle<LaneSnapshot>, HarnessError> {
        self.unavailable("watch")
    }

    pub fn lane(&self, _name: &str) -> Result<Option<AgentLane>, HarnessError> {
        self.unavailable("lane")
    }

    pub fn create_lane(&self, _name: &str, _at: Option<&str>) -> Result<(), HarnessError> {
        self.unavailable("createLane")
    }

    pub fn lanes(&self) -> Result<Vec<LaneInfo>, HarnessError> {
        self.unavailable("lanes")
    }

    pub fn watch_session(&self) -> Result<WatchHandle<SessionSnapshot>, HarnessError> {
        self.unavailable("watchSession")
    }

    // --- Scaffold-safe configuration (upstream getter/setter pairs) ------

    pub fn get_model(&self) -> Model {
        self.model.clone()
    }

    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    pub fn get_thinking_level(&self) -> crate::types::thinking::AgentThinkingLevel {
        self.thinking_level
    }

    pub fn set_thinking_level(&mut self, level: crate::types::thinking::AgentThinkingLevel) {
        self.thinking_level = level;
    }

    pub fn get_active_tools(&self) -> Vec<String> {
        self.active_tool_names.clone()
    }

    pub fn set_active_tools(&mut self, names: Vec<String>) {
        self.active_tool_names = names;
    }

    pub fn get_tools(&self) -> Vec<HarnessTool> {
        self.tools.clone()
    }

    pub fn set_tools(&mut self, tools: Vec<HarnessTool>, active_names: Option<Vec<String>>) {
        self.tools = tools;
        self.active_tool_names = active_names.unwrap_or_else(|| {
            self.tools
                .iter()
                .map(|tool| tool.tool.name.clone())
                .collect()
        });
    }

    pub fn get_resources(&self) -> Resources {
        self.resources.clone()
    }

    pub fn set_resources(&mut self, resources: Resources) {
        self.resources = resources;
    }

    pub fn get_stream_options(&self) -> SimpleStreamOptionsLike {
        self.stream_options.clone()
    }

    pub fn set_stream_options(&mut self, options: SimpleStreamOptionsLike) {
        self.stream_options = options;
    }

    pub fn get_retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy;
    }

    pub fn get_compaction_settings(&self) -> CompactionSettings {
        self.compaction_settings
    }

    pub fn set_compaction_settings(&mut self, settings: CompactionSettings) {
        self.compaction_settings = settings;
    }

    pub fn get_steering_mode(&self) -> QueueMode {
        self.steering_mode
    }

    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
    }

    pub fn get_follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode
    }

    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    /// Upstream `close`: marks the harness closed. Unfinished operations
    /// report `HarnessClosed` afterwards.
    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

/// Upstream `NavigateOptions`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavigateOptions {
    pub summarize: Option<bool>,
    pub custom_instructions: Option<String>,
    pub label: Option<String>,
}

/// Upstream `recordUsage` options.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordUsageOptions {
    pub entry_id: Option<String>,
    pub details: Option<serde_json::Value>,
}

/// Upstream `WatchHandle` (scaffold: unavailable, shape fixed for callers).
pub struct WatchHandle<TSnapshot> {
    pub snapshot: TSnapshot,
}

/// Upstream `AgentLane` handle. The scaffold has no lane operations; the
/// enum keeps the surface open for `lane(name)` results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLane {
    /// The harness's own "main" lane.
    Main,
}

/// Upstream `ActionInfo`: the durable action the driver is about to take.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionInfo {
    AppendEntry {
        entry_kind: String,
        entry_id: String,
    },
    AppendRecord {
        record_kind: String,
    },
    MoveLane {
        to: Option<String>,
    },
    SetFact {
        fact: SetFact,
    },
    TryFinishRun {
        outcome: TryFinishOutcome,
    },
    FinishOperation {
        outcome: crate::harness::session::types::OperationOutcome,
    },
    CommitFollowUp,
    ConsumeQueueItem {
        queue: ConsumedQueue,
        entry_id: String,
    },
    ApplyPendingWrite {
        entry_id: String,
    },
    StreamAssistant {
        step: StreamStep,
        attempt: u32,
    },
    ExecuteTool {
        tool_call_id: String,
        tool_name: String,
    },
    FetchDeferred {
        provider: String,
        id: String,
    },
    CancelDeferred {
        provider: String,
        id: String,
    },
    Hook {
        name: HookName,
    },
    Sleep {
        delay_ms: u64,
    },
}

/// Upstream `set_fact` fact names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetFact {
    Name,
    Label,
}

/// Upstream `try_finish_run` outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryFinishOutcome {
    Completed,
    Failed,
}

/// Upstream `consume_queue_item` queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumedQueue {
    Steer,
    FollowUp,
}

/// Upstream `stream_assistant` step names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStep {
    Assistant,
    Compaction,
    BranchSummary,
}

/// Record payload used by the scaffold test (upstream `NewRecord`).
pub type NewRecord = ProvisionedRecord;

/// Unused-storage suppression for the fields kept for the run plumbing.
impl AgentHarness {
    #[allow(dead_code)] // threaded through the run plumbing when operations land
    pub(crate) fn models_ref(&self) -> Option<&pillar_ai::models::Models> {
        self.models.as_ref()
    }

    #[allow(dead_code)] // threaded through the run plumbing when operations land
    pub(crate) fn tool_execution_ref(&self) -> Option<crate::types::ToolExecutionMode> {
        self.tool_execution
    }
}

/// Upstream also keeps `ProvisionedEntry` in scope for `pendingWrites`;
/// re-export for parity greps.
#[allow(unused_imports)]
pub use crate::harness::session::memory::ProvisionedEntry as ProvisionedEntryAlias;
