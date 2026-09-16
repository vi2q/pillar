//! Port of the `AgentSession` class of
//! packages/coding-agent/src/core/agent-session.ts (pi v0.84.3): the
//! streaming agent-run loop shared by all run modes. Owns agent event
//! dispatch to the extension runner and session listeners, session
//! persistence, the prompt/steer/followUp flow with auth validation,
//! auto-retry with exponential backoff, and automatic/manual compaction
//! orchestration.
//!
//! divergences:
//! - The tool registry (base + extension tool definitions with prompt
//!   snippets) is host-owned; the session uses the agent's current system
//!   prompt as its base prompt.
//! - The `preflightResult` callback is host-specific and not ported.
//! - Summarization routes through the agent's stream function with the
//!   `SimpleStreamOptions` subset; provider headers/env beyond apiKey ride
//!   the model boundary as in the agent loop.
//! - Extension command handlers are host-injected
//!   (`AgentSessionConfig::command_handler`); upstream executes them via
//!   `command.handler(args, ctx)`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::types::{AfterToolFuture, BeforeToolFuture};
use pillar_ai::abort::AbortSignal;
use pillar_ai::models::AuthTarget;
use pillar_ai::text::content_text;
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Message, Model, StopReason, ThinkingLevel,
    ToolResultMessage, UserContent,
};
use serde_json::Value;

use crate::core::agent_session::{
    ContextUsage, CustomDelivery, CustomMessagePlan, compute_context_usage, plan_custom_message,
    plan_tree_navigation, will_retry_after_agent_end,
};
use crate::core::auth_guidance::{
    format_no_api_key_found_message, format_no_model_selected_message,
};
use crate::core::bash_executor::{BashResult, CallbackSink};
use crate::core::compaction::auto_driver::{self, AutoReason, CompactionDecision};
use crate::core::compaction::branch_summarization::{
    BranchSummaryDetails, GenerateBranchSummaryOptions, collect_entries_for_branch_summary,
    generate_branch_summary,
};
use crate::core::compaction::driver as compaction_driver;
use crate::core::compaction::driver::{
    CompactionPreparation, CompactionResult, SummarizationOptions, SummarizeFn, compact,
    estimate_tokens, prepare_compaction,
};
use crate::core::export_html::{SessionData, generate_html, write_export};
use crate::core::extensions_runner::{
    ExtensionError, ExtensionRunner, ResolvedCommand, emit_session_shutdown_event,
};
use crate::core::messages::{
    BashExecutionMessage, CodingAgentMessage, CustomContent, CustomMessage,
};
use crate::core::model_mutation::{
    CycleDirection, ModelMutations, ModelSwitchOutcome, MutationEvent, ScopedModel,
    TranscriptAppends,
};
use crate::core::model_runtime::ModelRuntime;
use crate::core::package_manager::{PathMetadata, ResourceOrigin, SourceScope};
use crate::core::prompt_templates::{PromptTemplate, expand_prompt_template};
use crate::core::resource_loader::{
    LoadedPrompt, LoadedSkill, ResourceExtensionPaths, ResourceLoader,
};
use crate::core::session_entries::SessionEntry;
use crate::core::session_manager::{SessionManager, session_entry_to_context_messages};
use crate::core::settings_manager::SettingsManager;

/// Options for [`AgentSession::navigate_tree`] (upstream the
/// `navigateTree` options object).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeNavigationOptions {
    /// Whether to summarize the branch being left.
    pub summarize: bool,
    /// Custom instructions for the branch summarizer.
    pub custom_instructions: Option<String>,
    /// If true, custom instructions replace the default prompt.
    pub replace_instructions: Option<bool>,
    /// Label to attach to the branch summary entry (or the target entry when
    /// no summary is created).
    pub label: Option<String>,
}

/// Result of [`AgentSession::navigate_tree`] (upstream the resolve value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeNavigationResult {
    /// Text to place in the editor when the target is a user/custom message.
    pub editor_text: Option<String>,
    /// True when the navigation was cancelled (extension cancel).
    pub cancelled: bool,
    /// True when a branch summarization was aborted.
    pub aborted: bool,
    /// The id of the created branch summary entry, if any.
    pub summary_entry_id: Option<String>,
}

/// Session-level events (upstream the `AgentSessionEvent` union; agent
/// events pass through with `agent_end` enriched with `will_retry`).
#[derive(Debug, Clone)]
pub enum AgentSessionEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<pillar_agent::types::AgentMessage>,
        will_retry: bool,
    },
    TurnStart,
    TurnEnd {
        message: pillar_agent::types::AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: pillar_agent::types::AgentMessage,
    },
    MessageUpdate {
        message: pillar_agent::types::AgentMessage,
        assistant_message_event: Box<AssistantMessageEvent>,
    },
    MessageEnd {
        message: pillar_agent::types::AgentMessage,
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
    /// Upstream `{ type: "agent_settled" }`.
    AgentSettled,
    /// Upstream `{ type: "queue_update" }`.
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    /// Upstream `{ type: "thinking_level_changed" }`.
    ThinkingLevelChanged {
        level: String,
    },
    /// Upstream `{ type: "compaction_start" }`.
    CompactionStart {
        reason: &'static str,
    },
    /// Upstream `{ type: "entry_appended" }`.
    EntryAppended {
        entry: SessionEntry,
    },
    /// Upstream `{ type: "session_info_changed" }`.
    SessionInfoChanged {
        name: Option<String>,
    },
    CompactionEnd {
        reason: &'static str,
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart {
        source: SummarizationRetrySource,
    },
    SummarizationRetryFinished,
    BashExecutionUpdate {
        id: Option<String>,
        delta: String,
    },
}

/// Source of a summarization retry (upstream the `source` union).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizationRetrySource {
    BranchSummary,
    Compaction { reason: &'static str },
}

/// Options for [`AgentSession::prompt`] (upstream `PromptOptions`; the
/// `preflightResult` callback is host-specific and not ported).
#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    /// Expand `/command` extension dispatch, `/skill:` and `/template`
    /// expansion. Default true.
    pub expand_prompt_templates: Option<bool>,
    /// Streaming delivery mode; required when the agent is streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Image blocks to attach to the user message.
    pub images: Option<Vec<Content>>,
    /// Input source reported to `input` extension handlers. Default
    /// "interactive".
    pub source: Option<String>,
}

/// How a message is queued while the agent is streaming (upstream
/// `"steer" | "followUp"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

impl StreamingBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// Options for [`AgentSession::send_user_message`] (upstream
/// `SendUserMessageOptions`).
#[derive(Debug, Clone, Default)]
pub struct SendUserMessageOptions {
    pub deliver_as: Option<StreamingBehavior>,
    pub expand_prompt_templates: Option<bool>,
}

/// Options for [`AgentSession::send_custom_message`] (upstream the
/// `sendCustomMessage` options).
#[derive(Debug, Clone, Default)]
pub struct SendCustomMessageOptions {
    pub trigger_turn: Option<bool>,
    pub deliver_as: Option<CustomDelivery>,
}

/// Session-start metadata for extension binding (upstream
/// `SessionStartEvent`).
#[derive(Debug, Clone)]
pub struct SessionEventMeta {
    pub reason: String,
    pub previous_session_file: Option<String>,
}

/// Host hook that executes an extension command (`/name args`); returns
/// true when handled. Upstream `command.handler(args, ctx)` — the port
/// keeps command contexts host-owned.
pub type ExtensionCommandHandler = Arc<dyn Fn(&str, &str) -> Result<bool, String> + Send + Sync>;

/// Host hook that rebuilds the base system prompt from the active tool names
/// (upstream `_rebuildSystemPrompt`; the tool registry and prompt options are
/// host-owned in this port).
pub type SystemPromptRebuildFn = Arc<dyn Fn(&[String]) -> String + Send + Sync>;

/// The capabilities one extension generation provides (upstream
/// `_buildRuntime` builds the whole runtime, not just the runner). The session
/// applies them together, and only after the build reported no error.
pub struct ExtensionGeneration {
    pub runner: ExtensionRunner,
    /// Replaces the session's command handler when present.
    pub command_handler: Option<ExtensionCommandHandler>,
    /// The generation's callable extension tools; builtin tools are kept.
    pub tools: Vec<pillar_agent::types::AgentTool>,
}

impl ExtensionGeneration {
    /// A generation that only carries a runner (hosts without extra
    /// capabilities, and tests): the current tools and command handler stay.
    pub fn from_runner(runner: ExtensionRunner) -> Self {
        Self {
            runner,
            command_handler: None,
            tools: Vec::new(),
        }
    }
}

/// Host hook rebuilding the extension runner on reload from the previous
/// flag values (upstream `_buildRuntime` constructing a new runner). Returning
/// `Err` keeps the previous generation running: the session builds the new
/// generation *before* tearing the old one down, so a failed rebuild is not a
/// broken session.
pub type ExtensionRunnerFactory =
    Arc<dyn Fn(BTreeMap<String, Value>) -> Result<ExtensionGeneration, String> + Send + Sync>;

/// Host hook publishing the host-owned extension data after a rebuilt runner
/// is installed (upstream the host re-reading the runtime's registries).
pub type ExtensionReloadPublishFn = Arc<dyn Fn() + Send + Sync>;

/// Deferred `beforeSessionStart` callback passed to
/// [`AgentSession::reload`].
pub type BeforeSessionStartFn =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Extension error listener (upstream `ExtensionErrorListener`).
pub type ExtensionErrorListener = Arc<dyn Fn(&ExtensionError) + Send + Sync>;

/// Bindings applied by [`AgentSession::bind_extensions`] (upstream
/// `ExtensionBindings`). The UI and command contexts are host-owned in this
/// port, so only their presence and mode are tracked.
#[derive(Default)]
pub struct ExtensionBindings {
    /// Upstream `uiContext`; only presence is tracked (drives
    /// `runner.set_has_ui`).
    pub ui_context: Option<bool>,
    /// Upstream `mode` ("interactive" | "print" | "json" | "rpc").
    pub mode: Option<String>,
    /// Upstream `onError`; replaces the runner's error listener.
    pub on_error: Option<ExtensionErrorListener>,
}

/// Configuration for [`AgentSession::new`] (upstream `AgentSessionConfig`,
/// capability subset).
pub struct AgentSessionConfig {
    pub agent: Arc<pillar_agent::agent::Agent>,
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub settings_manager: Arc<Mutex<SettingsManager>>,
    pub cwd: String,
    pub resource_loader: Arc<Mutex<ResourceLoader>>,
    pub model_runtime: Arc<ModelRuntime>,
    /// The extension runner. The session reads it at execution time so an
    /// extension reload can swap in a new runner (upstream
    /// `_extensionRunnerRef`).
    pub extension_runner: Arc<Mutex<ExtensionRunner>>,
    /// Initial active built-in tool names (upstream
    /// `initialActiveToolNames`); stored for host use.
    pub initial_active_tool_names: Option<Vec<String>>,
    /// Allowlist of tool names (upstream `allowedToolNames`).
    pub allowed_tool_names: Option<BTreeSet<String>>,
    /// Denylist of tool names (upstream `excludedToolNames`).
    pub excluded_tool_names: Option<BTreeSet<String>>,
    /// Host hook that executes an extension command (`/name args`); returns
    /// true when handled. Upstream `command.handler(args, ctx)` — the port
    /// keeps command contexts host-owned.
    pub command_handler: Option<ExtensionCommandHandler>,
    /// Session-start event metadata (upstream `sessionStartEvent`).
    pub session_start_event: Option<SessionEventMeta>,
    /// Scoped models from `--models` (upstream `scopedModels`).
    pub scoped_models: Vec<ScopedModel>,
    /// Host hook rebuilding the base system prompt from active tool names
    /// (upstream `_rebuildSystemPrompt`).
    pub system_prompt_rebuild: Option<SystemPromptRebuildFn>,
    /// Host hook rebuilding the extension runner on reload (upstream
    /// `_buildRuntime`). Without it, `reload` only reloads settings and
    /// resources and leaves the current generation in place — a generation
    /// that cannot be replaced must not be torn down.
    pub extension_runner_rebuild: Option<ExtensionRunnerFactory>,
    /// Host hook run after the rebuilt runner is in place (upstream the host
    /// re-reading its own extension registries). The host must not run it
    /// while the old runner is still installed.
    pub extension_reload_publish: Option<ExtensionReloadPublishFn>,
    /// The single authorizer every tool call passes before it runs (upstream
    /// the host's permission layer). Denials block the call.
    pub effect_authorizer: Option<crate::core::effects::EffectAuthorizer>,
}

impl AgentSessionConfig {
    /// Minimal config for tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: Arc<pillar_agent::agent::Agent>,
        session_manager: Arc<Mutex<SessionManager>>,
        settings_manager: Arc<Mutex<SettingsManager>>,
        cwd: String,
        resource_loader: Arc<Mutex<ResourceLoader>>,
        model_runtime: Arc<ModelRuntime>,
        extension_runner: Arc<Mutex<ExtensionRunner>>,
    ) -> Self {
        Self {
            agent,
            session_manager,
            settings_manager,
            cwd,
            resource_loader,
            model_runtime,
            extension_runner,
            initial_active_tool_names: None,
            allowed_tool_names: None,
            excluded_tool_names: None,
            command_handler: None,
            session_start_event: None,
            scoped_models: Vec::new(),
            system_prompt_rebuild: None,
            extension_runner_rebuild: None,
            extension_reload_publish: None,
            effect_authorizer: None,
        }
    }
}

/// Listener for session events (upstream `AgentSessionEventListener`).
pub type AgentSessionEventListener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;

/// Mutable session state behind the [`AgentSession`] handle.
#[derive(Default)]
struct SessionState {
    event_listeners: Vec<AgentSessionEventListener>,
    steering_messages: Vec<String>,
    follow_up_messages: Vec<String>,
    pending_next_turn_messages: Vec<pillar_agent::types::CustomMessage>,
    pending_custom_messages: Vec<pillar_agent::types::CustomMessage>,
    is_agent_run_active: bool,
    last_assistant_message: Option<AssistantMessage>,
    retry_attempt: u32,
    overflow_recovery_attempted: bool,
    turn_index: u32,
    retry_abort: Option<AbortSignal>,
    compaction_abort: Option<AbortSignal>,
    auto_compaction_abort: Option<AbortSignal>,
    /// Running branch summarization (upstream `_branchSummaryAbortController`).
    branch_summary_abort: Option<AbortSignal>,
    system_prompt_override: Option<String>,
    /// Scoped models from `--models`, grown by persisted-default
    /// propagation (upstream `_scopedModels`).
    scoped_models: Vec<ScopedModel>,
    /// Upstream `_extensionUIContext` presence (the UI context itself is
    /// host-owned in this port).
    extension_has_ui: bool,
    /// Upstream `_extensionMode` (default "print").
    extension_mode: String,
    /// Upstream `_extensionErrorListener` registered on the runner.
    extension_error_listener: Option<ExtensionErrorListener>,
    /// Running bash commands (upstream `_bashAbortControllers`), keyed by a
    /// session-local token so a finished run can remove exactly its own.
    bash_aborts: Vec<(u64, pillar_agent::AbortSignal)>,
    next_bash_abort_id: u64,
    /// Bash results deferred while the agent streams (upstream
    /// `_pendingBashMessages`).
    pending_bash_messages: Vec<BashExecutionMessage>,
}

/// Captured decision output of a `ModelMutations` pass, applied to the
/// session (agent state, transcript, events) by the caller.
struct AppliedMutations {
    scoped_models: Vec<ScopedModel>,
    model_changes: Vec<(String, String)>,
    thinking_changes: Vec<String>,
    events: Vec<MutationEvent>,
    thinking_level: String,
}

/// Shared session internals.
struct SessionInner {
    agent: Arc<pillar_agent::agent::Agent>,
    session_manager: Arc<Mutex<SessionManager>>,
    settings_manager: Arc<Mutex<SettingsManager>>,
    cwd: String,
    resource_loader: Arc<Mutex<ResourceLoader>>,
    model_runtime: Arc<ModelRuntime>,
    extension_runner: Arc<Mutex<ExtensionRunner>>,
    command_handler: Mutex<Option<ExtensionCommandHandler>>,
    session_start_event: Option<SessionEventMeta>,
    system_prompt_rebuild: Option<SystemPromptRebuildFn>,
    extension_runner_rebuild: Option<ExtensionRunnerFactory>,
    extension_reload_publish: Mutex<Option<ExtensionReloadPublishFn>>,
    effect_authorizer: Option<crate::core::effects::EffectAuthorizer>,
    /// Set by [`AgentSession::dispose`].
    disposed: AtomicBool,
    initial_active_tool_names: Option<Vec<String>>,
    allowed_tool_names: Option<BTreeSet<String>>,
    excluded_tool_names: Option<BTreeSet<String>>,
    state: Mutex<SessionState>,
    /// Base system prompt (without per-turn extension modifications),
    /// captured at construction (upstream `_baseSystemPrompt`). Rewritten by
    /// resource discovery.
    base_system_prompt: Mutex<String>,
    /// Idle watch: `true` when no agent run is active.
    idle_tx: tokio::sync::watch::Sender<bool>,
    /// Kept alive so `send` always stores the latest value (tokio watch
    /// drops sends that have zero receivers).
    _idle_rx: tokio::sync::watch::Receiver<bool>,
    /// Agent-listener unsubscriber.
    unsubscribe_agent: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl SessionInner {
    /// Emit to all listeners (upstream `_emit`). Listeners run outside the
    /// state lock so they may call back into the session.
    /// Emit a payload to the extension runner. When a handler is already
    /// dispatching on this thread (the runner mutex is held for the whole
    /// dispatch), the payload is queued and delivered right after the
    /// outermost dispatch finishes.
    ///
    /// divergence: upstream delivers a nested emit immediately; the port
    /// queues it (same order, after the outer dispatch).
    fn emit_extensions(&self, payload: &Value) -> Option<Value> {
        if crate::core::extensions_runner::in_extension_dispatch() {
            crate::core::extensions_runner::queue_extension_event(payload.clone());
            return None;
        }
        self.extension_runner
            .lock()
            .expect("runner lock")
            .emit(payload)
    }

    fn emit(&self, event: &AgentSessionEvent) {
        let listeners: Vec<AgentSessionEventListener> = self
            .state
            .lock()
            .expect("session state")
            .event_listeners
            .clone();
        for listener in listeners {
            listener(event);
        }
    }

    fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let state = self.state.lock().expect("session state");
            (
                state.steering_messages.clone(),
                state.follow_up_messages.clone(),
            )
        };
        self.emit(&AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        });
    }

    /// Emit an extension error via the runner (upstream
    /// `_extensionRunner.emitError`).
    fn emit_extension_error(&self, event: &str, error: String) {
        self.extension_runner
            .lock()
            .expect("extension runner lock")
            .emit_error(ExtensionError {
                extension_path: "agent-session".to_string(),
                event: event.to_string(),
                error,
                stack: None,
            });
    }

    /// Emit `session_compact_failed` to extensions (upstream
    /// `_emitSessionCompactFailed`).
    fn emit_session_compact_failed(
        &self,
        reason: &'static str,
        error_message: Option<String>,
        aborted: bool,
        will_retry: bool,
        from_extension: bool,
    ) {
        let runner = self.extension_runner.lock().expect("extension runner lock");
        if runner.has_handlers("session_compact_failed") {
            let _ = runner.emit(&serde_json::json!({
                "type": "session_compact_failed",
                "reason": reason,
                "errorMessage": error_message,
                "aborted": aborted,
                "willRetry": will_retry,
                "fromExtension": from_extension,
            }));
        }
    }
}

/// The session streaming loop (upstream `AgentSession`): owns agent event
/// dispatch to extension runner and listeners, session persistence, the
/// prompt/steer/followUp flow, retry, and compaction orchestration.
pub struct AgentSession {
    inner: Arc<SessionInner>,
}

impl AgentSession {
    /// Create a session and subscribe to the agent's event stream
    /// (upstream the constructor: agent subscription + tool hooks).
    pub fn new(config: AgentSessionConfig) -> Self {
        let base_system_prompt = config.agent.state().system_prompt;
        let (idle_tx, idle_rx) = tokio::sync::watch::channel(true);
        let inner = Arc::new(SessionInner {
            agent: config.agent,
            session_manager: config.session_manager,
            settings_manager: config.settings_manager,
            cwd: config.cwd,
            resource_loader: config.resource_loader,
            model_runtime: config.model_runtime,
            extension_runner: config.extension_runner,
            command_handler: Mutex::new(config.command_handler),
            session_start_event: config.session_start_event,
            system_prompt_rebuild: config.system_prompt_rebuild,
            extension_runner_rebuild: config.extension_runner_rebuild,
            extension_reload_publish: Mutex::new(config.extension_reload_publish),
            effect_authorizer: config.effect_authorizer,
            disposed: AtomicBool::new(false),
            initial_active_tool_names: config.initial_active_tool_names,
            allowed_tool_names: config.allowed_tool_names,
            excluded_tool_names: config.excluded_tool_names,
            state: Mutex::new(SessionState {
                scoped_models: config.scoped_models,
                extension_mode: "print".to_string(),
                ..SessionState::default()
            }),
            base_system_prompt: Mutex::new(base_system_prompt),
            idle_tx,
            _idle_rx: idle_rx,
            unsubscribe_agent: Mutex::new(None),
        });
        let session = AgentSession {
            inner: Arc::clone(&inner),
        };
        // Always subscribe to agent events for internal handling (session
        // persistence, extensions, retry logic).
        // Weak: the subscription lives inside the agent, which the session
        // owns, so a strong handle here would keep the session alive forever
        // (docs/ARCHITECTURE-REVIEW-s05c0.md D).
        let unsubscribe = inner.agent.subscribe({
            let inner = Arc::downgrade(&inner);
            move |event, _signal| {
                let inner = inner.clone();
                Box::pin(async move {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    if let Err(message) = AgentSession::handle_agent_event(&inner, event) {
                        inner.emit_extension_error("agent_event", message);
                    }
                })
            }
        });
        *inner.unsubscribe_agent.lock().expect("unsub lock") = Some(Arc::new(unsubscribe));
        session.install_next_turn_refresh();
        session
    }

    // --- Accessors ------------------------------------------------------

    pub fn agent(&self) -> &pillar_agent::agent::Agent {
        &self.inner.agent
    }

    pub fn session_manager(&self) -> &Mutex<SessionManager> {
        &self.inner.session_manager
    }

    pub fn settings_manager(&self) -> &Mutex<SettingsManager> {
        &self.inner.settings_manager
    }

    /// The shared settings manager (hosts that keep their own handle, e.g.
    /// the interactive theme controller).
    pub fn settings_manager_arc(&self) -> Arc<Mutex<SettingsManager>> {
        Arc::clone(&self.inner.settings_manager)
    }

    /// The shared extension runner (hosts that need the registered commands,
    /// e.g. the interactive autocomplete provider).
    pub fn extension_runner_arc(
        &self,
    ) -> Arc<Mutex<crate::core::extensions_runner::ExtensionRunner>> {
        Arc::clone(&self.inner.extension_runner)
    }

    pub fn model_runtime(&self) -> &ModelRuntime {
        &self.inner.model_runtime
    }

    pub fn resource_loader(&self) -> std::sync::MutexGuard<'_, ResourceLoader> {
        self.inner
            .resource_loader
            .lock()
            .expect("resource loader lock")
    }

    /// Loaded skills and prompt templates from the shared resource loader
    /// (upstream reading `resourceLoader` snapshot state).
    fn loaded_skills_and_templates(&self) -> (Vec<LoadedSkill>, Vec<PromptTemplate>) {
        let loader = self
            .inner
            .resource_loader
            .lock()
            .expect("resource loader lock");
        let snapshot = loader.snapshot();
        (
            snapshot.skills.clone(),
            loaded_prompts_to_templates(&snapshot.prompts),
        )
    }

    pub fn cwd(&self) -> &str {
        &self.inner.cwd
    }

    /// Full agent state snapshot (upstream `state`).
    pub fn state(&self) -> pillar_agent::agent::AgentState {
        self.inner.agent.state()
    }

    /// Context usage from the session (upstream `getContextUsage`): after a
    /// compaction, tokens are unknown until the next LLM response.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.model()?;
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }
        let branch: Vec<crate::core::session_entries::SessionEntry> = self
            .session_manager()
            .lock()
            .expect("session manager")
            .get_branch(None)
            .into_iter()
            .cloned()
            .collect();
        compute_context_usage(&branch, context_window)
    }

    /// Current model, if selected (upstream `model`).
    pub fn model(&self) -> Option<pillar_agent::types::FauxModelRef> {
        let state = self.inner.agent.state();
        if state.model.id == "unknown" {
            None
        } else {
            Some(state.model)
        }
    }

    /// Current thinking level as a pi level string (upstream
    /// `thinkingLevel`).
    pub fn thinking_level(&self) -> String {
        self.inner.agent.state().thinking_level.as_str().to_string()
    }

    /// Current effective system prompt (upstream `systemPrompt`).
    pub fn system_prompt(&self) -> String {
        self.inner.agent.state().system_prompt
    }

    /// Whether the session is processing an agent run or post-run
    /// continuation (upstream `isStreaming`).
    pub fn is_streaming(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("session state")
            .is_agent_run_active
    }

    /// Whether the session has no active agent run (upstream `isIdle`).
    pub fn is_idle(&self) -> bool {
        !self.is_streaming()
    }

    /// Current retry attempt (upstream `retryAttempt`).
    pub fn retry_attempt(&self) -> u32 {
        self.inner
            .state
            .lock()
            .expect("session state")
            .retry_attempt
    }

    /// Whether auto-retry is currently in progress (upstream `isRetrying`).
    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        self.inner
            .state
            .lock()
            .expect("session state")
            .scoped_models
            .clone()
    }

    /// Upstream `setScopedModels`: replace the cycling scope (session-only,
    /// never persisted).
    pub fn set_scoped_models(&self, models: Vec<ScopedModel>) {
        self.inner
            .state
            .lock()
            .expect("session state")
            .scoped_models = models;
    }

    pub fn is_retrying(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("session state")
            .retry_abort
            .is_some()
    }

    /// Whether auto-compaction is enabled (upstream `autoCompactionEnabled`).
    pub fn auto_compaction_enabled(&self) -> bool {
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .compaction_settings()
            .enabled
    }

    /// Toggle auto-compaction (upstream `setAutoCompactionEnabled`).
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        let mut settings = self.inner.settings_manager.lock().expect("settings lock");
        let current = settings.compaction_settings();
        settings.set_global_setting(
            "compaction",
            serde_json::json!({
                "enabled": enabled,
                "reserveTokens": current.reserve_tokens,
                "keepRecentTokens": current.keep_recent_tokens,
            }),
        );
    }

    /// Whether compaction or branch summarization is running (upstream
    /// `isCompacting`).
    pub fn is_compacting(&self) -> bool {
        let state = self.inner.state.lock().expect("session state");
        state.auto_compaction_abort.is_some() || state.compaction_abort.is_some()
    }

    /// Thinking levels the current model supports (upstream
    /// `getAvailableThinkingLevels`).
    pub fn available_thinking_levels(&self) -> Vec<String> {
        match self.model() {
            Some(model) => pillar_ai::models::get_supported_thinking_levels(&model.to_model())
                .into_iter()
                .map(|level| match level {
                    pillar_ai::types::ModelThinkingLevel::Off => "off",
                    pillar_ai::types::ModelThinkingLevel::Minimal => "minimal",
                    pillar_ai::types::ModelThinkingLevel::Low => "low",
                    pillar_ai::types::ModelThinkingLevel::Medium => "medium",
                    pillar_ai::types::ModelThinkingLevel::High => "high",
                    pillar_ai::types::ModelThinkingLevel::Xhigh => "xhigh",
                    pillar_ai::types::ModelThinkingLevel::Max => "max",
                })
                .map(str::to_string)
                .collect(),
            None => crate::core::session_support::THINKING_LEVEL_OPTIONS
                .iter()
                .map(|level| (*level).to_string())
                .collect(),
        }
    }

    /// The text of the last assistant message (upstream the RPC
    /// `get_last_assistant_text` response).
    pub fn last_assistant_text(&self) -> Option<String> {
        let messages = self.inner.agent.state().messages;
        messages.iter().rev().find_map(|message| match message {
            pillar_agent::types::AgentMessage::Message(Message::Assistant(assistant)) => {
                Some(pillar_ai::text::content_text(&assistant.content, ""))
            }
            _ => None,
        })
    }

    /// Set the session display name and notify listeners (upstream
    /// `setSessionName`).
    pub fn set_session_name(&self, name: &str) -> Result<(), String> {
        {
            let mut session_manager = self.inner.session_manager.lock().expect("session lock");
            session_manager.append_session_info(name)?;
        }
        self.inner.emit(&AgentSessionEvent::SessionInfoChanged {
            name: Some(name.to_string()),
        });
        Ok(())
    }

    /// Upstream `getUserMessagesForForking`: the user messages that can be
    /// used as fork points, as `(entryId, text)`.
    pub fn user_messages_for_forking(&self) -> Vec<(String, String)> {
        let entries = self
            .inner
            .session_manager
            .lock()
            .expect("session lock")
            .get_entries_owned();
        entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(message) => match &message.message {
                    CodingAgentMessage::Base(Message::User { content, .. }) => {
                        let text = match content {
                            pillar_ai::types::UserContent::Text(text) => text.clone(),
                            pillar_ai::types::UserContent::Blocks(blocks) => {
                                content_text(blocks, "")
                            }
                        };
                        if text.is_empty() {
                            None
                        } else {
                            Some((entry.id().to_string(), text))
                        }
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Registered extension commands (upstream
    /// `extensionRunner.getRegisteredCommands()`).
    pub fn registered_commands(&self) -> Vec<ResolvedCommand> {
        self.inner
            .extension_runner
            .lock()
            .expect("runner lock")
            .registered_commands()
    }

    /// `user_bash` extension hook (upstream
    /// `extensionRunner.emitUserBash`): the first non-null handler result
    /// wins. Extensions may return `{ result, operations }` to take over
    /// the execution.
    pub fn emit_user_bash(&self, event: &Value) -> Option<Value> {
        self.inner
            .extension_runner
            .lock()
            .expect("runner lock")
            .emit_user_bash(event)
    }

    /// Loaded prompt templates (upstream `session.promptTemplates`).
    pub fn prompt_templates(&self) -> Vec<LoadedPrompt> {
        self.resource_loader().snapshot().prompts.clone()
    }

    /// Loaded skills (upstream `resourceLoader.getSkills().skills`).
    pub fn skills(&self) -> Vec<LoadedSkill> {
        self.resource_loader().snapshot().skills.clone()
    }

    /// Upstream `exportToHtml`: render the session and write the HTML file,
    /// returning the written path.
    ///
    /// divergence: the export omits `tools` / `renderedTools` (the port has
    /// no tool-definition renderer yet).
    pub fn export_to_html(&self, output_path: Option<&Path>) -> Result<PathBuf, String> {
        let (header, entries, leaf_id, session_file) = {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            (
                session_manager
                    .get_header()
                    .and_then(|header| serde_json::to_value(header).ok()),
                session_manager.get_entries_owned(),
                session_manager.get_leaf_id().map(str::to_string),
                session_manager.session_file().map(Path::to_path_buf),
            )
        };
        let session_file =
            session_file.ok_or_else(|| "Cannot export in-memory session to HTML".to_string())?;
        let system_prompt = self.system_prompt();
        let data = SessionData::from_parts(
            header,
            &entries,
            leaf_id.as_deref(),
            Some(&system_prompt),
            None,
            None,
        );
        let html = generate_html(&data, None)?;
        write_export(&html, output_path, &session_file)
    }

    /// Whether auto-retry is enabled (upstream `autoRetryEnabled`).
    pub fn auto_retry_enabled(&self) -> bool {
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .retry_settings()
            .enabled
    }

    /// Toggle auto-retry (upstream `setAutoRetryEnabled`).
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .apply_overrides(&serde_json::json!({ "retry": { "enabled": enabled } }));
    }

    /// Session id (upstream `sessionId`).
    pub fn session_id(&self) -> String {
        self.inner
            .session_manager
            .lock()
            .expect("session lock")
            .session_id()
            .to_string()
    }

    /// Initial active built-in tool names from config (upstream
    /// `initialActiveToolNames`).
    pub fn initial_active_tool_names(&self) -> Option<&[String]> {
        self.inner.initial_active_tool_names.as_deref()
    }

    /// Tool allowlist from config (upstream `allowedToolNames`).
    pub fn allowed_tool_names(&self) -> Option<&BTreeSet<String>> {
        self.inner.allowed_tool_names.as_ref()
    }

    /// Tool denylist from config (upstream `excludedToolNames`).
    pub fn excluded_tool_names(&self) -> Option<&BTreeSet<String>> {
        self.inner.excluded_tool_names.as_ref()
    }

    /// Session-start event metadata (upstream `sessionStartEvent`).
    pub fn session_start_event(&self) -> Option<&SessionEventMeta> {
        self.inner.session_start_event.as_ref()
    }

    // --- Event subscription ---------------------------------------------

    /// Subscribe to session events; returns an unsubscriber (upstream
    /// `subscribe`).
    pub fn subscribe(&self, listener: AgentSessionEventListener) -> Arc<dyn Fn() + Send + Sync> {
        self.inner
            .state
            .lock()
            .expect("session state")
            .event_listeners
            .push(listener.clone());
        let inner = Arc::clone(&self.inner);
        Arc::new(move || {
            let mut state = inner.state.lock().expect("session state");
            state.event_listeners.retain(|l| !Arc::ptr_eq(l, &listener));
        })
    }

    // --- Agent event handling -------------------------------------------

    /// Internal agent-event handler (upstream `_handleAgentEvent`). Runs
    /// synchronously: the port's extension runner and session manager are
    /// sync, so the returned future completes immediately.
    fn handle_agent_event(
        inner: &Arc<SessionInner>,
        event: pillar_agent::types::AgentEvent,
    ) -> Result<(), String> {
        // When a user message starts, remove it from either queue BEFORE
        // emitting so the UI sees the updated queue state.
        if let pillar_agent::types::AgentEvent::MessageStart { message } = &event {
            if let pillar_agent::types::AgentMessage::Message(Message::User { content, .. }) =
                &**message
            {
                let message_text = match content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(blocks) => content_text(blocks, ""),
                };
                if !message_text.is_empty() {
                    let mut state = inner.state.lock().expect("session state");
                    if let Some(index) = state
                        .steering_messages
                        .iter()
                        .position(|m| *m == message_text)
                    {
                        state.steering_messages.remove(index);
                        drop(state);
                        inner.emit_queue_update();
                    } else if let Some(index) = state
                        .follow_up_messages
                        .iter()
                        .position(|m| *m == message_text)
                    {
                        state.follow_up_messages.remove(index);
                        drop(state);
                        inner.emit_queue_update();
                    }
                }
            }
        }

        // Emit to extensions first.
        AgentSession::emit_extension_event(inner, &event)?;

        // Notify all listeners.
        let session_event = match &event {
            pillar_agent::types::AgentEvent::AgentEnd { messages } => AgentSessionEvent::AgentEnd {
                messages: messages.clone(),
                will_retry: AgentSession::will_retry_after_agent_end(inner, messages),
            },
            _ => AgentSession::to_session_event(&event),
        };
        inner.emit(&session_event);

        // Handle session persistence.
        if let pillar_agent::types::AgentEvent::MessageEnd { message } = &event {
            let message = (**message).clone();
            let appended = {
                let mut sm = inner.session_manager.lock().expect("session lock");
                match &message {
                    pillar_agent::types::AgentMessage::Custom(custom) => sm
                        .append_custom_message_entry(
                            &custom.custom_type,
                            user_content_to_custom_content(&custom.content),
                            custom.display,
                            custom.details.clone(),
                        ),
                    pillar_agent::types::AgentMessage::Message(msg) => {
                        sm.append_message(CodingAgentMessage::Base(msg.clone()))
                    }
                    _ => Ok(String::new()),
                }
            };
            if let Err(e) = appended {
                inner.emit_extension_error("message_end", e);
            }

            // Track assistant message for auto-compaction and retry.
            if let pillar_agent::types::AgentMessage::Message(Message::Assistant(assistant)) =
                &message
            {
                let mut state = inner.state.lock().expect("session state");
                state.last_assistant_message = Some((**assistant).clone());
                if assistant.stop_reason != StopReason::Error
                    && assistant.stop_reason != StopReason::Length
                {
                    state.overflow_recovery_attempted = false;
                }
                // Reset retry counter immediately on success.
                if assistant.stop_reason != StopReason::Error && state.retry_attempt > 0 {
                    let attempt = state.retry_attempt;
                    state.retry_attempt = 0;
                    drop(state);
                    inner.emit(&AgentSessionEvent::AutoRetryEnd {
                        success: true,
                        attempt,
                        final_error: None,
                    });
                }
            }
        }

        // Context-only custom messages can be inserted once a turn's tool
        // results are all appended.
        if matches!(event, pillar_agent::types::AgentEvent::TurnEnd { .. }) {
            AgentSession::flush_pending_custom_messages(inner);
        }
        Ok(())
    }

    fn to_session_event(event: &pillar_agent::types::AgentEvent) -> AgentSessionEvent {
        match event {
            pillar_agent::types::AgentEvent::AgentStart => AgentSessionEvent::AgentStart,
            pillar_agent::types::AgentEvent::AgentEnd { .. } => unreachable!("handled above"),
            pillar_agent::types::AgentEvent::TurnStart => AgentSessionEvent::TurnStart,
            pillar_agent::types::AgentEvent::TurnEnd {
                message,
                tool_results,
            } => AgentSessionEvent::TurnEnd {
                message: (**message).clone(),
                tool_results: tool_results.clone(),
            },
            pillar_agent::types::AgentEvent::MessageStart { message } => {
                AgentSessionEvent::MessageStart {
                    message: (**message).clone(),
                }
            }
            pillar_agent::types::AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => AgentSessionEvent::MessageUpdate {
                message: (**message).clone(),
                assistant_message_event: Box::new((**assistant_message_event).clone()),
            },
            pillar_agent::types::AgentEvent::MessageEnd { message } => {
                AgentSessionEvent::MessageEnd {
                    message: (**message).clone(),
                }
            }
            pillar_agent::types::AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => AgentSessionEvent::ToolExecutionStart {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
            },
            pillar_agent::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
                partial_result: partial_result.clone(),
            },
            pillar_agent::types::AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => AgentSessionEvent::ToolExecutionEnd {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                result: result.clone(),
                is_error: *is_error,
            },
        }
    }

    /// Emit agent events to the extension runner (upstream
    /// `_emitExtensionEvent`).
    fn emit_extension_event(
        inner: &Arc<SessionInner>,
        event: &pillar_agent::types::AgentEvent,
    ) -> Result<(), String> {
        let runner = inner.extension_runner.lock().expect("runner lock");
        match event {
            pillar_agent::types::AgentEvent::AgentStart => {
                let _ = runner.emit(&serde_json::json!({ "type": "agent_start" }));
            }
            pillar_agent::types::AgentEvent::AgentEnd { messages } => {
                let messages: Result<Vec<Value>, _> =
                    messages.iter().map(serde_json::to_value).collect();
                let _ = runner.emit(&serde_json::json!({
                    "type": "agent_end",
                    "messages": messages.map_err(|e| e.to_string())?,
                }));
            }
            pillar_agent::types::AgentEvent::TurnStart => {
                let turn_index = inner.state.lock().expect("session state").turn_index;
                let _ = runner.emit(&serde_json::json!({
                    "type": "turn_start",
                    "turnIndex": turn_index,
                    "timestamp": pillar_ai::models::now_ms(),
                }));
            }
            pillar_agent::types::AgentEvent::TurnEnd {
                message,
                tool_results,
            } => {
                let turn_index = inner.state.lock().expect("session state").turn_index;
                let message_json = serde_json::to_value(&**message).map_err(|e| e.to_string())?;
                let tool_results_json: Result<Vec<Value>, _> =
                    tool_results.iter().map(serde_json::to_value).collect();
                let _ = runner.emit(&serde_json::json!({
                    "type": "turn_end",
                    "turnIndex": turn_index,
                    "message": message_json,
                    "toolResults": tool_results_json.map_err(|e| e.to_string())?,
                }));
                inner.state.lock().expect("session state").turn_index += 1;
            }
            pillar_agent::types::AgentEvent::MessageStart { message } => {
                let _ = runner.emit(&serde_json::json!({
                    "type": "message_start",
                    "message": serde_json::to_value(&**message).map_err(|e| e.to_string())?,
                }));
            }
            pillar_agent::types::AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                let _ = runner.emit(&serde_json::json!({
                    "type": "message_update",
                    "message": serde_json::to_value(&**message).map_err(|e| e.to_string())?,
                    "assistantMessageEvent": serde_json::to_value(&**assistant_message_event).map_err(|e| e.to_string())?,
                }));
            }
            pillar_agent::types::AgentEvent::MessageEnd { message } => {
                let message_value = serde_json::to_value(&**message).map_err(|e| e.to_string())?;
                let replacement = runner.emit_message_end(
                    serde_json::json!({ "type": "message_end", "message": message_value }),
                );
                if let Some(replacement) = replacement {
                    let normalized = normalize_extension_message(replacement);
                    if let Ok(replacement) =
                        serde_json::from_value::<pillar_agent::types::AgentMessage>(normalized)
                    {
                        // Replace the finalized message in agent state so
                        // later turns and persistence see the rewrite
                        // (upstream `_replaceMessageInPlace`).
                        AgentSession::replace_last_agent_message(inner, &replacement);
                    }
                }
            }
            pillar_agent::types::AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let _ = runner.emit(&serde_json::json!({
                    "type": "tool_execution_start",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "args": args,
                }));
            }
            pillar_agent::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                let _ = runner.emit(&serde_json::json!({
                    "type": "tool_execution_update",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "args": args,
                    "partialResult": partial_result,
                }));
            }
            pillar_agent::types::AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let _ = runner.emit(&serde_json::json!({
                    "type": "tool_execution_end",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "result": result,
                    "isError": is_error,
                }));
            }
        }
        Ok(())
    }

    /// Upstream `_willRetryAfterAgentEnd` over the agent-end messages.
    fn will_retry_after_agent_end(
        inner: &Arc<SessionInner>,
        messages: &[pillar_agent::types::AgentMessage],
    ) -> bool {
        let settings = inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .retry_settings();
        let retry_attempt = inner.state.lock().expect("session state").retry_attempt;
        let context_window = inner.agent.state().model.context_window;
        let coding: Vec<CodingAgentMessage> = messages
            .iter()
            .cloned()
            .map(agent_message_to_coding)
            .collect();
        will_retry_after_agent_end(
            &coding,
            retry_attempt,
            settings.enabled,
            settings.max_retries,
            context_window,
        )
    }

    /// Replace the last agent-state message (upstream
    /// `_replaceMessageInPlace`; the finalized message is always last when
    /// `message_end` fires).
    fn replace_last_agent_message(
        inner: &Arc<SessionInner>,
        replacement: &pillar_agent::types::AgentMessage,
    ) {
        let mut messages = inner.agent.state().messages;
        if let Some(last) = messages.last_mut() {
            if std::mem::discriminant(last) == std::mem::discriminant(replacement) {
                *last = replacement.clone();
                inner.agent.set_messages(messages);
            }
        }
    }

    /// Upstream `executeBash`: run a shell command through the configured
    /// shell (honouring `shellCommandPrefix`), stream output as
    /// `bash_execution_update` events, and record the result in context and
    /// session history.
    pub async fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
    ) -> Result<BashResult, String> {
        let cwd = self.cwd().to_string();
        let prefix = self
            .inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .shell_command_prefix();
        let resolved_command = match prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };

        let abort_signal = pillar_agent::AbortSignal::new();
        let token = {
            let mut state = self.inner.state.lock().expect("session state");
            let token = state.next_bash_abort_id;
            state.next_bash_abort_id += 1;
            state.bash_aborts.push((token, abort_signal.clone()));
            token
        };

        let inner = Arc::clone(&self.inner);
        let event_id = id.map(str::to_string);
        let mut sink = CallbackSink {
            callback: move |delta: &str| {
                inner.emit(&AgentSessionEvent::BashExecutionUpdate {
                    id: event_id.clone(),
                    delta: delta.to_string(),
                });
            },
        };
        let result = crate::core::bash_executor::execute_bash_local(
            &resolved_command,
            &cwd,
            &mut sink,
            Some(&abort_signal),
        );
        self.inner
            .state
            .lock()
            .expect("session state")
            .bash_aborts
            .retain(|(candidate, _)| *candidate != token);
        let result = result?;
        self.record_bash_result(command, &result, exclude_from_context);
        Ok(result)
    }

    /// Upstream `recordBashResult`: record a bash execution in agent context
    /// and session history. Deferred while the agent streams, so a running
    /// tool call keeps its tool_use/tool_result ordering.
    pub fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: bool,
    ) {
        let message = BashExecutionMessage {
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result
                .full_output_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            timestamp: pillar_ai::models::now_ms(),
            exclude_from_context,
        };
        if self.is_streaming() {
            self.inner
                .state
                .lock()
                .expect("session state")
                .pending_bash_messages
                .push(message);
        } else {
            AgentSession::append_bash_message(&self.inner, message);
        }
    }

    /// Cancel running bash commands (upstream `abortBash`).
    pub fn abort_bash(&self) {
        let signals: Vec<pillar_agent::AbortSignal> = self
            .inner
            .state
            .lock()
            .expect("session state")
            .bash_aborts
            .iter()
            .map(|(_, signal)| signal.clone())
            .collect();
        for signal in signals {
            signal.abort();
        }
    }

    /// Whether a bash command is currently running (upstream
    /// `isBashRunning`).
    pub fn is_bash_running(&self) -> bool {
        !self
            .inner
            .state
            .lock()
            .expect("session state")
            .bash_aborts
            .is_empty()
    }

    fn append_bash_message(inner: &Arc<SessionInner>, message: BashExecutionMessage) {
        let agent_message = pillar_agent::types::AgentMessage::BashExecution(Box::new(
            pillar_agent::types::BashExecutionMessage {
                command: message.command.clone(),
                output: message.output.clone(),
                exit_code: message.exit_code,
                cancelled: message.cancelled,
                truncated: message.truncated,
                full_output_path: message.full_output_path.clone(),
                timestamp: message.timestamp,
                exclude_from_context: message.exclude_from_context,
            },
        ));
        let mut messages = inner.agent.state().messages;
        messages.push(agent_message);
        inner.agent.set_messages(messages);
        let mut session_manager = inner.session_manager.lock().expect("session lock");
        let _ = session_manager.append_message(CodingAgentMessage::BashExecution(message));
    }

    fn flush_pending_bash_messages(inner: &Arc<SessionInner>) {
        let pending: Vec<BashExecutionMessage> = {
            let mut state = inner.state.lock().expect("session state");
            std::mem::take(&mut state.pending_bash_messages)
        };
        for message in pending {
            AgentSession::append_bash_message(inner, message);
        }
    }

    fn flush_pending_custom_messages(inner: &Arc<SessionInner>) {
        let pending: Vec<pillar_agent::types::CustomMessage> = {
            let mut state = inner.state.lock().expect("session state");
            std::mem::take(&mut state.pending_custom_messages)
        };
        if pending.is_empty() {
            return;
        }
        for message in pending {
            AgentSession::append_custom_message(inner, message);
        }
    }

    fn append_custom_message(
        inner: &Arc<SessionInner>,
        message: pillar_agent::types::CustomMessage,
    ) {
        let app_message = pillar_agent::types::AgentMessage::Custom(Box::new(message.clone()));
        // Append to agent state + session, then emit start/end.
        let mut messages = inner.agent.state().messages;
        messages.push(app_message.clone());
        inner.agent.set_messages(messages);
        let mut sm = inner.session_manager.lock().expect("session lock");
        let _ = sm.append_custom_message_entry(
            &message.custom_type,
            user_content_to_custom_content(&message.content),
            message.display,
            message.details.clone(),
        );
        drop(sm);
        inner.emit(&AgentSessionEvent::MessageStart {
            message: app_message.clone(),
        });
        inner.emit(&AgentSessionEvent::MessageEnd {
            message: app_message,
        });
    }

    // --- Tool hooks ------------------------------------------------------

    /// Install tool interception hooks on the agent (upstream
    /// `_installAgentToolHooks`). The hooks read the runner at execution
    /// time so extension reloads swap in without reinstalling.
    /// Install the host's post-reload publish hook (upstream the host
    /// re-reading the runtime's registries after `_buildRuntime`).
    pub fn set_extension_reload_publish(&self, publish: ExtensionReloadPublishFn) {
        *self
            .inner
            .extension_reload_publish
            .lock()
            .expect("publish lock") = Some(publish);
    }

    /// Replace the extension-owned tools with a rebuilt generation's, keeping
    /// the builtin and host tools (upstream `_buildRuntime` re-registering the
    /// runtime's custom tools). Call it while the previous runner is still
    /// installed — that is what identifies the tools being replaced.
    pub fn replace_extension_tools(&self, tools: Vec<pillar_agent::types::AgentTool>) {
        let current = self.inner.agent.state().tools;
        let mut kept: Vec<pillar_agent::types::AgentTool> = {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            current
                .into_iter()
                .filter(|tool| runner.tool_owner(tool.name()).is_none())
                .collect()
        };
        kept.extend(tools);
        self.inner.agent.set_tools(kept);
    }

    pub fn install_tool_hooks(&self) {
        // The hooks live inside the agent, which the session owns: hold the
        // session weakly or the cycle keeps it (and the runner) alive forever
        // (docs/ARCHITECTURE-REVIEW-s05c0.md D).
        let inner = Arc::downgrade(&self.inner);
        let before = Arc::new(
            move |context: pillar_agent::types::BeforeToolCallContext,
                  _signal|
                  -> BeforeToolFuture {
                let inner = inner.clone();
                Box::pin(async move {
                    let inner = inner.upgrade()?;
                    // The effect gate comes first, so a tool call is authorized
                    // exactly like the host's `exec` / `fs` callbacks.
                    if let Some(authorizer) = inner.effect_authorizer.clone() {
                        let args = context.args.lock().expect("args lock").clone();
                        if let crate::core::effects::EffectDecision::Deny { reason } =
                            authorizer(&crate::core::effects::EffectIntent::ToolCall {
                                name: context.tool_call.name.clone(),
                                input: args,
                            })
                        {
                            return Some(pillar_agent::types::BeforeToolCallResult {
                                block: true,
                                reason: Some(reason),
                                terminate: false,
                            });
                        }
                    }
                    let runner = inner.extension_runner.lock().expect("runner lock");
                    if !runner.has_handlers("tool_call") {
                        return None;
                    }
                    let args = context.args.lock().expect("args lock").clone();
                    let result = runner.emit_tool_call(&serde_json::json!({
                        "type": "tool_call",
                        "toolName": context.tool_call.name,
                        "toolCallId": context.tool_call.id,
                        "input": args,
                    }));
                    drop(runner);
                    match result {
                        Ok(Some(value)) => {
                            // `block` is the tool_call key; a handler may also
                            // answer the `cancel` shape, so honor both.
                            let block =
                                value.get("block").and_then(Value::as_bool).unwrap_or(false)
                                    || value
                                        .get("cancel")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                            if block {
                                Some(pillar_agent::types::BeforeToolCallResult {
                                    block: true,
                                    reason: value
                                        .get("reason")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    terminate: false,
                                })
                            } else {
                                None
                            }
                        }
                        Ok(None) => None,
                        // A safety hook that failed must not let the tool run
                        // (docs/ARCHITECTURE-REVIEW-s05c0.md A).
                        Err(message) => Some(pillar_agent::types::BeforeToolCallResult {
                            block: true,
                            reason: Some(format!("extension tool_call hook failed: {message}")),
                            terminate: false,
                        }),
                    }
                })
            },
        );
        self.inner.agent.set_before_tool_call(before);

        let inner = Arc::downgrade(&self.inner);
        let after = Arc::new(
            move |context: pillar_agent::types::AfterToolCallContext, _signal| -> AfterToolFuture {
                let inner = inner.clone();
                Box::pin(async move {
                    let inner = inner.upgrade()?;
                    let runner = inner.extension_runner.lock().expect("runner lock");
                    let hook_result = if runner.has_handlers("tool_result") {
                        runner.emit_tool_result(&serde_json::json!({
                            "type": "tool_result",
                            "toolName": context.tool_call.name,
                            "toolCallId": context.tool_call.id,
                            "input": context.args,
                            "content": context.result.content,
                            "details": context.result.details,
                            "isError": context.is_error,
                            "usage": context.result.usage,
                        }))
                    } else {
                        None
                    };
                    drop(runner);
                    let hook_result = hook_result?;
                    let content = hook_result.get("content").cloned().unwrap_or_else(|| {
                        serde_json::to_value(&context.result.content).unwrap_or(Value::Null)
                    });
                    let parsed: Result<Vec<Content>, _> = serde_json::from_value(content);
                    Some(pillar_agent::types::AfterToolCallResult {
                        content: parsed.ok(),
                        details: hook_result.get("details").cloned(),
                        is_error: hook_result
                            .get("isError")
                            .and_then(Value::as_bool)
                            .or(Some(context.is_error)),
                        usage: None,
                        terminate: None,
                    })
                })
            },
        );
        self.inner.agent.set_after_tool_call(after);
    }

    /// Upstream `_installAgentNextTurnRefresh`: chain between-turn threshold
    /// compaction and the fresh system-prompt/tool/model/thinking state onto
    /// the agent's prepare-next-turn hook. The previously installed hook (if
    /// any) runs after compaction and its context replacement is preserved.
    pub fn install_next_turn_refresh(&self) {
        let weak = Arc::downgrade(&self.inner);
        let previous = self.inner.agent.prepare_next_turn_hook();
        let hook = Arc::new(
            move |turn: &pillar_agent::types::ShouldStopAfterTurnContext,
                  signal: Option<pillar_agent::AbortSignal>|
                  -> pillar_agent::types::PrepareNextFuture {
                let weak = weak.clone();
                let previous = previous.clone();
                let turn = turn.clone();
                Box::pin(async move {
                    let inner = weak.upgrade()?;
                    let session = AgentSession {
                        inner: Arc::clone(&inner),
                    };
                    let context = session
                        .compact_before_next_assistant_response(turn.context.clone())
                        .await;
                    let previous_snapshot = match &previous {
                        Some(previous) => {
                            let chained = pillar_agent::types::ShouldStopAfterTurnContext {
                                context: context.clone(),
                                ..turn.clone()
                            };
                            previous(&chained, signal).await
                        }
                        None => None,
                    };
                    let next_context = previous_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.context.clone())
                        .unwrap_or(context);
                    let state = inner.agent.state();
                    let system_prompt = inner
                        .state
                        .lock()
                        .expect("session state")
                        .system_prompt_override
                        .clone()
                        .unwrap_or_else(|| {
                            inner
                                .base_system_prompt
                                .lock()
                                .expect("base prompt lock")
                                .clone()
                        });
                    Some(pillar_agent::types::AgentLoopTurnUpdate {
                        context: Some(pillar_agent::types::AgentContext {
                            system_prompt,
                            messages: next_context.messages,
                            tools: state.tools,
                        }),
                        model: Some(state.model),
                        thinking_level: Some(state.thinking_level),
                    })
                }) as pillar_agent::types::PrepareNextFuture
            },
        );
        self.inner.agent.set_prepare_next_turn(Some(hook));
    }

    /// Upstream `_compactBeforeNextAssistantResponse`: run threshold
    /// compaction between turns when the completed turn's context exceeds
    /// the model window, then hand back the post-compaction transcript.
    async fn compact_before_next_assistant_response(
        &self,
        context: pillar_agent::types::AgentContext,
    ) -> pillar_agent::types::AgentContext {
        let settings = self
            .inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .compaction_settings();
        let model = self.model();
        let should_run = match &model {
            Some(model) if model.context_window > 0 => {
                let messages: Vec<CodingAgentMessage> = context
                    .messages
                    .iter()
                    .cloned()
                    .map(agent_message_to_coding)
                    .collect();
                compaction_driver::should_compact(
                    compaction_driver::estimate_context_tokens(&messages).tokens,
                    model.context_window,
                    &compaction_driver_settings(settings),
                )
            }
            _ => false,
        };
        if !should_run {
            return context;
        }
        if let Err(error) = self.run_auto_compaction(AutoReason::Threshold, false).await {
            self.inner.emit_extension_error(
                "session_before_compact",
                format!("between-turn compaction failed: {error}"),
            );
        }
        let mut next = context;
        next.messages = self.inner.agent.state().messages;
        next
    }

    // --- Model management ------------------------------------------------

    /// Upstream `setModel`: auth gate, transcript append, persisted
    /// defaults, thinking-level switch, and `model_select` emission.
    pub async fn set_model(&self, model: Model, persist: bool) -> Result<(), String> {
        let has_auth = self
            .inner
            .model_runtime
            .has_configured_auth(&model.provider)
            || self
                .inner
                .model_runtime
                .check_auth(&model.provider, None)
                .await
                .map_err(|error| error.to_string())?
                .is_some();
        if !has_auth {
            return Err(format!("No API key for {}/{}", model.provider, model.id));
        }

        let previous_model = self.model().as_ref().map(faux_model_to_model);
        let mut applied = self.run_model_mutation(|mutations| {
            mutations
                .set_model(model.clone(), persist, &mut |_| true)
                .map_err(|error| error.message)
        })?;
        self.apply_mutations(Some(&model), previous_model.as_ref(), &mut applied);
        Ok(())
    }

    /// Current model as the registry model (upstream `session.model`; the
    /// port's [`AgentSession::model`] is the agent's lighter ref).
    pub fn current_model(&self) -> Option<Model> {
        self.model().as_ref().map(faux_model_to_model)
    }

    /// Upstream `setThinkingLevel`: clamp to the current model, append on
    /// change, persist to global defaults only when requested.
    pub fn set_thinking_level(&self, level: &str, persist: bool) {
        if let Ok(mut applied) = self.run_model_mutation(|mutations| {
            mutations.set_thinking_level(level, persist);
            Ok(())
        }) {
            self.apply_mutations(None, None, &mut applied);
        }
    }

    /// Upstream `cycleModel`: cycle through the scoped models (or all
    /// available ones). Returns the applied switch, or `None` when there is
    /// nothing to cycle.
    pub async fn cycle_model(
        &self,
        direction: CycleDirection,
    ) -> Result<Option<ModelSwitchOutcome>, String> {
        let previous_model = self.model().as_ref().map(faux_model_to_model);
        let mut outcome: Option<ModelSwitchOutcome> = None;
        let mut applied = self.run_model_mutation(|mutations| {
            let model_runtime = &self.inner.model_runtime;
            outcome = mutations
                .cycle_model(direction, false, &mut |provider| {
                    model_runtime.has_configured_auth(provider)
                })
                .map_err(|error| error.message)?;
            Ok(())
        })?;
        if let Some(outcome) = &outcome {
            self.apply_mutations(Some(&outcome.model), previous_model.as_ref(), &mut applied);
        }
        Ok(outcome)
    }

    /// Upstream `cycleThinkingLevel`: returns the new level, or `None` when
    /// the model has no alternative levels.
    pub fn cycle_thinking_level(&self) -> Option<String> {
        let mut level = None;
        if let Ok(mut applied) = self.run_model_mutation(|mutations| {
            level = mutations.cycle_thinking_level(false);
            Ok(())
        }) {
            self.apply_mutations(None, None, &mut applied);
        }
        level
    }

    /// Run a `ModelMutations` pass against the settings manager and capture
    /// its decision outputs (appends, events, effective levels).
    fn run_model_mutation<F>(&self, run: F) -> Result<AppliedMutations, String>
    where
        F: FnOnce(&mut ModelMutations<'_>) -> Result<(), String>,
    {
        let current_model = self.model().as_ref().map(faux_model_to_model);
        let current_level = self.thinking_level();
        let scoped_models = self
            .inner
            .state
            .lock()
            .expect("session state")
            .scoped_models
            .clone();
        let available_models = self.inner.model_runtime.get_available_snapshot();
        let mut settings = self.inner.settings_manager.lock().expect("settings lock");
        let mut mutations = ModelMutations {
            settings: &mut settings,
            model: current_model,
            thinking_level: current_level,
            scoped_models,
            available_models,
            appends: TranscriptAppends::default(),
            events: Vec::new(),
        };
        run(&mut mutations)?;
        Ok(AppliedMutations {
            scoped_models: mutations.scoped_models.clone(),
            model_changes: std::mem::take(&mut mutations.appends.model_changes),
            thinking_changes: std::mem::take(&mut mutations.appends.thinking_changes),
            events: std::mem::take(&mut mutations.events),
            thinking_level: mutations.thinking_level.clone(),
        })
    }

    /// Apply a completed mutation pass: agent state, scoped models, session
    /// transcript appends, and session/extension events (upstream the
    /// side-effect order of `setModel` / `setThinkingLevel`).
    fn apply_mutations(
        &self,
        model: Option<&Model>,
        previous_model: Option<&Model>,
        applied: &mut AppliedMutations,
    ) {
        if let Some(model) = model {
            self.inner
                .agent
                .set_model(pillar_agent::types::FauxModelRef::from_model(model));
        }
        self.inner
            .state
            .lock()
            .expect("session state")
            .scoped_models = applied.scoped_models.clone();
        self.inner.agent.set_thinking_level(
            pillar_agent::types::thinking::AgentThinkingLevel::parse(&applied.thinking_level),
        );
        {
            let mut session_manager = self.inner.session_manager.lock().expect("session lock");
            for (provider, id) in &applied.model_changes {
                let _ = session_manager.append_model_change(provider, id);
            }
            for level in &applied.thinking_changes {
                let _ = session_manager.append_thinking_level_change(level);
            }
        }
        for event in &applied.events {
            match event {
                MutationEvent::ThinkingLevelSelect {
                    level,
                    previous_level,
                } => {
                    self.inner.emit(&AgentSessionEvent::ThinkingLevelChanged {
                        level: level.clone(),
                    });
                    self.inner.emit_extensions(&serde_json::json!({
                        "type": "thinking_level_select",
                        "level": level,
                        "previousLevel": previous_level,
                    }));
                }
                MutationEvent::ModelSelect { source, .. } => {
                    let model_json = model
                        .and_then(|model| serde_json::to_value(model).ok())
                        .unwrap_or(Value::Null);
                    let previous_json = previous_model
                        .and_then(|model| serde_json::to_value(model).ok())
                        .unwrap_or(Value::Null);
                    self.inner.emit_extensions(&serde_json::json!({
                        "type": "model_select",
                        "model": model_json,
                        "previousModel": previous_json,
                        "source": source,
                    }));
                }
            }
        }
    }

    // --- Extension binding ------------------------------------------------

    /// Upstream `bindExtensions`: apply host bindings to the runner, emit
    /// `session_start`, and merge extension-discovered resources.
    pub async fn bind_extensions(&self, bindings: ExtensionBindings) {
        {
            let mut state = self.inner.state.lock().expect("session state");
            if let Some(has_ui) = bindings.ui_context {
                state.extension_has_ui = has_ui;
            }
            if let Some(mode) = bindings.mode {
                state.extension_mode = mode;
            }
            if let Some(listener) = bindings.on_error {
                state.extension_error_listener = Some(listener);
            }
        }
        self.apply_extension_bindings();

        let session_start = self.inner.session_start_event.clone();
        let reason = session_start
            .as_ref()
            .map(|event| event.reason.clone())
            .unwrap_or_else(|| "startup".to_string());
        let mut payload = serde_json::json!({
            "type": "session_start",
            "reason": reason,
        });
        if let Some(previous) = session_start
            .as_ref()
            .and_then(|event| event.previous_session_file.clone())
        {
            payload["previousSessionFile"] = Value::String(previous);
        }
        self.inner
            .extension_runner
            .lock()
            .expect("runner lock")
            .emit(&payload);

        let reason = if reason == "reload" {
            "reload"
        } else {
            "startup"
        };
        self.extend_resources_from_extensions(reason);
    }

    /// Upstream `_applyExtensionBindings`: push the tracked UI presence and
    /// error listener onto the runner. The UI/command contexts themselves are
    /// host-owned (divergence).
    fn apply_extension_bindings(&self) {
        let (has_ui, mode, listener) = {
            let state = self.inner.state.lock().expect("session state");
            (
                state.extension_has_ui,
                state.extension_mode.clone(),
                state.extension_error_listener.clone(),
            )
        };
        let mut runner = self.inner.extension_runner.lock().expect("runner lock");
        runner.set_has_ui(has_ui);
        runner.set_context_facts(crate::core::extensions_types::ExtensionContextFacts {
            cwd: self.inner.cwd.clone(),
            mode: extension_mode(&mode),
            has_ui,
        });
        runner.set_error_listener(listener.map(
            |listener| -> Box<dyn Fn(&ExtensionError) + Send> {
                Box::new(move |error| listener(error))
            },
        ));
    }

    /// Upstream `extendResourcesFromExtensions`: merge `resources_discover`
    /// results into the resource loader and rebuild the base system prompt.
    fn extend_resources_from_extensions(&self, reason: &str) {
        let discovered = {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if !runner.has_handlers("resources_discover") {
                return;
            }
            runner.emit_resources_discover(&self.inner.cwd, reason)
        };
        if discovered.skill_paths.is_empty()
            && discovered.prompt_paths.is_empty()
            && discovered.theme_paths.is_empty()
        {
            return;
        }

        self.inner
            .resource_loader
            .lock()
            .expect("resource loader lock")
            .extend_resources(ResourceExtensionPaths {
                skill_paths: build_extension_resource_paths(discovered.skill_paths),
                prompt_paths: build_extension_resource_paths(discovered.prompt_paths),
                theme_paths: build_extension_resource_paths(discovered.theme_paths),
            });

        // Upstream rebuilds `_baseSystemPrompt` from the new resource set and
        // copies it onto the agent. The prompt builder is host-owned here.
        if let Some(rebuild) = &self.inner.system_prompt_rebuild {
            let tool_names: Vec<String> = self
                .inner
                .agent
                .state()
                .tools
                .iter()
                .map(|tool| tool.tool.name.clone())
                .collect();
            let rebuilt = rebuild(&tool_names);
            *self
                .inner
                .base_system_prompt
                .lock()
                .expect("base prompt lock") = rebuilt.clone();
            self.inner.agent.set_system_prompt(rebuilt);
        }
    }

    // --- Streaming loop --------------------------------------------------

    /// Upstream `reload`: shut down the old runner, reload settings and
    /// resources, rebuild the runner via the host factory, then re-emit
    /// `session_start` and extension resources when bindings are present.
    pub async fn reload(
        &self,
        before_session_start: Option<BeforeSessionStartFn>,
    ) -> Result<(), String> {
        let previous_flag_values = {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            runner.flag_values()
        };
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .reload();
        self.sync_queue_modes_from_settings();
        self.inner
            .resource_loader
            .lock()
            .expect("resource loader lock")
            .reload(None)?;
        // divergence: upstream `resetApiProviders()` resets the pi-ai global
        // provider registry; the port has no mutable global registry.

        let Some(factory) = self.inner.extension_runner_rebuild.clone() else {
            return Ok(());
        };
        // Build the new generation first: the old runner is only shut down once
        // a replacement exists, so a failed rebuild (or a factory that refuses)
        // leaves the session on the generation it had
        // (docs/ARCHITECTURE-REVIEW-s05c0.md 1).
        let generation = factory(previous_flag_values)?;
        // Apply the generation: the command handler and the extension tools
        // belong to it, and the previous runner is still installed here, which
        // is what identifies the tools being replaced.
        if generation.command_handler.is_some() {
            *self.inner.command_handler.lock().expect("command handler lock") =
                generation.command_handler;
        }
        self.replace_extension_tools(generation.tools);
        {
            let mut runner = self.inner.extension_runner.lock().expect("runner lock");
            emit_session_shutdown_event(&mut runner, "reload", None);
            runner.invalidate("Extension runtime reloaded.");
        }
        *self.inner.extension_runner.lock().expect("runner lock") = generation.runner;
        self.apply_extension_bindings();
        // The generation is fully installed at this point: the host factory
        // also swapped the command handler and the extension tools, so publish
        // before the new generation reacts to `session_start`.
        if let Some(publish) = self
            .inner
            .extension_reload_publish
            .lock()
            .expect("publish lock")
            .clone()
        {
            publish();
        }

        if let Some(rebuild) = &self.inner.system_prompt_rebuild {
            let tool_names: Vec<String> = self
                .inner
                .agent
                .state()
                .tools
                .iter()
                .map(|tool| tool.tool.name.clone())
                .collect();
            let rebuilt = rebuild(&tool_names);
            *self
                .inner
                .base_system_prompt
                .lock()
                .expect("base prompt lock") = rebuilt.clone();
            self.inner.agent.set_system_prompt(rebuilt);
        }

        let has_bindings = {
            let state = self.inner.state.lock().expect("session state");
            state.extension_has_ui || state.extension_error_listener.is_some()
        };
        if has_bindings {
            if let Some(callback) = before_session_start {
                callback().await;
            }
            self.inner
                .extension_runner
                .lock()
                .expect("runner lock")
                .emit(&serde_json::json!({
                    "type": "session_start",
                    "reason": "reload",
                }));
            self.extend_resources_from_extensions("reload");
        }
        Ok(())
    }

    /// Send a prompt to the agent (upstream `prompt`): extension commands,
    /// `input` interception, skill/template expansion, streaming queueing,
    /// auth validation, pre-prompt compaction, and `before_agent_start`
    /// custom messages.
    pub async fn prompt(&self, text: &str, options: Option<&PromptOptions>) -> Result<(), String> {
        let expand_prompt_templates = options
            .and_then(|o| o.expand_prompt_templates)
            .unwrap_or(true);

        // Handle extension commands first (execute immediately, even during
        // streaming).
        if expand_prompt_templates
            && text.starts_with('/')
            && self.try_execute_extension_command(text)?
        {
            return Ok(());
        }

        if self
            .inner
            .state
            .lock()
            .expect("session state")
            .compaction_abort
            .is_some()
        {
            return Err(
                "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry."
                    .to_string(),
            );
        }

        // Emit input event for extension interception.
        let mut current_text = text.to_string();
        let current_images = options.and_then(|o| o.images.clone());
        {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if runner.has_handlers("input") {
                let input_result = runner.emit_input(
                    &current_text,
                    options
                        .and_then(|o| o.source.as_deref())
                        .unwrap_or("interactive"),
                    options
                        .and_then(|o| o.streaming_behavior)
                        .map(|b| b.as_str()),
                );
                let action = input_result
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("continue");
                match action {
                    "handled" => return Ok(()),
                    "transform" => {
                        if let Some(new_text) = input_result.get("text").and_then(Value::as_str) {
                            current_text = new_text.to_string();
                        }
                    }
                    _ => {}
                }
            }
        }

        // Expand skill commands and prompt templates.
        let expanded_text = if expand_prompt_templates {
            let (skills, templates) = self.loaded_skills_and_templates();
            let mut expanded = self.expand_skill_command(&current_text, &skills);
            expanded = expand_prompt_template(&expanded, &templates);
            expanded
        } else {
            current_text.clone()
        };

        // If streaming, queue via steer() or followUp().
        if self.is_streaming() {
            let behavior = options
                .and_then(|o| o.streaming_behavior)
                .ok_or_else(|| {
                    "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                        .to_string()
                })?;
            match behavior {
                StreamingBehavior::FollowUp => {
                    self.queue_follow_up_now(&expanded_text, current_images)
                }
                StreamingBehavior::Steer => self.queue_steer_now(&expanded_text, current_images),
            }
            return Ok(());
        }

        // Flush any pending bash and custom messages before the new prompt.
        AgentSession::flush_pending_bash_messages(&self.inner);
        AgentSession::flush_pending_custom_messages(&self.inner);

        // Validate model.
        let model = self.model().ok_or_else(format_no_model_selected_message)?;

        let has_configured_auth = self
            .inner
            .model_runtime
            .has_configured_auth(&model.provider)
            || self
                .inner
                .model_runtime
                .check_auth(&model.provider, None)
                .await
                .map_err(|e| e.to_string())?
                .is_some();
        if !has_configured_auth {
            if self.inner.model_runtime.is_using_oauth(&model.provider) {
                return Err(format!(
                    "Authentication failed for \"{}\". Credentials may have expired or network is unavailable. Run '/login {}' to re-authenticate.",
                    model.provider, model.provider
                ));
            }
            return Err(format_no_api_key_found_message(&model.provider));
        }

        // Check whether to compact before sending (catches aborted
        // responses). The new prompt is sent below; do not continue.
        if let Some(last_assistant) = self.find_last_assistant_message() {
            let _ = self.check_compaction(&last_assistant, false).await?;
        }

        // Build messages: user message, pending next-turn asides, then
        // before_agent_start custom messages.
        let mut blocks = vec![Content::Text {
            text: expanded_text.clone(),
            text_signature: None,
        }];
        if let Some(images) = &current_images {
            blocks.extend(images.iter().cloned());
        }
        let mut messages: Vec<pillar_agent::types::AgentMessage> =
            vec![pillar_agent::types::AgentMessage::Message(Message::User {
                content: UserContent::Blocks(blocks),
                timestamp: pillar_ai::models::now_ms(),
            })];

        let pending_next_turn = {
            let mut state = self.inner.state.lock().expect("session state");
            std::mem::take(&mut state.pending_next_turn_messages)
        };
        for message in pending_next_turn {
            messages.push(pillar_agent::types::AgentMessage::Custom(Box::new(message)));
        }

        let mut system_prompt_override: Option<String> = None;
        let base_prompt = self
            .inner
            .base_system_prompt
            .lock()
            .expect("base prompt lock")
            .clone();
        {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if let Some(result) = runner.emit_before_agent_start(&expanded_text, &base_prompt) {
                if let Some(custom_messages) = result.get("messages").and_then(Value::as_array) {
                    for value in custom_messages {
                        if let Ok(message) =
                            serde_json::from_value::<pillar_agent::types::CustomMessage>(
                                normalize_extension_message(value.clone()),
                            )
                        {
                            messages
                                .push(pillar_agent::types::AgentMessage::Custom(Box::new(message)));
                        }
                    }
                }
                if let Some(new_prompt) = result.get("systemPrompt").and_then(Value::as_str) {
                    system_prompt_override = Some(new_prompt.to_string());
                }
            }
        }
        if let Some(override_prompt) = &system_prompt_override {
            self.inner.agent.set_system_prompt(override_prompt.clone());
            self.inner
                .state
                .lock()
                .expect("session state")
                .system_prompt_override = Some(override_prompt.clone());
        } else {
            self.inner.agent.set_system_prompt(base_prompt.clone());
            self.inner
                .state
                .lock()
                .expect("session state")
                .system_prompt_override = None;
        }

        self.run_agent_prompt(messages).await
    }

    /// Try to execute an extension command (upstream
    /// `_tryExecuteExtensionCommand`). Returns true when a command was
    /// found and executed.
    fn try_execute_extension_command(&self, text: &str) -> Result<bool, String> {
        let (command_name, args) = match text.find(' ') {
            Some(index) => (&text[1..index], &text[index + 1..]),
            None => (&text[1..], ""),
        };
        let mut runner = self.inner.extension_runner.lock().expect("runner lock");
        let command = runner.command(command_name);
        let Some(_command) = command else {
            return Ok(false);
        };
        drop(runner);
        let Some(handler) = self
            .inner
            .command_handler
            .lock()
            .expect("command handler lock")
            .clone()
        else {
            // No host handler: the command is registered but not executable
            // in this host.
            return Ok(false);
        };
        match handler(command_name, args) {
            Ok(true) => Ok(true),
            Ok(false) => Ok(false),
            Err(error) => {
                self.inner.emit_extension_error("command", error);
                Ok(true)
            }
        }
    }

    /// Expand `/skill:name args` to the skill's file content (upstream
    /// `_expandSkillCommand`).
    fn expand_skill_command(&self, text: &str, skills: &[LoadedSkill]) -> String {
        if !text.starts_with("/skill:") {
            return text.to_string();
        }
        let (skill_name, args) = match text.find(' ') {
            Some(index) => (&text[7..index], text[index + 1..].trim().to_string()),
            None => (text[7..].trim(), String::new()),
        };
        let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
            return text.to_string();
        };
        match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => {
                let body = strip_frontmatter(&content).trim().to_string();
                let skill_block = format!(
                    "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                    skill.name, skill.file_path, skill.base_dir, body
                );
                if args.is_empty() {
                    skill_block
                } else {
                    format!("{skill_block}\n\n{args}")
                }
            }
            Err(error) => {
                self.inner.emit_extension_error(
                    "skill_expansion",
                    format!("{}: {error}", skill.file_path),
                );
                text.to_string()
            }
        }
    }

    /// Queue a steering message (upstream `steer` + `_queueSteer`).
    pub async fn steer(&self, text: &str, images: Option<&[Content]>) -> Result<(), String> {
        if text.starts_with('/') {
            self.throw_if_extension_command(text);
        }
        let (skills, templates) = self.loaded_skills_and_templates();
        let mut expanded = self.expand_skill_command(text, &skills);
        expanded = expand_prompt_template(&expanded, &templates);
        self.queue_steer(&expanded, images.map(|i| i.to_vec()))
            .await;
        Ok(())
    }

    /// Queue a follow-up message (upstream `followUp` + `_queueFollowUp`).
    pub async fn follow_up(&self, text: &str, images: Option<&[Content]>) -> Result<(), String> {
        if text.starts_with('/') {
            self.throw_if_extension_command(text);
        }
        let (skills, templates) = self.loaded_skills_and_templates();
        let mut expanded = self.expand_skill_command(text, &skills);
        expanded = expand_prompt_template(&expanded, &templates);
        self.queue_follow_up(&expanded, images.map(|i| i.to_vec()))
            .await;
        Ok(())
    }

    async fn queue_steer(&self, text: &str, images: Option<Vec<Content>>) {
        self.queue_steer_now(text, images);
    }

    async fn queue_follow_up(&self, text: &str, images: Option<Vec<Content>>) {
        self.queue_follow_up_now(text, images);
    }

    /// Queue a steering message (sync; upstream `session.steer`).
    pub fn queue_steer_now(&self, text: &str, images: Option<Vec<Content>>) {
        self.inner
            .state
            .lock()
            .expect("session state")
            .steering_messages
            .push(text.to_string());
        self.inner.emit_queue_update();
        self.inner.agent.steer(agent_user_message(text, images));
    }

    /// Queue a follow-up message (sync; upstream `session.followUp`).
    pub fn queue_follow_up_now(&self, text: &str, images: Option<Vec<Content>>) {
        self.inner
            .state
            .lock()
            .expect("session state")
            .follow_up_messages
            .push(text.to_string());
        self.inner.emit_queue_update();
        self.inner.agent.follow_up(agent_user_message(text, images));
    }

    /// Submit text into the running turn (upstream the interactive mode's key
    /// handler calling `session.prompt(text, { streamingBehavior })`).
    ///
    /// divergence: the port's `ModeAction` executor awaits the running turn,
    /// so routing this through it would only queue the message *after* the
    /// turn ended — the message would never steer it. The interactive pump
    /// calls this synchronously instead (the input hook and prompt-template
    /// expansion are sync in the port).
    pub fn queue_streaming_message(
        &self,
        text: &str,
        behavior: StreamingBehavior,
        images: Option<Vec<Content>>,
    ) -> Result<(), String> {
        // Emit input event for extension interception (upstream the
        // `prompt` prologue).
        let mut current_text = text.to_string();
        {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if runner.has_handlers("input") {
                let input_result = runner.emit_input(&current_text, "interactive", None);
                match input_result
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("continue")
                {
                    "handled" => return Ok(()),
                    "transform" => {
                        if let Some(new_text) = input_result.get("text").and_then(Value::as_str) {
                            current_text = new_text.to_string();
                        }
                    }
                    _ => {}
                }
            }
        }

        // Expand skill commands and prompt templates.
        let (skills, templates) = self.loaded_skills_and_templates();
        let mut expanded = self.expand_skill_command(&current_text, &skills);
        expanded = expand_prompt_template(&expanded, &templates);

        match behavior {
            StreamingBehavior::Steer => self.queue_steer_now(&expanded, images),
            StreamingBehavior::FollowUp => self.queue_follow_up_now(&expanded, images),
        }
        Ok(())
    }

    /// The active run's abort signal, if a run is in flight (upstream the
    /// extension context's `signal`).
    pub fn abort_signal(&self) -> Option<pillar_agent::abort::AbortSignal> {
        self.inner.agent.abort_signal()
    }

    /// Signal an abort without waiting for idle (upstream the interactive
    /// mode's Escape handler calling `agent.abort()`).
    ///
    /// divergence: the port's pump calls this while the executor is awaiting
    /// the turn; `abort()` (which waits for idle) stays for callers that need
    /// the turn to be over.
    pub fn signal_abort(&self) {
        self.abort_retry();
        self.inner.agent.abort();
    }

    /// Throw an error if the text is an extension command (upstream
    /// `_throwIfExtensionCommand`).
    fn throw_if_extension_command(&self, text: &str) {
        let command_name = match text.find(' ') {
            Some(index) => &text[1..index],
            None => &text[1..],
        };
        let mut runner = self.inner.extension_runner.lock().expect("runner lock");
        if runner.command(command_name).is_some() {
            panic!(
                "Extension command \"/{command_name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            );
        }
    }

    /// Run the agent prompt plus the post-run loop (upstream
    /// `_runAgentPrompt`).
    async fn run_agent_prompt(
        &self,
        messages: Vec<pillar_agent::types::AgentMessage>,
    ) -> Result<(), String> {
        self.inner
            .state
            .lock()
            .expect("session state")
            .is_agent_run_active = true;
        let _ = self.inner.idle_tx.send(false);

        let prompt_result = self
            .inner
            .agent
            .prompt(pillar_agent::agent::PromptInput::Messages(messages))
            .await;
        prompt_result.map_err(|e| e.0)?;

        while self.handle_post_agent_run().await? {
            self.inner.agent.continue_run().await.map_err(|e| e.0)?;
        }

        self.inner
            .state
            .lock()
            .expect("session state")
            .system_prompt_override = None;
        AgentSession::flush_pending_bash_messages(&self.inner);
        AgentSession::flush_pending_custom_messages(&self.inner);
        self.emit_agent_settled().await;
        Ok(())
    }

    /// Handle what happens after an agent run (upstream
    /// `_handlePostAgentRun`): retry, auto-retry-end, compaction, queued
    /// messages. Returns whether to `continue_run`.
    async fn handle_post_agent_run(&self) -> Result<bool, String> {
        let message = self
            .inner
            .state
            .lock()
            .expect("session state")
            .last_assistant_message
            .take();
        let Some(message) = message else {
            return Ok(false);
        };

        if self.is_retryable_error(&message) && self.prepare_retry(&message).await? {
            return Ok(true);
        }

        if message.stop_reason == StopReason::Error {
            let retry_attempt = self
                .inner
                .state
                .lock()
                .expect("session state")
                .retry_attempt;
            if retry_attempt > 0 {
                self.inner
                    .state
                    .lock()
                    .expect("session state")
                    .retry_attempt = 0;
                self.inner.emit(&AgentSessionEvent::AutoRetryEnd {
                    success: false,
                    attempt: retry_attempt,
                    final_error: message.error_message.clone(),
                });
            }
        }

        if self.check_compaction(&message, true).await? {
            return Ok(true);
        }

        Ok(self.inner.agent.has_queued_messages())
    }

    /// Retry check (upstream `_isRetryableError`).
    fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let context_window = self.inner.agent.state().model.context_window;
        crate::core::agent_session::is_retryable_error(message, context_window)
    }

    /// Find the last assistant message in agent state (upstream
    /// `_findLastAssistantMessage`).
    fn find_last_assistant_message(&self) -> Option<AssistantMessage> {
        let messages = self.inner.agent.state().messages;
        for message in messages.iter().rev() {
            if let pillar_agent::types::AgentMessage::Message(Message::Assistant(assistant)) =
                message
            {
                return Some((**assistant).clone());
            }
        }
        None
    }

    /// Prepare a retryable error for continuation with exponential backoff
    /// (upstream `_prepareRetry`). Returns true if the caller should
    /// continue the agent.
    async fn prepare_retry(&self, message: &AssistantMessage) -> Result<bool, String> {
        let settings = self
            .inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .retry_settings();
        if !settings.enabled {
            return Ok(false);
        }

        let attempt = {
            let mut state = self.inner.state.lock().expect("session state");
            state.retry_attempt += 1;
            if state.retry_attempt > settings.max_retries {
                state.retry_attempt -= 1;
                return Ok(false);
            }
            state.retry_attempt
        };
        let delay_ms = settings.base_delay_ms * 2u64.pow(attempt - 1);

        let signal = AbortSignal::new();
        self.inner.state.lock().expect("session state").retry_abort = Some(signal.clone());

        self.inner.emit(&AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: settings.max_retries,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        });

        // Remove the error message from agent state (keep in session).
        let messages = self.inner.agent.state().messages;
        if let Some(last) = messages.last() {
            if matches!(
                last,
                pillar_agent::types::AgentMessage::Message(Message::Assistant(_))
            ) {
                let mut trimmed = messages;
                trimmed.pop();
                self.inner.agent.set_messages(trimmed);
            }
        }

        // Wait with exponential backoff (abortable).
        let completed = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
            _ = signal.aborted_or_pending() => false,
        };

        {
            let mut state = self.inner.state.lock().expect("session state");
            state.retry_abort = None;
            if !completed {
                let attempt = state.retry_attempt;
                state.retry_attempt = 0;
                drop(state);
                self.inner.emit(&AgentSessionEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: Some("Retry cancelled".to_string()),
                });
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Cancel an in-progress retry (upstream `abortRetry`).
    pub fn abort_retry(&self) {
        if let Some(signal) = self
            .inner
            .state
            .lock()
            .expect("session state")
            .retry_abort
            .as_ref()
        {
            signal.abort(None);
        }
    }

    /// Emit `agent_settled` and mark the session idle (upstream
    /// `_emitAgentSettled`).
    async fn emit_agent_settled(&self) {
        self.inner
            .state
            .lock()
            .expect("session state")
            .is_agent_run_active = false;
        {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            let _ = runner.emit(&serde_json::json!({ "type": "agent_settled" }));
        }
        self.inner.emit(&AgentSessionEvent::AgentSettled);
        let _ = self.inner.idle_tx.send(true);
    }

    // --- Custom and user messages ---------------------------------------

    /// Send a custom message to the session (upstream `sendCustomMessage`).
    pub async fn send_custom_message(
        &self,
        message: pillar_agent::types::CustomMessage,
        options: Option<&SendCustomMessageOptions>,
    ) -> Result<(), String> {
        let deliver_as = options.and_then(|o| o.deliver_as);
        let trigger_turn = options.and_then(|o| o.trigger_turn);
        let plan = plan_custom_message(self.is_streaming(), trigger_turn, deliver_as);
        match plan {
            CustomMessagePlan::PendingNextTurn => {
                self.inner
                    .state
                    .lock()
                    .expect("session state")
                    .pending_next_turn_messages
                    .push(message);
            }
            CustomMessagePlan::Steer => {
                self.inner
                    .agent
                    .steer(pillar_agent::types::AgentMessage::Custom(Box::new(message)));
            }
            CustomMessagePlan::FollowUp => {
                self.inner
                    .agent
                    .follow_up(pillar_agent::types::AgentMessage::Custom(Box::new(message)));
            }
            CustomMessagePlan::RunPrompt => {
                self.run_agent_prompt(vec![pillar_agent::types::AgentMessage::Custom(Box::new(
                    message,
                ))])
                .await?;
            }
            CustomMessagePlan::PendingTurnEnd => {
                self.inner
                    .state
                    .lock()
                    .expect("session state")
                    .pending_custom_messages
                    .push(message);
            }
            CustomMessagePlan::AppendNow => {
                AgentSession::append_custom_message(&self.inner, message);
            }
        }
        Ok(())
    }

    /// Send a user message to the agent; always triggers a turn (upstream
    /// `sendUserMessage`).
    pub async fn send_user_message(
        &self,
        content: UserContent,
        options: Option<&SendUserMessageOptions>,
    ) -> Result<(), String> {
        let (text, images) = match &content {
            UserContent::Text(text) => (text.clone(), None),
            UserContent::Blocks(blocks) => {
                let mut text_parts = Vec::new();
                let mut images = Vec::new();
                for part in blocks {
                    match part {
                        Content::Text { text, .. } => text_parts.push(text.clone()),
                        Content::Image { data, mime_type } => images.push(Content::Image {
                            data: data.clone(),
                            mime_type: mime_type.clone(),
                        }),
                        _ => {}
                    }
                }
                (
                    text_parts.join("\n"),
                    if images.is_empty() {
                        None
                    } else {
                        Some(images)
                    },
                )
            }
        };
        self.prompt(
            &text,
            Some(&PromptOptions {
                expand_prompt_templates: Some(
                    options
                        .and_then(|o| o.expand_prompt_templates)
                        .unwrap_or(false),
                ),
                streaming_behavior: options.and_then(|o| o.deliver_as),
                images,
                source: Some("extension".to_string()),
            }),
        )
        .await
    }

    /// Clear all queued messages and return them (upstream `clearQueue`).
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let (steering, follow_up) = {
            let mut state = self.inner.state.lock().expect("session state");
            let steering = std::mem::take(&mut state.steering_messages);
            let follow_up = std::mem::take(&mut state.follow_up_messages);
            (steering, follow_up)
        };
        self.inner.agent.clear_all_queues();
        self.inner.emit_queue_update();
        (steering, follow_up)
    }

    /// Number of pending messages (upstream `pendingMessageCount`).
    pub fn pending_message_count(&self) -> usize {
        let state = self.inner.state.lock().expect("session state");
        state.steering_messages.len() + state.follow_up_messages.len()
    }

    /// Get pending steering messages (upstream `getSteeringMessages`).
    pub fn get_steering_messages(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .expect("session state")
            .steering_messages
            .clone()
    }

    /// Get pending follow-up messages (upstream `getFollowUpMessages`).
    pub fn get_follow_up_messages(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .expect("session state")
            .follow_up_messages
            .clone()
    }

    // --- Abort / idle ---------------------------------------------------

    /// Abort the current operation and wait for the agent to become idle
    /// (upstream `abort`).
    pub async fn abort(&self) {
        self.signal_abort();
        self.wait_for_idle().await;
    }

    /// Wait until the session is idle (upstream `waitForIdle`).
    pub async fn wait_for_idle(&self) {
        if self.is_idle() {
            return;
        }
        let mut rx = self.inner.idle_tx.subscribe();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    // --- Queue modes ----------------------------------------------------

    /// Sync agent queue modes from settings (upstream
    /// `syncQueueModesFromSettings`).
    pub fn sync_queue_modes_from_settings(&self) {
        let settings = self.inner.settings_manager.lock().expect("settings lock");
        let steering = queue_mode_from_str(settings.steering_mode());
        let follow_up = queue_mode_from_str(settings.follow_up_mode());
        drop(settings);
        self.inner.agent.set_steering_mode(steering);
        self.inner.agent.set_follow_up_mode(follow_up);
    }

    /// Set steering message mode; saves to settings (upstream
    /// `setSteeringMode`).
    pub fn set_steering_mode(&self, mode: pillar_agent::types::QueueMode) {
        self.inner.agent.set_steering_mode(mode);
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .set_global_setting(
                "steeringMode",
                Value::String(match mode {
                    pillar_agent::types::QueueMode::All => "all".to_string(),
                    pillar_agent::types::QueueMode::OneAtATime => "one-at-a-time".to_string(),
                }),
            );
    }

    /// Set follow-up message mode; saves to settings (upstream
    /// `setFollowUpMode`).
    pub fn set_follow_up_mode(&self, mode: pillar_agent::types::QueueMode) {
        self.inner.agent.set_follow_up_mode(mode);
        self.inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .set_global_setting(
                "followUpMode",
                Value::String(match mode {
                    pillar_agent::types::QueueMode::All => "all".to_string(),
                    pillar_agent::types::QueueMode::OneAtATime => "one-at-a-time".to_string(),
                }),
            );
    }

    // --- Disposal -------------------------------------------------------

    /// Remove all listeners and disconnect from the agent (upstream
    /// `dispose`): after it no hook, listener or extension callback may
    /// mutate this session (docs/ARCHITECTURE-REVIEW-s05c0.md D).
    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.abort_retry();
        self.abort_compaction();
        self.inner.agent.abort();
        // The agent keeps the tool / next-turn hooks: without clearing them a
        // later event would still call back into a disposed session.
        self.inner.agent.clear_agent_hooks();
        if let Some(unsubscribe) = self
            .inner
            .unsubscribe_agent
            .lock()
            .expect("unsub lock")
            .take()
        {
            unsubscribe();
        }
        {
            let mut runner = self.inner.extension_runner.lock().expect("runner lock");
            // `disposed` is already set, so the host API is unbound while this
            // event runs: a shutdown handler may clean up its own state but
            // cannot mutate the session it is shutting down.
            emit_session_shutdown_event(&mut runner, "quit", None);
            runner.invalidate("The session was disposed.");
        }
        self.inner
            .state
            .lock()
            .expect("session state")
            .event_listeners
            .clear();
    }

    /// Whether [`AgentSession::dispose`] ran (the host APIs treat a disposed
    /// session like an unbound one).
    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::SeqCst)
    }

    // --- Compaction -----------------------------------------------------

    /// Cancel an in-progress compaction (upstream `abortCompaction`).
    pub fn abort_compaction(&self) {
        let state = self.inner.state.lock().expect("session state");
        if let Some(signal) = &state.compaction_abort {
            signal.abort(None);
        }
        if let Some(signal) = &state.auto_compaction_abort {
            signal.abort(None);
        }
    }

    /// Upstream `_checkCompaction`: decide and run automatic compaction
    /// after an assistant message. Returns whether the post-run loop should
    /// `continue_run`.
    async fn check_compaction(
        &self,
        assistant: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> Result<bool, String> {
        let settings = self
            .inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .compaction_settings();
        if !settings.enabled {
            return Ok(false);
        }
        if skip_aborted_check && assistant.stop_reason == StopReason::Aborted {
            return Ok(false);
        }

        let model = self.model();
        let decision = {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            let branch: Vec<SessionEntry> = session_manager
                .get_branch(None)
                .into_iter()
                .cloned()
                .collect();
            let messages = self
                .inner
                .agent
                .state()
                .messages
                .iter()
                .cloned()
                .map(agent_message_to_coding)
                .collect::<Vec<_>>();
            let overflow_recovery_attempted = self
                .inner
                .state
                .lock()
                .expect("session state")
                .overflow_recovery_attempted;
            auto_driver::check_compaction(&auto_driver::CheckInput {
                assistant,
                settings: compaction_driver_settings(settings),
                current_model: model.as_ref().map(|m| {
                    (
                        m.provider.as_str(),
                        m.id.as_str(),
                        m.context_window,
                        m.max_tokens,
                    )
                }),
                branch: &branch,
                messages: &messages,
                overflow_recovery_attempted,
            })
        };

        match decision {
            CompactionDecision::None => Ok(false),
            CompactionDecision::RecoveryFailed { error_message } => {
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: "overflow",
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(error_message.clone()),
                });
                self.inner.emit_session_compact_failed(
                    "overflow",
                    Some(error_message),
                    false,
                    false,
                    false,
                );
                Ok(false)
            }
            CompactionDecision::Run {
                reason,
                will_retry,
                drop_trailing_assistant,
            } => {
                if drop_trailing_assistant {
                    self.drop_trailing_assistant_message();
                }
                self.run_auto_compaction(reason, will_retry).await
            }
        }
    }

    fn drop_trailing_assistant_message(&self) {
        let messages = self.inner.agent.state().messages;
        if let Some(last) = messages.last() {
            if matches!(
                last,
                pillar_agent::types::AgentMessage::Message(Message::Assistant(_))
            ) {
                let mut trimmed = messages;
                trimmed.pop();
                self.inner.agent.set_messages(trimmed);
            }
        }
    }

    /// Execute threshold or overflow compaction (upstream
    /// `_runAutoCompaction`). Returns whether the post-run loop should
    /// `continue_run`.
    async fn run_auto_compaction(
        &self,
        reason: AutoReason,
        will_retry: bool,
    ) -> Result<bool, String> {
        let settings = self
            .inner
            .settings_manager
            .lock()
            .expect("settings lock")
            .compaction_settings();
        let mut from_extension = false;

        let model = self.model();
        let Some(model) = model else {
            return Ok(false);
        };

        let (request_model, api_key, headers, env) =
            self.get_summarization_request_auth(&model).await?;

        let (preparation, path_entries) = {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            let path_entries: Vec<SessionEntry> = session_manager
                .get_branch(None)
                .into_iter()
                .cloned()
                .collect();
            let context = session_manager.session_context();
            let messages = context.messages;
            let resolver = first_kept_entry_id_resolver(&path_entries);
            (
                prepare_compaction(&messages, compaction_driver_settings(settings), resolver),
                path_entries,
            )
        };
        let Some(preparation) = preparation else {
            return Ok(false);
        };

        self.inner.emit(&AgentSessionEvent::CompactionStart {
            reason: reason.as_str(),
        });
        let auto_signal = AbortSignal::new();
        self.inner
            .state
            .lock()
            .expect("session state")
            .auto_compaction_abort = Some(auto_signal.clone());

        let mut extension_compaction: Option<CompactionResult> = None;
        {
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if runner.has_handlers("session_before_compact") {
                let extension_result = runner.emit(&serde_json::json!({
                    "type": "session_before_compact",
                    "preparation": preparation_to_json(&preparation),
                    "branchEntries": branch_entries_to_json(&path_entries),
                    "customInstructions": Value::Null,
                    "reason": reason.as_str(),
                    "willRetry": will_retry,
                }));
                if let Some(result) = extension_result {
                    if result
                        .get("cancel")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        drop(runner);
                        self.inner.emit(&AgentSessionEvent::CompactionEnd {
                            reason: reason.as_str(),
                            result: None,
                            aborted: true,
                            will_retry: false,
                            error_message: None,
                        });
                        self.inner.emit_session_compact_failed(
                            reason.as_str(),
                            None,
                            true,
                            false,
                            false,
                        );
                        return Ok(false);
                    }
                    if let Some(compaction) = result.get("compaction") {
                        if let Some(compaction) = compaction_result_from_json(compaction) {
                            extension_compaction = Some(compaction);
                            from_extension = true;
                        }
                    }
                }
            }
        }

        let (summary, first_kept_entry_id, tokens_before, usage, details) =
            if let Some(extension_compaction) = extension_compaction {
                (
                    extension_compaction.summary,
                    extension_compaction.first_kept_entry_id,
                    extension_compaction.tokens_before,
                    extension_compaction.usage,
                    extension_compaction.details,
                )
            } else {
                let result = self
                    .run_default_compaction(
                        preparation,
                        &request_model,
                        api_key,
                        headers,
                        None,
                        auto_signal.clone(),
                        env,
                    )
                    .await?;
                (
                    result.summary,
                    result.first_kept_entry_id,
                    result.tokens_before,
                    result.usage,
                    result.details,
                )
            };

        if auto_signal.is_aborted() {
            self.inner.emit(&AgentSessionEvent::CompactionEnd {
                reason: reason.as_str(),
                result: None,
                aborted: true,
                will_retry: false,
                error_message: None,
            });
            self.inner.emit_session_compact_failed(
                reason.as_str(),
                None,
                true,
                false,
                from_extension,
            );
            self.inner
                .state
                .lock()
                .expect("session state")
                .auto_compaction_abort = None;
            return Ok(false);
        }

        {
            let mut session_manager = self.inner.session_manager.lock().expect("session lock");
            let _ = session_manager.append_compaction(
                &summary,
                &first_kept_entry_id,
                tokens_before,
                compaction_details_to_json(&details),
                from_extension,
                usage.clone(),
            );
        }
        let new_messages = {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            session_manager.session_context().messages
        };
        self.inner.agent.set_messages(
            new_messages
                .iter()
                .cloned()
                .map(coding_message_to_agent)
                .collect(),
        );

        let estimated_tokens_after: u64 = new_messages.iter().map(estimate_tokens).sum();

        // Saved compaction entry for the extension event.
        {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            let entries = session_manager.get_entries_owned();
            let saved = entries
                .iter()
                .rev()
                .find(|e| matches!(e, SessionEntry::Compaction(c) if c.summary == summary))
                .cloned();
            let runner = self.inner.extension_runner.lock().expect("runner lock");
            if let Some(SessionEntry::Compaction(saved)) = saved {
                let _ = runner.emit(&serde_json::json!({
                    "type": "session_compact",
                    "compactionEntry": compaction_entry_to_json(&saved),
                    "fromExtension": from_extension,
                    "reason": reason.as_str(),
                    "willRetry": will_retry,
                }));
            }
        }

        let result = CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            estimated_tokens_after: Some(estimated_tokens_after),
            usage,
            details,
        };
        self.inner
            .state
            .lock()
            .expect("session state")
            .auto_compaction_abort = None;
        self.inner.emit(&AgentSessionEvent::CompactionEnd {
            reason: reason.as_str(),
            result: Some(result),
            aborted: false,
            will_retry,
            error_message: None,
        });

        if will_retry {
            // The overflow response was persisted on message_end before
            // _checkCompaction removed it; rebuild state can restore a
            // trailing assistant that agent.continue() rejects.
            let messages = self.inner.agent.state().messages;
            if let Some(last) = messages.last() {
                if let pillar_agent::types::AgentMessage::Message(Message::Assistant(assistant)) =
                    last
                    && (assistant.stop_reason == StopReason::Error
                        || assistant.stop_reason == StopReason::Length)
                {
                    let mut trimmed = messages;
                    trimmed.pop();
                    self.inner.agent.set_messages(trimmed);
                }
            }
            return Ok(true);
        }

        // Auto-compaction can complete while messages are queued; continue
        // once so they are delivered.
        Ok(self.inner.agent.has_queued_messages())
    }

    /// Resolve request auth for the summarization call (upstream
    /// `_getSummarizationRequestAuth`): required auth for simple streams,
    /// best-effort otherwise.
    async fn get_summarization_request_auth(
        &self,
        model: &pillar_agent::types::FauxModelRef,
    ) -> Result<
        (
            pillar_ai::types::Model,
            Option<String>,
            Option<Value>,
            Option<Value>,
        ),
        String,
    > {
        let is_simple = self.inner.agent.stream_function.is_some();
        if is_simple {
            return self.get_required_request_auth(model).await;
        }
        let model_value = faux_model_to_model(model);
        let result = self
            .inner
            .model_runtime
            .get_auth(AuthTarget::Model(Box::new(model_value.clone())), None)
            .await
            .map_err(|e| e.to_string())?;
        let Some(result) = result else {
            return Ok((model_value, None, None, None));
        };
        Ok((
            model_value,
            result.auth.api_key,
            result
                .auth
                .headers
                .map(|h| serde_json::to_value(h).unwrap_or(Value::Null)),
            result
                .env
                .map(|env| serde_json::to_value(env).unwrap_or(Value::Null)),
        ))
    }

    /// Required request auth (upstream `_getRequiredRequestAuth`).
    async fn get_required_request_auth(
        &self,
        model: &pillar_agent::types::FauxModelRef,
    ) -> Result<
        (
            pillar_ai::types::Model,
            Option<String>,
            Option<Value>,
            Option<Value>,
        ),
        String,
    > {
        let model_value = faux_model_to_model(model);
        let result = self
            .inner
            .model_runtime
            .get_auth(AuthTarget::Model(Box::new(model_value.clone())), None)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(result) = result {
            if result.auth.api_key.is_some() || result.auth.headers.is_some() {
                return Ok((
                    model_value,
                    result.auth.api_key,
                    result
                        .auth
                        .headers
                        .map(|h| serde_json::to_value(h).unwrap_or(Value::Null)),
                    result
                        .env
                        .map(|env| serde_json::to_value(env).unwrap_or(Value::Null)),
                ));
            }
        }
        if self.inner.model_runtime.is_using_oauth(&model.provider) {
            return Err(format!(
                "Authentication failed for \"{}\". Credentials may have expired or network is unavailable. Run '/login {}' to re-authenticate.",
                model.provider, model.provider
            ));
        }
        Err(format_no_api_key_found_message(&model.provider))
    }

    /// Run the built-in default compaction summary (upstream
    /// `_runDefaultCompaction`), routed through the agent's stream
    /// function.
    #[allow(clippy::too_many_arguments)]
    async fn run_default_compaction(
        &self,
        preparation: CompactionPreparation,
        request_model: &pillar_ai::types::Model,
        api_key: Option<String>,
        headers: Option<Value>,
        custom_instructions: Option<&str>,
        signal: AbortSignal,
        env: Option<Value>,
    ) -> Result<CompactionResult, String> {
        let summarizer = SummarizeStreamFn {
            stream: self.inner.agent.stream_function.clone(),
        };
        let options = SummarizationOptions {
            api_key,
            headers: headers.and_then(|h| serde_json::from_value(h).ok()),
            env: env.and_then(|e| serde_json::from_value(e).ok()),
            signal: Some(signal),
            reasoning: Some(self.thinking_level()),
            session_id: None,
            max_tokens: None,
        };
        compact(
            preparation,
            request_model,
            options,
            custom_instructions,
            &summarizer,
        )
        .await
    }

    /// The session tree (upstream `sessionManager.getTree()`).
    pub fn get_tree(&self) -> Vec<crate::core::session_manager::SessionTreeNode> {
        self.inner
            .session_manager
            .lock()
            .expect("session lock")
            .get_tree()
    }

    /// The current leaf id (upstream `sessionManager.getLeafId()`).
    pub fn get_leaf_id(&self) -> Option<String> {
        self.inner
            .session_manager
            .lock()
            .expect("session lock")
            .get_leaf_id()
            .map(str::to_string)
    }

    /// Abort a running branch summarization (upstream
    /// `abortBranchSummary`).
    pub fn abort_branch_summary(&self) {
        let signal = self
            .inner
            .state
            .lock()
            .expect("session state")
            .branch_summary_abort
            .clone();
        if let Some(signal) = signal {
            signal.abort(None);
        }
    }

    /// Whether a branch summarization is running (upstream the abort
    /// controller's presence).
    pub fn is_branch_summarizing(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("session state")
            .branch_summary_abort
            .is_some()
    }

    /// Navigate the session tree: move the leaf to `target_id`, optionally
    /// summarizing the abandoned branch (upstream `navigateTree`).
    ///
    /// divergence: upstream resolves after awaiting extension handlers and
    /// the summarizer inline; the port keeps the same ordering but the host
    /// drives it from the executor (the escape-to-abort hook uses
    /// [`Self::abort_branch_summary`]).
    #[allow(clippy::too_many_lines)]
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: TreeNavigationOptions,
    ) -> Result<TreeNavigationResult, String> {
        if self.is_streaming() {
            return Err(
                "Wait for the current response to finish before navigating the session tree."
                    .to_string(),
            );
        }

        let old_leaf_id = self.get_leaf_id();

        // No-op if already at the target.
        if Some(target_id) == old_leaf_id.as_deref() {
            return Ok(TreeNavigationResult::default());
        }

        // Model required for summarization.
        let model = self.model();
        if options.summarize && model.is_none() {
            return Err("No model available for summarization".to_string());
        }

        // Target entry and the entries to summarize (from old leaf to the
        // common ancestor).
        let (target_entry, collected) = {
            let session_manager = self.inner.session_manager.lock().expect("session lock");
            let Some(target_entry) = session_manager.get_entry(target_id).cloned() else {
                return Err(format!("Entry {target_id} not found"));
            };
            (
                target_entry,
                collect_entries_for_branch_summary(
                    &session_manager.tree_view(),
                    old_leaf_id.as_deref(),
                    target_id,
                ),
            )
        };
        let entries_to_summarize = collected.entries;
        let common_ancestor_id = collected.common_ancestor_id;

        let mut custom_instructions = options.custom_instructions.clone();
        let mut replace_instructions = options.replace_instructions;
        let mut label = options.label.clone();

        let preparation = crate::core::extensions_types::TreePreparation {
            target_id: target_id.to_string(),
            old_leaf_id: old_leaf_id.clone(),
            common_ancestor_id,
            entries_to_summarize: entries_to_summarize.clone(),
            user_wants_summary: options.summarize,
            custom_instructions: custom_instructions.clone(),
            replace_instructions,
            label: label.clone(),
        };

        let branch_signal = AbortSignal::new();
        self.inner
            .state
            .lock()
            .expect("session state")
            .branch_summary_abort = Some(branch_signal.clone());

        let result: Result<TreeNavigationResult, String> = async {
            let mut extension_summary: Option<(String, Option<Value>, Option<pillar_ai::types::Usage>)> =
                None;
            let mut from_extension = false;

            // Emit `session_before_tree`.
            {
                let runner = self.inner.extension_runner.lock().expect("runner lock");
                if runner.has_handlers("session_before_tree") {
                    let extension_result = runner.emit(&serde_json::json!({
                        "type": "session_before_tree",
                        "preparation": tree_preparation_to_json(&preparation),
                    }));
                    if let Some(result) = extension_result {
                        if result
                            .get("cancel")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            return Ok(TreeNavigationResult {
                                cancelled: true,
                                ..Default::default()
                            });
                        }
                        let summary = result.get("summary");
                        if options.summarize {
                            if let Some(summary) = summary {
                                if let Some(text) = summary.get("summary").and_then(Value::as_str) {
                                    extension_summary = Some((
                                        text.to_string(),
                                        summary.get("details").cloned(),
                                        summary
                                            .get("usage")
                                            .and_then(|u| serde_json::from_value(u.clone()).ok()),
                                    ));
                                    from_extension = true;
                                }
                            }
                        }
                        if let Some(value) = result.get("customInstructions") {
                            custom_instructions = value.as_str().map(str::to_string);
                        }
                        if let Some(value) = result.get("replaceInstructions") {
                            replace_instructions = value.as_bool();
                        }
                        if let Some(value) = result.get("label") {
                            label = value.as_str().map(str::to_string);
                        }
                    }
                }
            }

            // Run the default summarizer when needed.
            let mut summary_text: Option<String> = None;
            let mut summary_details: Option<Value> = None;
            let mut summary_usage: Option<pillar_ai::types::Usage> = None;
            if options.summarize && !entries_to_summarize.is_empty() && extension_summary.is_none() {
                let model = model.clone().expect("model checked above");
                let (request_model, api_key, headers, env) =
                    self.get_summarization_request_auth(&model).await?;
                let reserve_tokens = self
                    .inner
                    .settings_manager
                    .lock()
                    .expect("settings lock")
                    .branch_summary_settings()
                    .reserve_tokens;
                let summarizer = SummarizeStreamFn {
                    stream: self.inner.agent.stream_function.clone(),
                };
                let result = generate_branch_summary(
                    &entries_to_summarize,
                    GenerateBranchSummaryOptions {
                        model: &request_model,
                        api_key,
                        headers: headers.and_then(|h| serde_json::from_value(h).ok()),
                        env: env.and_then(|e| serde_json::from_value(e).ok()),
                        signal: Some(branch_signal.clone()),
                        custom_instructions: custom_instructions.as_deref(),
                        replace_instructions: replace_instructions.unwrap_or(false),
                        reserve_tokens,
                        stream_fn: &summarizer,
                    },
                )
                .await;
                if result.aborted {
                    return Ok(TreeNavigationResult {
                        cancelled: true,
                        aborted: true,
                        ..Default::default()
                    });
                }
                if let Some(error) = result.error {
                    return Err(error);
                }
                summary_text = result.summary;
                summary_usage = result.usage;
                summary_details = Some(
                    serde_json::to_value(BranchSummaryDetails {
                        read_files: result.read_files.unwrap_or_default(),
                        modified_files: result.modified_files.unwrap_or_default(),
                    })
                    .unwrap_or(Value::Null),
                );
            } else if let Some((text, details, usage)) = extension_summary {
                summary_text = Some(text);
                summary_details = details;
                summary_usage = usage;
            }

            // Determine the new leaf position based on the target type.
            let navigation = plan_tree_navigation(&target_entry, options.summarize)?;
            let new_leaf_id = navigation.new_leaf_id;
            let editor_text = navigation.editor_text;

            // Switch the leaf (with or without summary).
            let summary_entry_id = {
                let mut session_manager =
                    self.inner.session_manager.lock().expect("session lock");
                if let Some(summary_text) = &summary_text {
                    let id = session_manager.branch_with_summary(
                        new_leaf_id.as_deref(),
                        summary_text,
                        summary_details,
                        from_extension,
                        summary_usage,
                    )?;
                    if let Some(label) = &label {
                        let _ = session_manager.append_label_change(&id, Some(label));
                    }
                    Some(id)
                } else {
                    match new_leaf_id.as_deref() {
                        Some(id) => session_manager.branch(id)?,
                        None => session_manager.reset_leaf(),
                    }
                    if let Some(label) = &label {
                        let _ = session_manager.append_label_change(target_id, Some(label));
                    }
                    None
                }
            };

            // Update agent state.
            let messages = {
                let session_manager = self.inner.session_manager.lock().expect("session lock");
                session_manager.session_context().messages
            };
            self.inner.agent.set_messages(
                messages
                    .iter()
                    .cloned()
                    .map(coding_message_to_agent)
                    .collect(),
            );

            // Emit `session_tree`.
            {
                let new_leaf = self.get_leaf_id();
                let summary_entry = summary_entry_id.as_ref().and_then(|id| {
                    let session_manager =
                        self.inner.session_manager.lock().expect("session lock");
                    match session_manager.get_entry(id) {
                        Some(SessionEntry::BranchSummary(entry)) => {
                            Some(branch_summary_entry_to_json(entry))
                        }
                        _ => None,
                    }
                });
                let runner = self.inner.extension_runner.lock().expect("runner lock");
                let _ = runner.emit(&serde_json::json!({
                    "type": "session_tree",
                    "newLeafId": new_leaf,
                    "oldLeafId": old_leaf_id,
                    "summaryEntry": summary_entry,
                    "fromExtension": if summary_text.is_some() { Value::Bool(from_extension) } else { Value::Null },
                }));
            }

            Ok(TreeNavigationResult {
                editor_text,
                cancelled: false,
                aborted: false,
                summary_entry_id,
            })
        }
        .await;

        self.inner
            .state
            .lock()
            .expect("session state")
            .branch_summary_abort = None;

        result
    }

    /// Manually compact the session context (upstream `compact`; the entry
    /// point used by `/compact`, RPC, and extensions).
    #[allow(clippy::too_many_lines)]
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, String> {
        self.abort().await;
        let compaction_signal = AbortSignal::new();
        self.inner
            .state
            .lock()
            .expect("session state")
            .compaction_abort = Some(compaction_signal.clone());
        self.inner
            .emit(&AgentSessionEvent::CompactionStart { reason: "manual" });
        let mut from_extension = false;

        let result: Result<CompactionResult, String> = (async {
            let model = self.model().ok_or_else(format_no_model_selected_message)?;
            let (request_model, api_key, headers, env) =
                self.get_summarization_request_auth(&model).await?;

            let settings = self
                .inner
                .settings_manager
                .lock()
                .expect("settings lock")
                .compaction_settings();

            let (preparation, path_entries) = {
                let session_manager = self.inner.session_manager.lock().expect("session lock");
                let path_entries: Vec<SessionEntry> = session_manager
                    .get_branch(None)
                    .into_iter()
                    .cloned()
                    .collect();
                let context = session_manager.session_context();
                let messages = context.messages;
                let resolver = first_kept_entry_id_resolver(&path_entries);
                (
                    prepare_compaction(&messages, compaction_driver_settings(settings), resolver),
                    path_entries,
                )
            };
            let Some(preparation) = preparation else {
                if matches!(path_entries.last(), Some(SessionEntry::Compaction(_))) {
                    return Err("Already compacted".to_string());
                }
                return Err("Nothing to compact (session too small)".to_string());
            };

            let mut extension_compaction: Option<CompactionResult> = None;
            {
                let runner = self.inner.extension_runner.lock().expect("runner lock");
                if runner.has_handlers("session_before_compact") {
                    let extension_result = runner.emit(&serde_json::json!({
                        "type": "session_before_compact",
                        "preparation": preparation_to_json(&preparation),
                        "branchEntries": branch_entries_to_json(&path_entries),
                        "customInstructions": custom_instructions,
                        "reason": "manual",
                        "willRetry": false,
                    }));
                    if let Some(result) = extension_result {
                        if result
                            .get("cancel")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            return Err("Compaction cancelled".to_string());
                        }
                        if let Some(compaction) = result.get("compaction") {
                            if let Some(compaction) = compaction_result_from_json(compaction) {
                                extension_compaction = Some(compaction);
                                from_extension = true;
                            }
                        }
                    }
                }
            }

            let (summary, first_kept_entry_id, tokens_before, usage, details) =
                if let Some(extension_compaction) = extension_compaction {
                    (
                        extension_compaction.summary,
                        extension_compaction.first_kept_entry_id,
                        extension_compaction.tokens_before,
                        extension_compaction.usage,
                        extension_compaction.details,
                    )
                } else {
                    let result = self
                        .run_default_compaction(
                            preparation,
                            &request_model,
                            api_key,
                            headers,
                            custom_instructions,
                            compaction_signal.clone(),
                            env,
                        )
                        .await?;
                    (
                        result.summary,
                        result.first_kept_entry_id,
                        result.tokens_before,
                        result.usage,
                        result.details,
                    )
                };

            if compaction_signal.is_aborted() {
                return Err("Compaction cancelled".to_string());
            }

            {
                let mut session_manager = self.inner.session_manager.lock().expect("session lock");
                let _ = session_manager.append_compaction(
                    &summary,
                    &first_kept_entry_id,
                    tokens_before,
                    compaction_details_to_json(&details),
                    from_extension,
                    usage.clone(),
                );
            }
            let new_messages = {
                let session_manager = self.inner.session_manager.lock().expect("session lock");
                session_manager.session_context().messages
            };
            self.inner.agent.set_messages(
                new_messages
                    .iter()
                    .cloned()
                    .map(coding_message_to_agent)
                    .collect(),
            );
            let estimated_tokens_after: u64 = new_messages.iter().map(estimate_tokens).sum();

            {
                let session_manager = self.inner.session_manager.lock().expect("session lock");
                let entries = session_manager.get_entries_owned();
                let saved = entries
                    .iter()
                    .rev()
                    .find(|e| matches!(e, SessionEntry::Compaction(c) if c.summary == summary))
                    .cloned();
                let runner = self.inner.extension_runner.lock().expect("runner lock");
                if let Some(SessionEntry::Compaction(saved)) = saved {
                    let _ = runner.emit(&serde_json::json!({
                        "type": "session_compact",
                        "compactionEntry": compaction_entry_to_json(&saved),
                        "fromExtension": from_extension,
                        "reason": "manual",
                        "willRetry": false,
                    }));
                }
            }

            Ok(CompactionResult {
                summary,
                first_kept_entry_id,
                tokens_before,
                estimated_tokens_after: Some(estimated_tokens_after),
                usage,
                details,
            })
        })
        .await;

        self.inner
            .state
            .lock()
            .expect("session state")
            .compaction_abort = None;

        match result {
            Ok(compaction_result) => {
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: "manual",
                    result: Some(compaction_result.clone()),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                Ok(compaction_result)
            }
            Err(error) => {
                let aborted = error == "Compaction cancelled";
                let error_message = if aborted {
                    None
                } else {
                    Some(format!("Compaction failed: {error}"))
                };
                self.inner.emit(&AgentSessionEvent::CompactionEnd {
                    reason: "manual",
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: error_message.clone(),
                });
                self.inner.emit_session_compact_failed(
                    "manual",
                    error_message,
                    aborted,
                    false,
                    from_extension,
                );
                Err(error)
            }
        }
    }
}

// ============================================================================
// Module helpers
// ============================================================================

/// Build an agent user message with text and optional images.
fn agent_user_message(
    text: &str,
    images: Option<Vec<Content>>,
) -> pillar_agent::types::AgentMessage {
    let mut blocks = vec![Content::Text {
        text: text.to_string(),
        text_signature: None,
    }];
    if let Some(images) = images {
        blocks.extend(images);
    }
    pillar_agent::types::AgentMessage::Message(Message::User {
        content: UserContent::Blocks(blocks),
        timestamp: pillar_ai::models::now_ms(),
    })
}

/// Normalize an extension-provided message (upstream content `?? []`):
/// missing/null content becomes an empty array.
fn normalize_extension_message(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        if obj.get("content").is_none_or(|v| v.is_null()) {
            obj.insert("content".to_string(), Value::Array(Vec::new()));
        }
    }
    value
}

/// Strip a YAML frontmatter block from skill content (upstream
/// `stripFrontmatter`).
fn strip_frontmatter(content: &str) -> String {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---") else {
        return normalized.trim().to_string();
    };
    match rest.find("\n---") {
        Some(offset) => rest[offset + 4..].trim().to_string(),
        None => normalized.trim().to_string(),
    }
}

/// Convert pillar-agent custom content to coding-agent custom content.
fn user_content_to_custom_content(content: &UserContent) -> Vec<CustomContent> {
    match content {
        UserContent::Text(text) => vec![CustomContent::Text(text.clone())],
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text { text, .. } => Some(CustomContent::Text(text.clone())),
                Content::Image { data, mime_type } => Some(CustomContent::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                }),
                _ => None,
            })
            .collect(),
    }
}

/// Convert a pillar-agent message to the session message union.
pub(crate) fn agent_message_to_coding(
    message: pillar_agent::types::AgentMessage,
) -> CodingAgentMessage {
    match message {
        pillar_agent::types::AgentMessage::Message(message) => CodingAgentMessage::Base(message),
        pillar_agent::types::AgentMessage::BashExecution(bash) => {
            CodingAgentMessage::BashExecution(BashExecutionMessage {
                command: bash.command,
                output: bash.output,
                exit_code: bash.exit_code,
                cancelled: bash.cancelled,
                truncated: bash.truncated,
                full_output_path: bash.full_output_path,
                timestamp: bash.timestamp,
                exclude_from_context: bash.exclude_from_context,
            })
        }
        pillar_agent::types::AgentMessage::Custom(custom) => {
            CodingAgentMessage::Custom(CustomMessage {
                custom_type: custom.custom_type,
                content: user_content_to_custom_content(&custom.content),
                display: custom.display,
                details: custom.details,
                timestamp: custom.timestamp,
            })
        }
        pillar_agent::types::AgentMessage::BranchSummary(summary) => {
            CodingAgentMessage::BranchSummary(crate::core::messages::BranchSummaryMessage {
                summary: summary.summary,
                from_id: summary.from_id,
                timestamp: summary.timestamp,
            })
        }
        pillar_agent::types::AgentMessage::CompactionSummary(summary) => {
            CodingAgentMessage::CompactionSummary(crate::core::messages::CompactionSummaryMessage {
                summary: summary.summary,
                tokens_before: summary.tokens_before,
                timestamp: summary.timestamp,
            })
        }
    }
}

/// Convert a session message to a pillar-agent message.
pub(crate) fn coding_message_to_agent(
    message: CodingAgentMessage,
) -> pillar_agent::types::AgentMessage {
    match message {
        CodingAgentMessage::Base(message) => pillar_agent::types::AgentMessage::Message(message),
        CodingAgentMessage::BashExecution(bash) => {
            pillar_agent::types::AgentMessage::BashExecution(Box::new(
                pillar_agent::types::BashExecutionMessage {
                    command: bash.command,
                    output: bash.output,
                    exit_code: bash.exit_code,
                    cancelled: bash.cancelled,
                    truncated: bash.truncated,
                    full_output_path: bash.full_output_path,
                    timestamp: bash.timestamp,
                    exclude_from_context: bash.exclude_from_context,
                },
            ))
        }
        CodingAgentMessage::Custom(custom) => pillar_agent::types::AgentMessage::Custom(Box::new(
            pillar_agent::types::CustomMessage {
                custom_type: custom.custom_type,
                content: custom_content_to_user_content(custom.content),
                display: custom.display,
                details: custom.details,
                timestamp: custom.timestamp,
            },
        )),
        CodingAgentMessage::BranchSummary(summary) => {
            pillar_agent::types::AgentMessage::BranchSummary(Box::new(
                pillar_agent::types::BranchSummaryMessage {
                    summary: summary.summary,
                    from_id: summary.from_id,
                    timestamp: summary.timestamp,
                },
            ))
        }
        CodingAgentMessage::CompactionSummary(summary) => {
            pillar_agent::types::AgentMessage::CompactionSummary(Box::new(
                pillar_agent::types::CompactionSummaryMessage {
                    summary: summary.summary,
                    tokens_before: summary.tokens_before,
                    timestamp: summary.timestamp,
                },
            ))
        }
    }
}

/// Convert coding-agent custom content to a user-content value.
fn custom_content_to_user_content(content: Vec<CustomContent>) -> UserContent {
    if content.len() == 1 {
        if let CustomContent::Text(text) = &content[0] {
            return UserContent::Text(text.clone());
        }
    }
    let blocks: Vec<Content> = content
        .into_iter()
        .map(|block| match block {
            CustomContent::Text(text) => Content::Text {
                text,
                text_signature: None,
            },
            CustomContent::Image { data, mime_type } => Content::Image { data, mime_type },
        })
        .collect();
    UserContent::Blocks(blocks)
}

/// Build a `Model` from the agent's model ref (upstream the model object;
/// the port's registry model carries the same fields).
fn faux_model_to_model(model: &pillar_agent::types::FauxModelRef) -> pillar_ai::types::Model {
    pillar_ai::types::Model {
        id: model.id.clone(),
        name: model.name.clone(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        base_url: model.base_url.clone(),
        reasoning: model.reasoning,
        thinking_level_map: None,
        input: model.input.clone(),
        cost: pillar_ai::types::ModelCost {
            rates: pillar_ai::types::ModelCostRates {
                input: model.cost.input,
                output: model.cost.output,
                cache_read: model.cost.cache_read,
                cache_write: model.cost.cache_write,
            },
            tiers: None,
        },
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Resolve the entry id for a context-message cut index (upstream the
/// entry-id mapping `prepareCompaction` does over session entries).
fn first_kept_entry_id_resolver(
    path_entries: &[SessionEntry],
) -> impl Fn(usize) -> Option<String> + use<'_> {
    let spans: Vec<(String, usize)> = path_entries
        .iter()
        .filter_map(|entry| {
            let count = session_entry_to_context_messages(entry).len();
            if count == 0 {
                None
            } else {
                Some((entry.id().to_string(), count))
            }
        })
        .collect();
    move |index| {
        let mut cursor = 0usize;
        for (id, count) in &spans {
            if index < cursor + count {
                return Some(id.clone());
            }
            cursor += count;
        }
        None
    }
}

/// Parse a settings queue-mode string into a [`pillar_agent::types::QueueMode`].
fn queue_mode_from_str(mode: &str) -> pillar_agent::types::QueueMode {
    match mode {
        "all" => pillar_agent::types::QueueMode::All,
        _ => pillar_agent::types::QueueMode::OneAtATime,
    }
}

/// Parse a pi thinking-level string into the LLM-level enum.
fn thinking_level_from_str(level: &str) -> Option<ThinkingLevel> {
    match level {
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

/// Convert loaded resource prompts to the template type used by
/// `expand_prompt_template`.
fn loaded_prompts_to_templates(prompts: &[LoadedPrompt]) -> Vec<PromptTemplate> {
    prompts
        .iter()
        .map(|p| PromptTemplate {
            name: p.name.clone(),
            description: p.description.clone(),
            argument_hint: p.argument_hint.clone(),
            content: p.content.clone(),
            file_path: p.file_path.clone(),
        })
        .collect()
}

/// Map settings-manager compaction settings to the driver type.
fn compaction_driver_settings(
    settings: crate::core::settings_manager::CompactionSettings,
) -> compaction_driver::CompactionSettings {
    compaction_driver::CompactionSettings {
        enabled: settings.enabled,
        reserve_tokens: settings.reserve_tokens,
        keep_recent_tokens: settings.keep_recent_tokens,
    }
}

/// Serialize a compaction preparation for the extension boundary.
fn preparation_to_json(preparation: &CompactionPreparation) -> Value {
    serde_json::json!({
        "firstKeptEntryId": preparation.first_kept_entry_id,
        "tokensBefore": preparation.tokens_before,
        "settings": {
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        },
    })
}

/// Map `resources_discover` entries to loader metadata (upstream
/// `buildExtensionResourcePaths`).
fn build_extension_resource_paths(entries: Vec<(String, String)>) -> Vec<(String, PathMetadata)> {
    entries
        .into_iter()
        .map(|(path, extension_path)| {
            let base_dir = if extension_path.starts_with('<') {
                None
            } else {
                std::path::Path::new(&extension_path)
                    .parent()
                    .map(|parent| parent.to_path_buf())
            };
            (
                path,
                PathMetadata {
                    source: extension_source_label(&extension_path),
                    scope: SourceScope::Temporary,
                    origin: ResourceOrigin::TopLevel,
                    base_dir,
                },
            )
        })
        .collect()
}

/// Upstream `getExtensionSourceLabel`: `<inline>` paths keep their inner
/// name, file paths use the basename without the extension suffix.
fn extension_source_label(extension_path: &str) -> String {
    if extension_path.starts_with('<') {
        return format!("extension:{}", extension_path.replace(['<', '>'], ""));
    }
    let base = std::path::Path::new(extension_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(extension_path);
    let name = base
        .strip_suffix(".luau")
        .or_else(|| base.strip_suffix(".ts"))
        .or_else(|| base.strip_suffix(".js"))
        .unwrap_or(base);
    format!("extension:{name}")
}

/// Serialize branch entries for the extension boundary.
fn branch_entries_to_json(entries: &[SessionEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| {
                let kind = match entry {
                    SessionEntry::Message(_) => "message",
                    SessionEntry::ThinkingLevelChange(_) => "thinking_level_change",
                    SessionEntry::ModelChange(_) => "model_change",
                    SessionEntry::Compaction(_) => "compaction",
                    SessionEntry::BranchSummary(_) => "branch_summary",
                    SessionEntry::Custom(_) => "custom",
                    SessionEntry::Label(_) => "label",
                    SessionEntry::SessionInfo(_) => "session_info",
                    SessionEntry::CustomMessage(_) => "custom_message",
                };
                serde_json::json!({
                    "type": kind,
                    "id": entry.id(),
                })
            })
            .collect(),
    )
}

/// Parse an extension-provided compaction result.
fn compaction_result_from_json(value: &Value) -> Option<CompactionResult> {
    Some(CompactionResult {
        summary: value.get("summary")?.as_str()?.to_string(),
        first_kept_entry_id: value.get("firstKeptEntryId")?.as_str()?.to_string(),
        tokens_before: value.get("tokensBefore")?.as_u64()?,
        estimated_tokens_after: value.get("estimatedTokensAfter").and_then(Value::as_u64),
        usage: value
            .get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok()),
        details: value.get("details").and_then(compaction_details_from_json),
    })
}

fn compaction_details_from_json(
    value: &Value,
) -> Option<crate::core::compaction::driver::CompactionDetails> {
    Some(crate::core::compaction::driver::CompactionDetails {
        read_files: value
            .get("readFiles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        modified_files: value
            .get("modifiedFiles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn compaction_details_to_json(
    details: &Option<crate::core::compaction::driver::CompactionDetails>,
) -> Option<Value> {
    details.as_ref().map(|d| {
        serde_json::json!({
            "readFiles": d.read_files,
            "modifiedFiles": d.modified_files,
        })
    })
}

/// Serialize a saved compaction entry for the extension boundary.
fn compaction_entry_to_json(entry: &crate::core::session_entries::CompactionEntry) -> Value {
    serde_json::json!({
        "id": entry.base.id,
        "timestamp": entry.base.timestamp,
        "summary": entry.summary,
        "firstKeptEntryId": entry.first_kept_entry_id,
        "tokensBefore": entry.tokens_before,
        "fromHook": entry.from_hook,
        "usage": entry.usage,
    })
}

/// Serialize a tree preparation for the `session_before_tree` payload.
fn tree_preparation_to_json(preparation: &crate::core::extensions_types::TreePreparation) -> Value {
    serde_json::json!({
        "targetId": preparation.target_id,
        "oldLeafId": preparation.old_leaf_id,
        "commonAncestorId": preparation.common_ancestor_id,
        "entriesToSummarize": branch_entries_to_json(&preparation.entries_to_summarize),
        "userWantsSummary": preparation.user_wants_summary,
        "customInstructions": preparation.custom_instructions,
        "replaceInstructions": preparation.replace_instructions,
        "label": preparation.label,
    })
}

/// Serialize a branch summary entry for the `session_tree` payload.
fn branch_summary_entry_to_json(entry: &crate::core::session_entries::BranchSummaryEntry) -> Value {
    serde_json::json!({
        "type": "branch_summary",
        "id": entry.base.id,
        "parentId": entry.base.parent_id,
        "timestamp": entry.base.timestamp,
        "fromId": entry.from_id,
        "summary": entry.summary,
        "details": entry.details,
        "usage": entry.usage,
        "fromHook": entry.from_hook,
    })
}

/// A [`SummarizeFn`] backed by the agent's stream function (upstream
/// `agent.streamFunction` for summarization).
struct SummarizeStreamFn {
    stream: Option<pillar_agent::types::StreamFn>,
}

impl SummarizeFn for SummarizeStreamFn {
    fn call(
        &self,
        model: &pillar_ai::types::Model,
        context: &pillar_ai::types::Context,
        options: &SummarizationOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AssistantMessage, String>> + Send>>
    {
        let Some(stream) = self.stream.clone() else {
            let model_id = model.id.clone();
            return Box::pin(async move {
                Err(format!(
                    "No stream function available for summarization of model {model_id}"
                ))
            });
        };
        let context = context.clone();
        // Bridge the pillar-ai abort signal to the agent-loop signal type
        // (divergence: the two layers use distinct signal types).
        let agent_signal = pillar_agent::AbortSignal::new();
        if let Some(signal) = options.signal.clone() {
            let agent_signal = agent_signal.clone();
            tokio::spawn(async move {
                let _ = signal.aborted_or_pending().await;
                agent_signal.abort();
            });
        }
        let call_options = pillar_agent::types::StreamCallOptions {
            simple: pillar_ai::types::SimpleStreamOptionsLike {
                api_key: options.api_key.clone(),
                temperature: None,
                max_tokens: options.max_tokens,
                session_id: options.session_id.clone(),
                reasoning: options
                    .reasoning
                    .as_deref()
                    .and_then(thinking_level_from_str),
            },
            abort: Some(agent_signal),
            on_payload: None,
            on_response: None,
            transport: None,
            thinking_budgets: None,
            max_retry_delay_ms: None,
            session_id: options.session_id.clone(),
        };
        Box::pin(async move {
            let event_stream = stream.call(context, Some(call_options)).await;
            Ok(event_stream.result().await)
        })
    }
}

/// Map the session's extension mode string onto the extension-facing mode
/// (upstream `ExtensionMode`).
fn extension_mode(mode: &str) -> crate::core::extensions_types::ExtensionMode {
    use crate::core::extensions_types::ExtensionMode;
    match mode {
        "tui" | "interactive" => ExtensionMode::Tui,
        "rpc" => ExtensionMode::Rpc,
        "json" => ExtensionMode::Json,
        _ => ExtensionMode::Print,
    }
}
