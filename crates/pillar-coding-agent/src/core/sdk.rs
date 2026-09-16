//! Port of packages/coding-agent/src/core/sdk.ts (pi v0.84.3), the
//! session-assembly decision core: initial model/thinking-level/tool
//! selection from settings and session state, and the
//! convertToLlm block-images filter.
//!
//! divergences: model lookup (ModelRuntime) and the streaming fn wiring
//! are host-injected; the port covers the pure resolution logic over a
//! host-provided model lookup.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use pillar_agent::AgentThinkingLevel;
use pillar_agent::agent::{Agent, AgentOptions, AgentState};
use pillar_agent::types::{AgentTool, FauxModelRef, QueueMode, StreamFn};
use pillar_ai::models::clamp_thinking_level;
use pillar_ai::types::{Message, Model};

use crate::core::agent_session_class::{
    AgentSession, AgentSessionConfig, ExtensionCommandHandler, ExtensionRunnerFactory,
    SessionEventMeta, SystemPromptRebuildFn, coding_message_to_agent,
};
use crate::core::auth_guidance::format_no_models_available_message;
use crate::core::extensions_runner::ExtensionRunner;
use crate::core::extras::{PILLAR_TELEMETRY_ENV, is_install_telemetry_enabled};
use crate::core::model_mutation::ScopedModel;
use crate::core::model_runtime::ModelRuntime;
use crate::core::provider_attribution::merge_provider_attribution_headers;
use crate::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use crate::core::session_entries::SessionEntry;
use crate::core::session_manager::SessionManager;
use crate::core::session_support::{DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::system_prompt::{BuildSystemPromptOptions, PromptPaths, build_system_prompt};

/// Upstream the `transformHeaders` callback of the session stream function:
/// attribution headers under the assembled provider/request headers (the
/// OpenCode session id is the one pi routes requests with).
fn provider_headers_transform(
    model: Model,
    settings: Arc<Mutex<SettingsManager>>,
    session_id: String,
) -> pillar_ai::models::HeadersTransform {
    Arc::new(move |request_headers: pillar_ai::types::ProviderHeaders| {
        let model = model.clone();
        let settings = Arc::clone(&settings);
        let session_id = session_id.clone();
        Box::pin(async move {
            let telemetry_enabled = {
                let settings = settings.lock().expect("settings lock");
                is_install_telemetry_enabled(
                    settings.enable_install_telemetry(),
                    std::env::var(PILLAR_TELEMETRY_ENV).ok().as_deref(),
                )
            };
            merge_provider_attribution_headers(
                &model,
                telemetry_enabled,
                Some(&session_id),
                &[&request_headers],
            )
            .unwrap_or_default()
        })
    })
}

// ============================================================================
// Model resolution (upstream createAgentSession model restoration)
// ============================================================================}

/// The resolved initial model + fallback message (upstream the model
/// restoration block of createAgentSession).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedInitialModel {
    pub model: Option<Model>,
    pub model_fallback_message: Option<String>,
}

/// Resolve the initial model (upstream the createAgentSession model
/// block): a session with saved model data tries
/// `provider/modelId` through the host lookup with configured auth; the
/// fallback message records what could not be restored, then records the
/// actually selected model.
pub fn resolve_initial_model(
    existing_session_model: Option<(&str, &str)>,
    explicit_model: Option<Model>,
    has_configured_auth: impl Fn(&str) -> bool,
    lookup: impl Fn(&str, &str) -> Option<Model>,
) -> ResolvedInitialModel {
    let mut fallback: Option<String> = None;
    if let Some(model) = explicit_model {
        return ResolvedInitialModel {
            model: Some(model),
            model_fallback_message: None,
        };
    }
    if let Some((provider, model_id)) = existing_session_model {
        let restored = lookup(provider, model_id).filter(|m| has_configured_auth(&m.provider));
        if restored.is_none() {
            fallback = Some(format!("Could not restore model {provider}/{model_id}"));
        } else {
            return ResolvedInitialModel {
                model: restored,
                model_fallback_message: None,
            };
        }
    }
    // findInitialModel (settings default / provider defaults) is
    // host-driven; the caller supplies the outcome via a second call if
    // needed. Here we surface the fallback message shape.
    ResolvedInitialModel {
        model: None,
        model_fallback_message: fallback,
    }
}

/// Compose the fallback message with the eventually selected model
/// (upstream: "... . Using provider/id").
pub fn fallback_message_using(fallback: &str, model: &Model) -> String {
    format!("{fallback}. Using {}/{}", model.provider, model.id)
}

// ============================================================================
// Thinking level resolution (upstream createAgentSession thinking block)
// ============================================================================}

/// Inputs for thinking-level resolution (upstream the settings lookups).
#[derive(Debug, Clone, Default)]
pub struct ThinkingLevelInputs<'a> {
    pub explicit_level: Option<String>,
    pub has_existing_session: bool,
    pub has_thinking_entry: bool,
    pub session_thinking_level: Option<String>,
    pub per_model_override: Option<&'a str>,
    pub default_level: Option<String>,
}

/// Parse a thinking-level string (upstream the ThinkingLevel union).
fn parse_thinking_level(level: &str) -> Option<pillar_ai::types::ModelThinkingLevel> {
    use pillar_ai::types::ModelThinkingLevel;
    let level = level.to_lowercase();
    match level.as_str() {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Format a thinking level back to its pi string.
fn thinking_level_to_string(level: pillar_ai::types::ModelThinkingLevel) -> String {
    use pillar_ai::types::ModelThinkingLevel;
    match level {
        ModelThinkingLevel::Off => "off".to_string(),
        ModelThinkingLevel::Minimal => "minimal".to_string(),
        ModelThinkingLevel::Low => "low".to_string(),
        ModelThinkingLevel::Medium => "medium".to_string(),
        ModelThinkingLevel::High => "high".to_string(),
        ModelThinkingLevel::Xhigh => "xhigh".to_string(),
        ModelThinkingLevel::Max => "max".to_string(),
    }
}

/// Resolve the initial thinking level (upstream the thinking-level chain):
/// explicit → session entry (or settings default when the session has no
/// entry) → per-model override → global default → DEFAULT_THINKING_LEVEL;
/// no model clamps to "off", otherwise the level is clamped to the model's
/// capabilities.
pub fn resolve_thinking_level(inputs: ThinkingLevelInputs<'_>, model: Option<&Model>) -> String {
    let mut level = if inputs.has_existing_session {
        if inputs.has_thinking_entry {
            inputs.session_thinking_level.clone()
        } else {
            inputs.default_level.clone()
        }
    } else {
        None
    };
    if level.is_none() {
        if let Some(per_model) = inputs.per_model_override {
            if model.is_some() {
                level = Some(per_model.to_string());
            }
        }
    }
    if level.is_none() {
        level = Some(
            inputs
                .default_level
                .clone()
                .unwrap_or_else(|| DEFAULT_THINKING_LEVEL.to_string()),
        );
    }
    let level = level.unwrap_or_else(|| DEFAULT_THINKING_LEVEL.to_string());
    match model {
        None => "off".to_string(),
        // Unknown levels keep the default; THINKING_LEVEL_OPTIONS defines
        // the valid set (upstream ThinkingLevel union).
        Some(model) => {
            if !THINKING_LEVEL_OPTIONS.contains(&level.as_str()) {
                return DEFAULT_THINKING_LEVEL.to_string();
            }
            let parsed = parse_thinking_level(&level).expect("validated level");
            thinking_level_to_string(clamp_thinking_level(model, parsed))
        }
    }
}

// ============================================================================
// Tool selection (upstream createAgentSession tool block)
// ============================================================================}

/// The default built-in tool names (upstream `defaultActiveToolNames`).
pub const DEFAULT_ACTIVE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// Tool-selection inputs (upstream `tools` / `noTools` /
/// `excludeTools` / the `defaultTools` setting).
#[derive(Debug, Clone, Default)]
pub struct ToolSelectionInputs<'a> {
    pub tools: Option<&'a [String]>,
    pub no_tools: Option<&'a str>,
    pub exclude_tools: &'a [String],
    pub configured_default_tools: Option<&'a [String]>,
}

/// Resolve initial active tool names (upstream `initialActiveToolNames`):
/// an explicit allowlist wins; `noTools` starts with none; otherwise the
/// configured defaultTools setting or the built-in defaults apply; the
/// exclude list filters afterward.
pub fn resolve_initial_active_tool_names(inputs: ToolSelectionInputs<'_>) -> Vec<String> {
    let base: Vec<String> = if let Some(tools) = inputs.tools {
        tools.to_vec()
    } else if let Some(mode) = inputs.no_tools {
        match mode {
            "all" | "builtin" => Vec::new(),
            _ => inputs
                .configured_default_tools
                .map(|t| t.to_vec())
                .unwrap_or_else(|| {
                    DEFAULT_ACTIVE_TOOL_NAMES
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }),
        }
    } else {
        inputs
            .configured_default_tools
            .map(|t| t.to_vec())
            .unwrap_or_else(|| {
                DEFAULT_ACTIVE_TOOL_NAMES
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            })
    };
    base.into_iter()
        .filter(|name| !inputs.exclude_tools.contains(name))
        .collect()
}

// ============================================================================
// convertToLlm block-images filter (upstream convertToLlmWithBlockImages)
// ============================================================================}

/// Filter images out of converted LLM messages when blockImages is set
/// (upstream `convertToLlmWithBlockImages`): image content is replaced by
/// a text placeholder, with consecutive placeholders deduped. Messages
/// with mixed content (user Blocks / tool result arrays) have image items
/// replaced in place.
pub fn filter_blocked_images(messages: Vec<Message>, block_images: bool) -> Vec<Message> {
    if !block_images {
        return messages;
    }
    const PLACEHOLDER: &str = "Image reading is disabled.";
    messages
        .into_iter()
        .map(|message| match message {
            Message::User { content, timestamp } => {
                let content = match content {
                    pillar_ai::types::UserContent::Blocks(items) => {
                        let mut replaced: Vec<pillar_ai::types::Content> = Vec::new();
                        for item in items {
                            match item {
                                pillar_ai::types::Content::Image { .. } => {
                                    let is_duplicate = replaced.last().is_some_and(|previous| {
                                        matches!(previous, pillar_ai::types::Content::Text {
                                            text,
                                            ..
                                        } if text == PLACEHOLDER)
                                    });
                                    if !is_duplicate {
                                        replaced.push(pillar_ai::types::Content::text(PLACEHOLDER));
                                    }
                                }
                                other => replaced.push(other),
                            }
                        }
                        pillar_ai::types::UserContent::Blocks(replaced)
                    }
                    other => other,
                };
                Message::User { content, timestamp }
            }
            Message::ToolResult(mut tool_result) => {
                let mut replaced: Vec<pillar_ai::types::Content> = Vec::new();
                for item in std::mem::take(&mut tool_result.content) {
                    match item {
                        pillar_ai::types::Content::Image { .. } => {
                            let is_duplicate = replaced.last().is_some_and(|previous| {
                                matches!(previous, pillar_ai::types::Content::Text {
                                    text,
                                    ..
                                } if text == PLACEHOLDER)
                            });
                            if !is_duplicate {
                                replaced.push(pillar_ai::types::Content::text(PLACEHOLDER));
                            }
                        }
                        other => replaced.push(other),
                    }
                }
                tool_result.content = replaced;
                Message::ToolResult(tool_result)
            }
            other => other,
        })
        .collect()
}

// ============================================================================
// Settings accessors used by the SDK (upstream getters)
// ============================================================================}

/// The settings lookups needed for session assembly, taken from a
/// SettingsManager (upstream getters: getDefaultTools /
/// getDefaultThinkingLevel / getModelThinkingLevel / getBlockImages).
pub struct SdkSettings {
    pub default_tools: Option<Vec<String>>,
    pub default_thinking_level: Option<String>,
    pub block_images: bool,
}

/// Extract the SDK-relevant settings snapshot.
pub fn sdk_settings(settings: &SettingsManager) -> SdkSettings {
    let default_tools: Option<Vec<String>> = settings
        .get_global_setting("defaultTools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    let default_thinking_level = settings
        .get_global_setting("defaultThinkingLevel")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let block_images = settings
        .get_global_setting("images")
        .and_then(|v| v.get("blockImages"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    SdkSettings {
        default_tools,
        default_thinking_level,
        block_images,
    }
}

// ============================================================================
// createAgentSession (upstream the factory)
// ============================================================================

/// Options for [`create_agent_session`] (upstream
/// `CreateAgentSessionOptions`, capability subset).
///
/// divergence: the extension runner and the model runtime are host-injected
/// (the port cannot build the Luau runner from this crate), and the
/// blockImages convertToLlm filter is not applied here.
pub struct CreateAgentSessionOptions {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub model_runtime: Arc<ModelRuntime>,
    pub settings_manager: Option<Arc<Mutex<SettingsManager>>>,
    pub session_manager: Option<SessionManager>,
    pub resource_loader: Option<Arc<Mutex<ResourceLoader>>>,
    pub model: Option<Model>,
    pub thinking_level: Option<String>,
    pub scoped_models: Vec<ScopedModel>,
    pub tools: Option<Vec<String>>,
    /// `"all"` or `"builtin"` (upstream `noTools`).
    pub no_tools: Option<String>,
    pub exclude_tools: Vec<String>,
    pub custom_tools: Vec<AgentTool>,
    pub extension_runner: Arc<Mutex<ExtensionRunner>>,
    /// Host handler executing a registered extension command (upstream the
    /// runner invoking `RegisteredCommand.handler`).
    pub command_handler: Option<ExtensionCommandHandler>,
    pub session_start_event: Option<SessionEventMeta>,
    pub system_prompt_rebuild: Option<SystemPromptRebuildFn>,
    pub extension_runner_rebuild: Option<ExtensionRunnerFactory>,
    /// Optional stream-fn override (tests / faux providers). Defaults to the
    /// model runtime's simple stream over the agent's current model.
    pub stream_fn: Option<StreamFn>,
}

/// Result of [`create_agent_session`] (upstream
/// `CreateAgentSessionResult`; `extensionsResult` is host-owned in the
/// port).
pub struct CreateAgentSessionResult {
    pub session: AgentSession,
    pub model_fallback_message: Option<String>,
}

fn queue_mode(value: &str) -> QueueMode {
    if value == "one-at-a-time" {
        QueueMode::OneAtATime
    } else {
        QueueMode::All
    }
}

fn resolve_model_default(
    model_runtime: &ModelRuntime,
    settings: &SettingsManager,
) -> Option<Model> {
    let configured = |model: &Model| model_runtime.has_configured_auth(&model.provider);
    settings
        .default_model_and_provider()
        .and_then(|(provider, model_id)| model_runtime.get_model(&provider, &model_id))
        .filter(configured)
        .or_else(|| {
            model_runtime
                .get_available_snapshot()
                .into_iter()
                .find(configured)
        })
}

/// Create an agent session (upstream `createAgentSession`): model/thinking
/// resolution, tool selection, system prompt, agent assembly, and the
/// session wiring.
pub async fn create_agent_session(
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, String> {
    let cwd = options.cwd.clone();
    let agent_dir = options
        .agent_dir
        .clone()
        .or_else(default_agent_dir_string)
        .unwrap_or_else(|| cwd.clone());
    let model_runtime = Arc::clone(&options.model_runtime);

    let settings_manager = match options.settings_manager {
        Some(manager) => manager,
        None => Arc::new(Mutex::new(SettingsManager::create(
            &cwd,
            Path::new(&agent_dir),
            SettingsManagerCreateOptions {
                project_trusted: Some(true),
            },
        ))),
    };

    let session_manager = match options.session_manager {
        Some(manager) => manager,
        None => SessionManager::create(&cwd, None, None)?,
    };

    let resource_loader = match options.resource_loader {
        Some(loader) => loader,
        None => {
            let mut loader = ResourceLoader::new(
                &cwd,
                ResourceLoaderOptions {
                    agent_dir: agent_dir.clone(),
                    ..Default::default()
                },
                Arc::clone(&settings_manager),
            );
            loader.reload(None)?;
            Arc::new(Mutex::new(loader))
        }
    };

    // Restore model/thinking/tools from the session and settings.
    let context = session_manager.session_context();
    let has_existing_session = !context.messages.is_empty();
    let has_thinking_entry = session_manager
        .get_branch(None)
        .iter()
        .any(|entry| matches!(entry, SessionEntry::ThinkingLevelChange(_)));

    let mut model_fallback_message: Option<String> = None;
    let mut model = options.model.clone();
    if model.is_none() && has_existing_session {
        if let Some((provider, model_id)) = &context.model {
            let restored = model_runtime
                .get_model(provider, model_id)
                .filter(|candidate| model_runtime.has_configured_auth(&candidate.provider));
            if restored.is_none() {
                model_fallback_message =
                    Some(format!("Could not restore model {provider}/{model_id}"));
            } else {
                model = restored;
            }
        }
    }
    if model.is_none() {
        let settings = settings_manager.lock().expect("settings lock");
        model = resolve_model_default(&model_runtime, &settings);
        drop(settings);
        if let (Some(fallback), Some(selected)) = (&model_fallback_message, &model) {
            model_fallback_message = Some(fallback_message_using(fallback, selected));
        }
    }
    if model.is_none() && model_fallback_message.is_none() {
        model_fallback_message = Some(format_no_models_available_message());
    }

    let settings_snapshot = {
        let settings = settings_manager.lock().expect("settings lock");
        sdk_settings(&settings)
    };
    let per_model_thinking = model.as_ref().and_then(|selected| {
        let settings = settings_manager.lock().expect("settings lock");
        settings.model_thinking_level(&selected.provider, &selected.id)
    });
    let thinking_level = resolve_thinking_level(
        ThinkingLevelInputs {
            explicit_level: options.thinking_level.clone(),
            has_existing_session,
            has_thinking_entry,
            session_thinking_level: Some(context.thinking_level.clone()),
            per_model_override: per_model_thinking.as_deref(),
            default_level: settings_snapshot.default_thinking_level.clone(),
        },
        model.as_ref(),
    );

    let initial_active_tool_names = resolve_initial_active_tool_names(ToolSelectionInputs {
        tools: options.tools.as_deref(),
        no_tools: options.no_tools.as_deref(),
        exclude_tools: &options.exclude_tools,
        configured_default_tools: settings_snapshot.default_tools.as_deref(),
    });

    let mut tools: Vec<AgentTool> = initial_active_tool_names
        .iter()
        .filter_map(|name| crate::core::tools::index::create_tool(name, &cwd))
        .collect();
    for tool in options.custom_tools {
        let keep = options.no_tools.as_deref() != Some("all")
            && !options.exclude_tools.contains(&tool.tool.name);
        if keep {
            tools.push(tool);
        }
    }

    let system_prompt = build_system_prompt(&BuildSystemPromptOptions {
        custom_prompt: None,
        selected_tools: Some(initial_active_tool_names.clone()),
        tool_snippets: None,
        prompt_guidelines: None,
        append_system_prompt: None,
        cwd: cwd.clone(),
        context_files: None,
        skills: None,
        paths: PromptPaths::default(),
    });

    // The agent reads its model from state at stream time, so the closure can
    // follow model switches and prepare_next_turn updates.
    let agent_cell: Arc<OnceLock<Arc<Agent>>> = Arc::new(OnceLock::new());
    let stream_fn = options.stream_fn.clone().unwrap_or_else(|| {
        let runtime = Arc::clone(&model_runtime);
        let cell = Arc::clone(&agent_cell);
        let settings = Arc::clone(&settings_manager);
        let session_id = session_manager.session_id().to_string();
        StreamFn::new(move |context, stream_options| {
            let runtime = Arc::clone(&runtime);
            let settings = Arc::clone(&settings);
            let session_id = session_id.clone();
            let model = cell.get().map(|agent| agent.state().model);
            // The agent's abort signal must reach the provider: it is what
            // cancels an in-flight request when the user presses Escape
            // (upstream passes `options.signal` straight through to the
            // stream function). The port's agent and provider layers use
            // separate signal types, so one is forwarded to the other.
            let signal = stream_options
                .as_ref()
                .and_then(|options| options.abort.as_ref())
                .map(bridge_abort_signal);
            async move {
                match model {
                    Some(model) if model.id != "unknown" => {
                        let request_model = model.to_model();
                        let options = pillar_ai::models::ModelsStreamOptions {
                            transform_headers: Some(provider_headers_transform(
                                request_model.clone(),
                                settings,
                                session_id,
                            )),
                            signal,
                            ..Default::default()
                        };
                        runtime.stream_simple(&request_model, &context, Some(options))
                    }
                    _ => no_model_stream(),
                }
            }
        })
    });

    let mut initial_state = AgentState {
        system_prompt: system_prompt.clone(),
        model: model
            .as_ref()
            .map(FauxModelRef::from_model)
            .unwrap_or_else(FauxModelRef::unknown),
        thinking_level: AgentThinkingLevel::parse(&thinking_level),
        tools: tools.clone(),
        messages: Vec::new(),
        ..Default::default()
    };
    if has_existing_session {
        initial_state.messages = context
            .messages
            .iter()
            .cloned()
            .map(coding_message_to_agent)
            .collect();
    }

    let mut agent_options = AgentOptions::new(stream_fn);
    agent_options.initial_state = Some(initial_state);
    agent_options.session_id = Some(session_manager.session_id().to_string());
    {
        let settings = settings_manager.lock().expect("settings lock");
        agent_options.steering_mode = Some(queue_mode(settings.steering_mode()));
        agent_options.follow_up_mode = Some(queue_mode(settings.follow_up_mode()));
    }
    let agent = Arc::new(Agent::new(agent_options));
    let _ = agent_cell.set(Arc::clone(&agent));

    // Persist the initial model/thinking for new sessions.
    let mut session_manager = session_manager;
    if !has_existing_session {
        if let Some(selected) = &model {
            let _ = session_manager.append_model_change(&selected.provider, &selected.id);
        }
        let _ = session_manager.append_thinking_level_change(&thinking_level);
    } else if !has_thinking_entry {
        let _ = session_manager.append_thinking_level_change(&thinking_level);
    }

    let excluded_tool_names = if options.exclude_tools.is_empty() {
        None
    } else {
        Some(options.exclude_tools.iter().cloned().collect())
    };
    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager: Arc::new(Mutex::new(session_manager)),
        settings_manager,
        cwd,
        resource_loader,
        model_runtime,
        extension_runner: options.extension_runner,
        initial_active_tool_names: Some(initial_active_tool_names),
        allowed_tool_names: None,
        excluded_tool_names,
        command_handler: options.command_handler,
        session_start_event: options.session_start_event,
        scoped_models: options.scoped_models,
        system_prompt_rebuild: options.system_prompt_rebuild,
        extension_runner_rebuild: options.extension_runner_rebuild,
    });
    session.install_tool_hooks();

    Ok(CreateAgentSessionResult {
        session,
        model_fallback_message,
    })
}

fn default_agent_dir_string() -> Option<String> {
    std::env::var_os("PILLAR_CODING_AGENT_DIR")
        .map(|value| value.to_string_lossy().to_string())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| Path::new(&home).join(".pillar").join("agent"))
                .map(|path| path.to_string_lossy().to_string())
        })
}

/// Forward the agent layer's abort signal onto a provider-layer signal
/// (upstream has a single `AbortSignal` shared by both; the port's
/// `pillar-agent` signal notifies through wakers, `pillar-ai`'s through a
/// watch channel, so one task bridges them).
fn bridge_abort_signal(source: &pillar_agent::AbortSignal) -> pillar_ai::abort::AbortSignal {
    let target = pillar_ai::abort::AbortSignal::new();
    if source.is_aborted() {
        target.abort(None);
        return target;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime to forward on (wasm / tests without tokio): the request
        // keeps running, which matches the pre-bridge behavior.
        return target;
    };
    let source = source.clone();
    let forward = target.clone();
    handle.spawn(async move {
        source.aborted().await;
        forward.abort(None);
    });
    target
}

fn no_model_stream() -> pillar_ai::event_stream::AssistantMessageEventStream {
    let stream = pillar_ai::event_stream::assistant_message_event_stream();
    let error = pillar_ai::types::AssistantMessage {
        content: Vec::new(),
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        usage: Default::default(),
        stop_reason: pillar_ai::types::StopReason::Error,
        deferred: None,
        error_message: Some("No model selected".to_string()),
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };
    stream.push(pillar_ai::types::AssistantMessageEvent::Start {
        partial: error.clone(),
    });
    stream.push(pillar_ai::types::AssistantMessageEvent::Error {
        reason: pillar_ai::types::StopReason::Error,
        error,
    });
    stream
}
