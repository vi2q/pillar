//! Port of packages/coding-agent/src/modes/interactive/interactive-mode.ts
//! (pi v0.84.3), starting with the pure helpers at the top of the module.
//!
//! Progress: the value helpers below are ported. The `InteractiveMode` class
//! (rendering loop, slash-command handling, selectors) is ported
//! incrementally; helpers that need not-yet-ported types (`AuthSelectorProvider`
//! login completions, `ExpandableText`) land with those types.

use std::io::IsTerminal;
use std::time::Instant;

use pillar_ai::types::Model;
use pillar_tui::autocomplete::AutocompleteItem;

use pillar_tui::editor::Editor;
pub use pillar_tui::tui::TuiMode;

/// Terminal-title callback (upstream `terminal.setTitle`).
pub type TerminalTitleCallback = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Terminal progress callback (upstream `terminal.setProgress`).
pub type TerminalProgressCallback = std::sync::Arc<dyn Fn(bool) + Send + Sync>;

use crate::cli::args::APP_NAME;
use crate::core::model_resolver::default_model_per_provider;
use crate::core::session_manager::SessionManager;
use crate::modes::interactive::theme::theme;

/// Warning shown when Anthropic subscription auth is in use (upstream
/// `ANTHROPIC_SUBSCRIPTION_AUTH_WARNING`).
pub const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str = "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

/// Terminal error codes that mean the terminal is gone (upstream
/// `DEAD_TERMINAL_ERROR_CODES`).
pub const DEAD_TERMINAL_ERROR_CODES: [&str; 3] = ["EIO", "EPIPE", "ENOTCONN"];

/// Whether an I/O error means the terminal died (upstream
/// `isDeadTerminalError`): EIO, EPIPE, or ENOTCONN.
pub fn is_dead_terminal_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::BrokenPipe | ErrorKind::NotConnected
    ) {
        return true;
    }
    // EIO has no `ErrorKind` variant; macOS/Linux agree on 5.
    error.raw_os_error() == Some(5)
}

/// Whether the API key is an Anthropic subscription (OAuth) token (upstream
/// `isAnthropicSubscriptionAuthKey`).
pub fn is_anthropic_subscription_auth_key(api_key: Option<&str>) -> bool {
    api_key.is_some_and(|key| key.starts_with("sk-ant-oat"))
}

/// Whether the model is the placeholder unknown model (upstream
/// `isUnknownModel`).
pub fn is_unknown_model(model: Option<&Model>) -> bool {
    model.is_some_and(|model| {
        model.provider == "unknown" && model.id == "unknown" && model.api == "unknown"
    })
}

/// Shell-quote a value only when it contains unsafe characters (upstream
/// `quoteIfNeeded`).
pub fn quote_if_needed(value: &str) -> String {
    let safe = !value.is_empty()
        && !value.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || "_\\-./~:@".contains(character))
        });
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The `pillar` resume command for a session, or `None` when resuming makes no
/// sense (upstream `formatResumeCommand`).
pub fn format_resume_command(session_manager: &SessionManager) -> Option<String> {
    format_resume_command_with(session_manager, std::io::stdout().is_terminal())
}

/// As [`format_resume_command`] with the TTY check supplied (upstream reads
/// `process.stdout.isTTY`).
pub fn format_resume_command_with(
    session_manager: &SessionManager,
    stdout_is_tty: bool,
) -> Option<String> {
    if !stdout_is_tty {
        return None;
    }
    if !session_manager.is_persisted() {
        return None;
    }
    let session_file = session_manager.session_file()?;
    if !session_file.exists() {
        return None;
    }

    let mut args = vec![APP_NAME.to_string()];
    if !session_manager.uses_default_session_dir() {
        args.push("--session-dir".to_string());
        args.push(quote_if_needed(
            &session_manager.session_dir().to_string_lossy(),
        ));
    }
    args.push("--session".to_string());
    args.push(session_manager.session_id().to_string());
    Some(args.join(" "))
}

/// Whether the provider has a built-in default model (upstream
/// `hasDefaultModelProvider`).
pub fn has_default_model_provider(provider_id: &str) -> bool {
    default_model_per_provider(provider_id).is_some()
}

/// Guidance shown after a llama.cpp login (upstream
/// `llamaCppPostLoginGuidance`).
pub fn llama_cpp_post_login_guidance(action_label: &str, loaded_model_count: usize) -> String {
    if loaded_model_count == 0 {
        format!(
            "{action_label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
        )
    } else {
        format!(
            "{action_label}. Use /model to select a loaded llama.cpp model, or /llama to manage models."
        )
    }
}

/// Filter items for the autocomplete (upstream
/// `createFuzzyAutocompleteItems`): `None` when nothing matches.
pub fn create_fuzzy_autocomplete_items<T>(
    items: &[T],
    prefix: &str,
    get_search_text: impl Fn(&T) -> String,
    to_autocomplete_item: impl Fn(&T) -> AutocompleteItem,
) -> Option<Vec<AutocompleteItem>> {
    let filtered = pillar_tui::fuzzy::fuzzy_filter_by(items, prefix, &get_search_text);
    if filtered.is_empty() {
        return None;
    }
    Some(filtered.into_iter().map(&to_autocomplete_item).collect())
}

// ============================================================================
// InteractiveMode (upstream the class body, assembled over slices)
// ============================================================================

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pillar_tui::components::{Spacer, Text};
use pillar_tui::tui::{ComponentId, TuiBase};

use crate::core::agent_session_class::{AgentSession, AgentSessionEvent, StreamingBehavior};
use crate::core::footer_data_provider::FooterDataProvider;
use crate::core::messages::{CodingAgentMessage, create_compaction_summary_message};
use crate::core::resource_loader::GitPaths;
use crate::core::session_entries::SessionEntry;
use crate::core::session_manager::SessionInfo;
use crate::core::settings_manager::DoubleEscapeAction;
use crate::core::truncate::TruncationResult;
use crate::modes::interactive::autocomplete::InteractiveAutocomplete;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::extension_input::{
    ExtensionInputComponent, ExtensionInputOutcome,
};
use crate::modes::interactive::components::extension_selector::{
    ExtensionSelectorComponent, ExtensionSelectorOutcome,
};
use crate::modes::interactive::components::footer::FooterComponent;
use crate::modes::interactive::components::model_picker::{
    CategoryKind, ModelPickerComponent, ModelPickerOutcome, PickerCategory, RECENT_CATEGORY_ID,
};
use crate::modes::interactive::components::scoped_models_selector::{
    ScopedModelsOutcome, ScopedModelsSelectorComponent,
};
use crate::modes::interactive::components::session_selector::{
    SessionScope, SessionSelectorComponent, SessionSelectorOutcome, StatusKind,
};
use crate::modes::interactive::components::settings_selector::{
    SettingsConfig, SettingsSelectorComponent, SettingsSelectorOutcome,
};
use crate::modes::interactive::components::status_indicator::{
    CompactionStatusReason, RetryStatusIndicator, StatusIndicatorKind,
    branch_summary_status_indicator, compaction_status_indicator,
};
use crate::modes::interactive::components::thinking_selector::{
    ThinkingSelectorComponent, ThinkingSelectorOutcome,
};
use crate::modes::interactive::components::tree_selector::{
    TreeSelectorComponent, TreeSelectorOutcome,
};
use crate::modes::interactive::components::user_message_selector::{
    UserMessageItem, UserMessageSelectorComponent, UserMessageSelectorOutcome,
};
use crate::modes::interactive::mode_ui::{PendingMessagesUi, QueueMode, StatusUi};
use crate::modes::interactive::model_picker_recent::RecentModels;
use crate::modes::interactive::theme::get_editor_theme;
use crate::modes::interactive::theme::{TerminalTheme, current_theme_name};
use crate::modes::interactive::transcript::{
    CompactionCostKind, InteractiveTranscript, Shared, TranscriptSettings,
};

/// What the host must execute after a mode dispatch (upstream the awaits
/// inside `handleEvent` and the submit handler; the port's mode stays
/// synchronous and reports the async work).
#[derive(Debug, Clone, PartialEq)]
pub enum ModeAction {
    /// `session.prompt(text, streamingBehavior?)`.
    Prompt {
        text: String,
        streaming_behavior: Option<StreamingBehavior>,
    },
    /// `session.steer(text)`.
    Steer(String),
    /// `session.followUp(text)`.
    FollowUp(String),
    /// `session.executeBash(command, excluded)`; the mode already put the
    /// command into the editor history.
    Bash { command: String, excluded: bool },
    /// `session.compact(instructions)`.
    Compact { instructions: Option<String> },
    /// Upstream `/quit` → `shutdown()`.
    Shutdown,
    /// The normal message submission (upstream
    /// `pendingUserInputs.push(text)` / `onInputCallback`).
    SubmitToLoop(String),
    /// Upstream `agent.abort()` (Escape while streaming). The mode aborts
    /// inline now (`AgentSession::signal_abort`) because the executor is
    /// awaiting the running turn; the variant stays for host-side callers.
    Abort,
    /// Upstream `session.cycleModel(direction)`.
    CycleModel { forward: bool },
    /// Upstream the model selector's `selectModel`: `session.setModel(model,
    /// { persist })`. The host resolves the model from the runtime (see the
    /// model selector's divergence note) and reports the result back through
    /// [`UiCommand::ModelSelected`].
    SelectModel {
        provider: String,
        id: String,
        persist: bool,
    },
    /// Upstream `showSessionSelector`'s loaders: list the sessions of one
    /// scope (upstream the async `SessionsLoader`). The result travels back
    /// through [`UiCommand::SessionsLoaded`].
    LoadSessions { scope: SessionScope },
    /// Upstream `deleteSessionFile` (the selector's confirmed delete).
    DeleteSession { path: String },
    /// Upstream the selector's `renameSession` callback: append a
    /// `session_info` entry to the target file.
    RenameSession { path: String, name: String },
    /// Upstream `handleResumeSession`: the pump intercepts this action (the
    /// session switch replaces the whole run loop) and reports it to the
    /// host as a run outcome; the executor only sees it as a no-op.
    ResumeSession { session_path: String },
    /// The editor slot changed (a selector was shown or closed; upstream
    /// `showSelector`'s `editorContainer` swap + `setFocus`).
    EditorSlotChanged,
    /// Upstream `showTreeSelector`'s `onSelect` continuation:
    /// `session.navigateTree(targetId, { summarize, customInstructions })`.
    NavigateTree {
        target_id: String,
        summarize: bool,
        custom_instructions: Option<String>,
    },
    /// Upstream `runtimeHost.fork(entryId, { position })`: the caller rebuilds
    /// the runtime as a branched session (the pump intercepts this and ends
    /// the run loop; `editor_text` restores the forked-from message).
    ForkSession {
        entry_id: String,
        position: String,
        editor_text: Option<String>,
    },
    /// The `/settings` theme submenu previewed a theme setting (upstream
    /// `themeController.preview`).
    ThemePreview(String),
    /// The `/settings` theme was committed (upstream
    /// `themeController.setThemeSetting`, after the settings write).
    ThemeApplied(String),
    /// Upstream `ui.setShowHardwareCursor(enabled)`.
    SetShowHardwareCursor(bool),
    /// Upstream `ui.setClearOnShrink(enabled)`.
    SetClearOnShrink(bool),
    /// Upstream `/reload` → `session.reload()`: rebuild the extension runner
    /// and re-load resources.
    Reload,
}

/// Mode-level options (upstream the settings-derived fields of
/// `InteractiveModeOptions`).
#[derive(Clone, Default)]
pub struct InteractiveModeOptions {
    pub tui_mode: Option<TuiMode>,
    pub clear_on_shrink: Option<bool>,
    pub show_terminal_progress: Option<bool>,
    pub version: Option<String>,
    /// Upstream `terminal.setTitle` through the injected TUI.
    pub on_terminal_title: Option<TerminalTitleCallback>,
    /// Upstream `terminal.setProgress`.
    pub on_terminal_progress: Option<TerminalProgressCallback>,
    pub cwd_git_paths: Option<GitPaths>,
    /// Agent directory: where the 2-column picker keeps its recent-model
    /// history (upstream the extension's `getAgentDir()`).
    pub agent_dir: Option<std::path::PathBuf>,
    /// Live terminal height for the 2-column picker's window (upstream reads
    /// `tui.terminal.rows` on every render; the pump keeps this up to date).
    pub terminal_rows: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

/// The assembled interactive mode (upstream `InteractiveMode`).
pub struct InteractiveMode {
    session: std::sync::Arc<AgentSession>,
    transcript: Shared<InteractiveTranscript>,
    status: Shared<StatusUi>,
    pending: Shared<PendingMessagesUi>,
    editor: Shared<Editor>,
    footer: Shared<FooterComponent>,
    app_title: String,
    show_terminal_progress: AtomicBool,
    on_terminal_title: Option<TerminalTitleCallback>,
    on_terminal_progress: Option<TerminalProgressCallback>,
    /// Upstream `isBashMode` (the `!` prefix toggles the editor border).
    pub bash_mode: AtomicBool,
    /// Host-driven render dirty flag (upstream `ui.requestRender()` inside
    /// the event handlers; the port's host polls this).
    dirty: AtomicBool,
    /// Upstream `lastSigintTime` (milliseconds).
    last_sigint_ms: AtomicU64,
    /// Upstream `lastEscapeTime` (milliseconds).
    last_escape_ms: AtomicU64,
    /// Upstream `bashComponent`: the bash block currently running.
    bash_component: std::sync::Mutex<Option<Shared<BashExecutionComponent>>>,
    /// Upstream `pendingBashComponents`: blocks run while the agent streams,
    /// shown in the pending area until the next submission flushes them into
    /// the chat.
    pending_bash_components: std::sync::Mutex<Vec<Shared<BashExecutionComponent>>>,
    /// Upstream `activeSelectorToken` + the selector in the editor slot.
    active_selector: std::sync::Mutex<Option<ActiveSelector>>,
    /// The waiting `ctx.ui.confirm` reply.
    pending_confirm:
        std::sync::Mutex<Option<std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>>>,
    /// Monotonic token so a stale `done` cannot close a newer selector.
    next_selector_token: AtomicU64,
    /// The 2-column picker's recent-model history (the user's
    /// `pi-model-picker` extension stores it in the agent directory).
    recent_models: std::sync::Mutex<crate::modes::interactive::model_picker_recent::RecentModels>,
    /// Live terminal height (see
    /// [`InteractiveModeOptions::terminal_rows`]).
    terminal_rows: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Where the 2-column picker keeps its recent-model history.
    agent_dir: Option<std::path::PathBuf>,
    /// Host-side autocomplete (upstream the editor's provider; the port's
    /// editor only renders the dropdown).
    autocomplete: std::sync::Mutex<InteractiveAutocomplete>,
    /// The tree-navigation dialog continuation (upstream the awaited
    /// selector/editor chain inside `showTreeSelector`).
    pending_tree_flow: std::sync::Mutex<Option<PendingTreeFlow>>,
    /// The terminal's detected background brightness (upstream
    /// `themeController.getTerminalTheme()`; the pump keeps it in sync).
    terminal_theme: std::sync::Mutex<TerminalTheme>,
    /// The active TUI mode (upstream `this.ui.mode`).
    tui_mode: TuiMode,
}

/// The tree-navigation continuation waiting on a dialog (upstream the
/// `await this.showExtensionSelector(...)` chain inside `showTreeSelector`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingTreeFlow {
    /// Waiting for the "Summarize branch?" answer.
    SummaryChoice { target_id: String },
    /// Waiting for custom summarization instructions.
    CustomInstructions { target_id: String },
}

/// The selector currently shown in place of the editor (upstream the
/// `editorContainer` child plus `activeSelectorToken`).
pub enum ActiveSelector {
    Thinking {
        token: u64,
        component: Shared<ThinkingSelectorComponent>,
    },
    ModelPicker {
        token: u64,
        component: Shared<ModelPickerComponent>,
    },
    ScopedModels {
        token: u64,
        component: Shared<ScopedModelsSelectorComponent>,
    },
    Session {
        token: u64,
        component: Shared<SessionSelectorComponent>,
    },
    Tree {
        token: u64,
        component: Shared<TreeSelectorComponent>,
    },
    ExtensionSelector {
        token: u64,
        component: Shared<ExtensionSelectorComponent>,
    },
    ExtensionInput {
        token: u64,
        component: Shared<ExtensionInputComponent>,
    },
    UserMessage {
        token: u64,
        component: Shared<UserMessageSelectorComponent>,
    },
    Settings {
        token: u64,
        component: Shared<SettingsSelectorComponent>,
    },
}

impl ActiveSelector {
    fn token(&self) -> u64 {
        match self {
            ActiveSelector::Thinking { token, .. } => *token,
            ActiveSelector::ModelPicker { token, .. } => *token,
            ActiveSelector::ScopedModels { token, .. } => *token,
            ActiveSelector::Session { token, .. } => *token,
            ActiveSelector::Tree { token, .. } => *token,
            ActiveSelector::ExtensionSelector { token, .. } => *token,
            ActiveSelector::ExtensionInput { token, .. } => *token,
            ActiveSelector::UserMessage { token, .. } => *token,
            ActiveSelector::Settings { token, .. } => *token,
        }
    }

    /// The mountable component (upstream `created.component`).
    fn mount(&self) -> Box<dyn pillar_tui::tui::Component> {
        match self {
            ActiveSelector::Thinking { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::ModelPicker { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::ScopedModels { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::Session { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::Tree { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::ExtensionSelector { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::ExtensionInput { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::UserMessage { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
            ActiveSelector::Settings { component, .. } => Box::new(
                crate::modes::interactive::transcript::FocusHandle::new(component.clone()),
            ),
        }
    }
}

impl InteractiveMode {
    pub fn new(
        session: std::sync::Arc<AgentSession>,
        transcript_settings: TranscriptSettings,
        markdown_transformers: Vec<crate::core::extensions_types::MarkdownTransformer>,
        options: InteractiveModeOptions,
    ) -> Self {
        let cwd = session
            .session_manager()
            .lock()
            .expect("session")
            .cwd()
            .to_string();
        let mut editor = Editor::new();
        editor.set_theme(get_editor_theme());

        // Upstream reads `session.extensionRunner` while building each
        // component; the port captures the renderers and transformers once,
        // when the transcript is built (extensions are registered before the
        // session exists).
        let extension_runner = session.extension_runner_arc();
        let mut markdown_transformers = markdown_transformers;
        markdown_transformers.extend(
            extension_runner
                .lock()
                .expect("extension runner lock")
                .get_markdown_transformers(),
        );

        let mut transcript =
            InteractiveTranscript::new(transcript_settings, None, markdown_transformers, &cwd);
        {
            let runner = std::sync::Arc::clone(&extension_runner);
            transcript.set_entry_renderer_lookup(Some(Box::new(move |custom_type: &str| {
                // `try_lock`: a `ctx.ui.confirm` dialog blocks the extension's
                // thread while it holds the runner lock, so a render must
                // never wait for it (a contended lookup falls back to the
                // default rendering).
                runner
                    .try_lock()
                    .ok()
                    .and_then(|runner| runner.get_entry_renderer(custom_type))
            })));
        }
        {
            let runner = std::sync::Arc::clone(&extension_runner);
            transcript.set_message_renderer_lookup(Some(Box::new(move |custom_type: &str| {
                runner
                    .try_lock()
                    .ok()
                    .and_then(|runner| runner.get_message_renderer(custom_type))
            })));
        }

        let mut footer = FooterComponent::new(
            std::sync::Arc::clone(&session),
            FooterDataProvider::new(),
            options.cwd_git_paths,
        );
        let auto_compact = session.auto_compaction_enabled();
        footer.set_auto_compact_enabled(auto_compact);

        Self {
            transcript: Shared::new(transcript),
            status: Shared::new(StatusUi::new(
                options.tui_mode.unwrap_or_default(),
                options.clear_on_shrink.unwrap_or(false),
            )),
            pending: Shared::new(PendingMessagesUi::new()),
            editor: Shared::new(editor),
            footer: Shared::new(footer),
            app_title: APP_NAME.to_string(),
            show_terminal_progress: AtomicBool::new(
                options.show_terminal_progress.unwrap_or(false),
            ),
            on_terminal_title: options.on_terminal_title,
            on_terminal_progress: options.on_terminal_progress,
            bash_mode: AtomicBool::new(false),
            dirty: AtomicBool::new(true),
            last_sigint_ms: AtomicU64::new(0),
            last_escape_ms: AtomicU64::new(0),
            bash_component: std::sync::Mutex::new(None),
            pending_bash_components: std::sync::Mutex::new(Vec::new()),
            active_selector: std::sync::Mutex::new(None),
            pending_confirm: std::sync::Mutex::new(None),
            next_selector_token: AtomicU64::new(1),
            recent_models: std::sync::Mutex::new(match options.agent_dir.as_deref() {
                Some(agent_dir) => RecentModels::load(agent_dir),
                None => RecentModels::disabled(),
            }),
            terminal_rows: options
                .terminal_rows
                .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(24))),
            agent_dir: options.agent_dir.clone(),
            autocomplete: std::sync::Mutex::new(InteractiveAutocomplete::new(
                Vec::new(),
                Vec::new(),
                &cwd,
                5,
            )),
            pending_tree_flow: std::sync::Mutex::new(None),
            terminal_theme: std::sync::Mutex::new(TerminalTheme::Dark),
            tui_mode: options.tui_mode.unwrap_or_default(),
            session,
        }
    }

    /// The transcript handle (upstream `chatContainer` etc.).
    pub fn transcript(&self) -> &Shared<InteractiveTranscript> {
        &self.transcript
    }

    /// The status handle (upstream `statusContainer`).
    pub fn status(&self) -> &Shared<StatusUi> {
        &self.status
    }

    /// The pending-messages handle (upstream `pendingMessagesContainer`).
    pub fn pending(&self) -> &Shared<PendingMessagesUi> {
        &self.pending
    }

    /// The editor handle (upstream `editor`).
    pub fn editor(&self) -> &Shared<Editor> {
        &self.editor
    }

    /// The footer handle (upstream `footer`).
    pub fn footer(&self) -> &Shared<FooterComponent> {
        &self.footer
    }

    /// Upstream `updateTerminalTitle`.
    pub fn update_terminal_title(&self) {
        let Some(on_title) = &self.on_terminal_title else {
            return;
        };
        let session_manager = self.session.session_manager().lock().expect("session");
        let cwd_basename = std::path::Path::new(session_manager.cwd())
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| session_manager.cwd().to_string());
        let title = match session_manager.session_name() {
            Some(session_name) => {
                format!("{} - {} - {}", self.app_title, session_name, cwd_basename)
            }
            None => format!("{} - {}", self.app_title, cwd_basename),
        };
        drop(session_manager);
        on_title(&title);
    }

    /// Upstream `renderInitialMessages`: rebuild the transcript from the
    /// compaction-aware entries and note prior compactions. The project
    /// trust warning needs `hasTrustRequiringProjectResources` and lands
    /// with the trust slice.
    pub fn render_initial_messages(&self) {
        let entries = self
            .session
            .session_manager()
            .lock()
            .expect("session")
            .context_entries();
        let mut transcript = self.transcript.lock();
        transcript.render_session_entries(&entries, None);
        let compaction_count = entries
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Compaction(_)))
            .count();
        if compaction_count > 0 {
            let times = if compaction_count == 1 {
                "1 time".to_string()
            } else {
                format!("{compaction_count} times")
            };
            transcript.show_status(&format!("Session compacted {times}"));
        }
    }

    /// The queued messages for the pending display (upstream
    /// `getAllQueuedMessages`'s session half; the compaction queue lives in
    /// the pending UI).
    fn session_queues(&self) -> (Vec<String>, Vec<String>) {
        (
            self.session.get_steering_messages(),
            self.session.get_follow_up_messages(),
        )
    }

    fn set_terminal_progress(&self, running: bool) {
        if self.show_terminal_progress.load(Ordering::SeqCst) {
            if let Some(on_progress) = &self.on_terminal_progress {
                on_progress(running);
            }
        }
    }

    /// Upstream `handleEvent`. Async side effects (prompts, compaction, the
    /// queued-message flush) are reported as [`ModeAction`]s.
    pub fn handle_event(&self, event: &AgentSessionEvent) -> Vec<ModeAction> {
        match event {
            AgentSessionEvent::AgentStart => {
                self.transcript.lock().handle_event(event);
                // Upstream restores the main escape handler when a retry
                // handler is still active; the escape swapping lands with
                // the input loop.
                Vec::new()
            }
            AgentSessionEvent::TurnStart => {
                self.set_terminal_progress(true);
                let mut status = self.status.lock();
                if status.working_visible {
                    if status.active_kind() != Some(StatusIndicatorKind::Working) {
                        status.show_working_status();
                    }
                } else {
                    status.clear_status_indicator(None);
                }
                Vec::new()
            }
            AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } => {
                self.pending.lock().update_display(steering, follow_up);
                Vec::new()
            }
            AgentSessionEvent::SessionInfoChanged { .. } => {
                self.update_terminal_title();
                self.mark_dirty();
                Vec::new()
            }
            AgentSessionEvent::ThinkingLevelChanged { .. } => {
                // footer.invalidate is a no-op upstream; the editor border
                // colour update lands with the editor wiring.
                Vec::new()
            }
            AgentSessionEvent::MessageStart { .. } => {
                // Upstream shows the pending display after a user message.
                if let AgentSessionEvent::MessageStart { message } = event {
                    if matches!(
                        message,
                        pillar_agent::types::AgentMessage::Message(
                            pillar_ai::types::Message::User { .. }
                        )
                    ) {
                        let (steering, follow_up) = self.session_queues();
                        self.pending.lock().update_display(&steering, &follow_up);
                    }
                }
                self.transcript.lock().handle_event(event);
                Vec::new()
            }
            AgentSessionEvent::MessageUpdate { .. } | AgentSessionEvent::MessageEnd { .. } => {
                self.transcript.lock().handle_event(event);
                Vec::new()
            }
            AgentSessionEvent::ToolExecutionStart { .. }
            | AgentSessionEvent::ToolExecutionUpdate { .. }
            | AgentSessionEvent::ToolExecutionEnd { .. } => {
                self.transcript.lock().handle_event(event);
                Vec::new()
            }
            // Upstream: the bash execution callback handles TUI output
            // rendering; the live bash output area is deferred.
            AgentSessionEvent::AgentEnd { .. } => {
                self.set_terminal_progress(false);
                self.status
                    .lock()
                    .clear_status_indicator(Some(StatusIndicatorKind::Working));
                self.transcript.lock().handle_event(event);
                Vec::new()
            }
            AgentSessionEvent::AgentSettled => {
                // Upstream checks the extension-driven shutdown flag; the
                // port's shutdown comes from `/quit`.
                Vec::new()
            }
            AgentSessionEvent::CompactionStart { reason } => {
                self.set_terminal_progress(true);
                let status_reason = match *reason {
                    "manual" => CompactionStatusReason::Manual,
                    "overflow" => CompactionStatusReason::Overflow,
                    _ => CompactionStatusReason::Threshold,
                };
                self.status
                    .lock()
                    .show_status_indicator(compaction_status_indicator(status_reason));
                Vec::new()
            }
            AgentSessionEvent::CompactionEnd {
                reason,
                result,
                aborted,
                will_retry,
                error_message,
            } => {
                self.set_terminal_progress(false);
                let manual = *reason == "manual";
                self.status
                    .lock()
                    .clear_status_indicator(Some(StatusIndicatorKind::Compaction));
                if *aborted {
                    if manual {
                        self.transcript.lock().show_error("Compaction cancelled");
                    } else {
                        self.transcript
                            .lock()
                            .show_status("Auto-compaction cancelled");
                    }
                } else if let Some(result) = result {
                    // Rebuild the transcript from the compaction-aware
                    // entries (the latest compaction is prepended for model
                    // context; it is re-added below at its chronological
                    // position).
                    let entries = self
                        .session
                        .session_manager()
                        .lock()
                        .expect("session")
                        .context_entries();
                    let mut transcript = self.transcript.lock();
                    transcript.chat.clear();
                    let skip = 1.min(entries.len());
                    transcript.render_session_entries(&entries[skip..], None);
                    transcript.add_message_to_chat(
                        CodingAgentMessage::CompactionSummary(create_compaction_summary_message(
                            &result.summary,
                            result.tokens_before,
                            now_ms(),
                        )),
                        false,
                    );
                    if let Some(usage) = &result.usage {
                        transcript
                            .add_compaction_cost_notice(CompactionCostKind::Compaction, usage);
                    }
                } else if let Some(error_message) = error_message {
                    self.transcript.lock().show_error(error_message);
                }
                self.flush_compaction_queue_actions(*will_retry)
            }
            AgentSessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => {
                self.status
                    .lock()
                    .show_retry_indicator(RetryStatusIndicator::new(
                        *attempt as usize,
                        *max_attempts as usize,
                        *delay_ms,
                    ));
                Vec::new()
            }
            AgentSessionEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
                ..
            } => {
                self.status
                    .lock()
                    .clear_status_indicator(Some(StatusIndicatorKind::Retry));
                if !*success {
                    self.transcript.lock().show_error(&format!(
                        "Retry failed after {} attempts: {}",
                        attempt,
                        final_error
                            .clone()
                            .unwrap_or_else(|| "Unknown error".to_string())
                    ));
                }
                Vec::new()
            }
            AgentSessionEvent::EntryAppended { .. } => {
                self.transcript.lock().handle_event(event);
                Vec::new()
            }
            // Upstream's `bash_execution_update` case is a no-op because the
            // `executeBash` chunk callback writes to the component directly;
            // in the port the chunk arrives as an event.
            AgentSessionEvent::BashExecutionUpdate { delta, .. } => {
                self.append_bash_output(delta);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Upstream `flushCompactionQueue`: re-prompt the queued compaction
    /// messages as actions (the failure-restore lives at the host layer,
    /// which re-queues on prompt failure).
    fn flush_compaction_queue_actions(&self, will_retry: bool) -> Vec<ModeAction> {
        let queued = self.pending.lock().take_compaction_queue();
        if queued.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if will_retry {
            for (text, mode) in &queued {
                actions.push(match mode {
                    QueueMode::FollowUp => ModeAction::FollowUp(text.clone()),
                    QueueMode::Steer => ModeAction::Steer(text.clone()),
                });
            }
        } else {
            // The first message becomes the prompt; the rest queue.
            let (first, rest) = queued
                .split_first()
                .expect("non-empty queue checked by caller");
            actions.push(ModeAction::Prompt {
                text: first.0.clone(),
                streaming_behavior: Some(match first.1 {
                    QueueMode::Steer => StreamingBehavior::Steer,
                    QueueMode::FollowUp => StreamingBehavior::FollowUp,
                }),
            });
            for (text, mode) in rest {
                actions.push(match mode {
                    QueueMode::FollowUp => ModeAction::FollowUp(text.clone()),
                    QueueMode::Steer => ModeAction::Steer(text.clone()),
                });
            }
        }
        let (steering, follow_up) = self.session_queues();
        self.pending.lock().update_display(&steering, &follow_up);
        actions
    }

    /// Upstream the `setupEditorSubmitHandler` router. The editor text is
    /// cleared for handled commands; async work is reported as
    /// [`ModeAction`]s.
    pub fn handle_submit(&self, text: &str) -> Vec<ModeAction> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }

        // Selector-backed commands answer a warning until the selectors
        // land (upstream opens the corresponding selector).
        const SELECTOR_COMMANDS: [&str; 11] = [
            "/export",
            "/import",
            "/share",
            "/copy",
            "/session",
            "/changelog",
            "/trust",
            "/login",
            "/logout",
            "/new",
            "/debug",
        ];
        for command in SELECTOR_COMMANDS {
            if text == command || text.starts_with(&format!("{command} ")) {
                self.transcript.lock().show_warning(&format!(
                    "{command} is not available yet (selector UI is not ported)"
                ));
                self.set_editor_text("");
                return Vec::new();
            }
        }

        if text == "/reload" {
            self.set_editor_text("");
            // Upstream `handleReloadCommand`'s guards: reloading mid-run would
            // swap the runner under the streaming turn.
            if self.session.is_streaming() {
                self.transcript
                    .lock()
                    .show_warning("Wait for the current response to finish before reloading.");
                return Vec::new();
            }
            if self.session.is_compacting() {
                self.transcript
                    .lock()
                    .show_warning("Wait for compaction to finish before reloading.");
                return Vec::new();
            }
            return vec![ModeAction::Reload];
        }
        if text == "/resume" {
            self.set_editor_text("");
            return self.show_session_selector();
        }
        if text == "/tree" {
            self.set_editor_text("");
            return self.show_tree_selector(None);
        }
        if text == "/fork" {
            self.set_editor_text("");
            return self.show_user_message_selector(None);
        }
        if text == "/clone" {
            self.set_editor_text("");
            return self.clone_session();
        }
        if text == "/hotkeys" {
            self.set_editor_text("");
            return self.handle_hotkeys_command();
        }
        if text == "/settings" {
            self.set_editor_text("");
            return self.show_settings_selector();
        }
        if text == "/quit" {
            self.set_editor_text("");
            return vec![ModeAction::Shutdown];
        }
        if text == "/arminsayshi" || text == "/dementedelves" {
            // Novelty commands (upstream `handleArminSaysHi` /
            // `handleDementedDelves`) are not ported.
            self.transcript
                .lock()
                .show_warning("This command is not ported");
            self.set_editor_text("");
            return Vec::new();
        }
        if text == "/compact" || text.starts_with("/compact ") {
            let instructions = text
                .strip_prefix("/compact ")
                .map(str::trim)
                .filter(|instructions| !instructions.is_empty())
                .map(str::to_string);
            self.set_editor_text("");
            return vec![ModeAction::Compact { instructions }];
        }
        if text == "/scoped-models" {
            self.set_editor_text("");
            return self.show_scoped_models_selector();
        }
        if text == "/thinking" || text.starts_with("/thinking ") {
            let search = text
                .strip_prefix("/thinking ")
                .map(str::trim)
                .filter(|search| !search.is_empty());
            self.set_editor_text("");
            return match search {
                None => self.show_thinking_selector(),
                Some(search) => {
                    let available = self.session.available_thinking_levels();
                    let normalized = search.to_lowercase();
                    match available
                        .iter()
                        .find(|level| level.to_lowercase() == normalized)
                    {
                        Some(level) => {
                            let level = level.clone();
                            self.select_thinking_level(&level, false);
                            Vec::new()
                        }
                        None => {
                            self.transcript.lock().show_error(&format!(
                                "Unknown thinking level \"{search}\". Available levels: {}.",
                                available.join(", ")
                            ));
                            Vec::new()
                        }
                    }
                }
            };
        }
        if text == "/model" || text.starts_with("/model ") {
            self.set_editor_text("");
            return self.handle_model_command(text);
        }
        // The 2-column picker of the `pi-model-picker` extension (not an
        // upstream command); `/model` above stays as upstream has it.
        if text == "/m" || text.starts_with("/m ") {
            self.set_editor_text("");
            let query = text
                .strip_prefix("/m")
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .map(str::to_string);
            let actions = self.show_model_picker(query.as_deref());
            self.mark_dirty();
            return actions;
        }
        if text == "/name" || text.starts_with("/name ") {
            self.handle_name_command(text);
            self.set_editor_text("");
            return Vec::new();
        }

        // Bash commands (`!` normal, `!!` excluded from context).
        if let Some(rest) = text.strip_prefix('!') {
            let excluded = rest.starts_with('!');
            let command = if excluded {
                rest[1..].trim()
            } else {
                rest.trim()
            };
            if !command.is_empty() {
                if self.session.is_bash_running() {
                    self.transcript.lock().show_warning(
                        "A bash command is already running. Press Esc to cancel it first.",
                    );
                    self.set_editor_text(text);
                    return Vec::new();
                }
                self.editor.lock().add_to_history(text);
                return vec![ModeAction::Bash {
                    command: command.to_string(),
                    excluded,
                }];
            }
        }

        // Queue input during compaction (extension commands would execute
        // immediately; the port has no extension commands yet).
        if self.session.is_compacting() {
            self.editor.lock().add_to_history(text);
            self.set_editor_text("");
            self.pending
                .lock()
                .queue_compaction_message(text.to_string(), QueueMode::Steer);
            self.transcript
                .lock()
                .show_status("Queued message for after compaction");
            let (steering, follow_up) = self.session_queues();
            self.pending.lock().update_display(&steering, &follow_up);
            return Vec::new();
        }

        // Streaming submissions steer the running turn. Upstream awaits
        // `session.prompt(text, { streamingBehavior: "steer" })` from the key
        // handler; the port must queue it synchronously (see
        // `queue_streaming_message`).
        if self.session.is_streaming() {
            self.editor.lock().add_to_history(text);
            self.set_editor_text("");
            if let Err(error) =
                self.session
                    .queue_streaming_message(text, StreamingBehavior::Steer, None)
            {
                self.transcript.lock().show_error(&error);
            }
            let (steering, follow_up) = self.session_queues();
            self.pending.lock().update_display(&steering, &follow_up);
            self.mark_dirty();
            return Vec::new();
        }

        // Normal message submission: move any pending bash blocks into the
        // chat first (upstream `flushPendingBashComponents`).
        self.flush_pending_bash_components();
        self.editor.lock().add_to_history(text);
        // Upstream clears the editor in the generic path too. Without this an
        // extension command (which the mode cannot recognize) stays in the
        // editor, so the next submit appends to it — found by the pty probe
        // for `/tasks-info` followed by `/tasks-init`.
        self.set_editor_text("");
        vec![ModeAction::SubmitToLoop(text.to_string())]
    }

    /// Upstream `handleNameCommand`.
    fn handle_name_command(&self, text: &str) {
        let name = text.replacen("/name", "", 1).trim().to_string();
        if name.is_empty() {
            let mut transcript = self.transcript.lock();
            let session_manager = self.session.session_manager().lock().expect("session");
            match session_manager.session_name() {
                Some(current_name) => {
                    transcript.chat.add_child(Box::new(Spacer::new(1)));
                    transcript.chat.add_child(Box::new(Text::new(
                        &theme().fg("dim", &format!("Session name: {current_name}")),
                        1,
                        0,
                    )));
                }
                None => transcript.show_warning("Usage: /name <name>"),
            }
            return;
        }

        let _ = self.session.set_session_name(&name);
        let session_manager = self.session.session_manager().lock().expect("session");
        let session_name = session_manager.session_name();
        let mut transcript = self.transcript.lock();
        if session_name.as_deref() != Some(name.as_str()) {
            transcript.show_warning(&format!(
                "Session name was normalized from {:?} to {:?}",
                name,
                session_name.clone().unwrap_or_else(|| name.clone())
            ));
        }
        transcript.chat.add_child(Box::new(Spacer::new(1)));
        transcript.chat.add_child(Box::new(Text::new(
            &theme().fg(
                "dim",
                &format!("Session name set: {}", session_name.unwrap_or(name)),
            ),
            1,
            0,
        )));
    }
}

// ============================================================================
// Host-loop surface (upstream `init` / `run` / the key handlers)
// ============================================================================

impl InteractiveMode {
    /// Mark the render tree dirty (upstream `ui.requestRender()` inside the
    /// event handlers; the port's host polls this).
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Consume the dirty flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    /// Mount the mode's components (upstream `mountInteractiveTui` plus the
    /// `init` child list). Returns the editor slot (the editor, or a selector
    /// when one is active) so the host can focus it; the host swaps it with
    /// [`InteractiveMode::replace_editor_slot`] when a selector opens or
    /// closes.
    ///
    /// divergence: the extension widget containers and the header /
    /// loaded-resources containers are not ported yet, so the document half
    /// is the transcript itself.
    pub fn mount(&self, base: &mut TuiBase) -> ComponentId {
        base.add_child(Box::new(self.transcript.clone()));
        base.add_child(Box::new(self.pending.clone()));
        base.add_child(Box::new(self.status.clone()));
        // The editor goes through `FocusHandle`: a plain `Shared` cannot
        // forward `as_focusable`, so the TUI could not focus it (and the
        // hardware-cursor marker would never be emitted).
        let editor = base.add_child(Box::new(
            crate::modes::interactive::transcript::FocusHandle::new(self.editor.clone()),
        ));
        base.add_child(Box::new(self.footer.clone()));
        editor
    }

    /// Advance the host-driven animations (upstream the status indicator's
    /// own interval). Returns whether anything changed.
    pub fn tick(&self) -> bool {
        // Upstream the header status `setTimeout` (auto-hide); the selector's
        // status messages expire the same way.
        let selector_tick = {
            let guard = self.active_selector.lock().expect("active selector");
            match guard.as_ref() {
                Some(ActiveSelector::Session { component, .. }) => {
                    component.lock().tick(Instant::now())
                }
                _ => false,
            }
        };
        let changed = self.status.lock().tick() || selector_tick;
        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Apply one `ctx.ui.*` request (upstream `createExtensionUIContext`'s
    /// methods). The pump calls this: an extension handler only queues the
    /// request, because it runs with the Luau runtime locked and the
    /// transcript's renderers lock that runtime while holding transcript
    /// state.
    pub fn handle_extension_ui(
        &self,
        request: &crate::core::extensions_types::ExtensionUiRequest,
    ) -> Result<(), String> {
        use crate::core::extensions_types::ExtensionUiRequest;
        let ExtensionUiRequest { op, args } = request;
        let text_arg = |name: &str| -> Result<String, String> {
            args.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("ctx.ui.{op}: `{name}` must be a string"))
        };
        let bool_arg = |name: &str| -> Result<bool, String> {
            args.get(name)
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| format!("ctx.ui.{op}: `{name}` must be a boolean"))
        };
        match op.as_str() {
            // Upstream `notify`: info / warning / error chat notices.
            "notify" => {
                let message = text_arg("message")?;
                match args.get("type").and_then(serde_json::Value::as_str) {
                    Some("error") => self.transcript.lock().show_error(&message),
                    Some("warning") => self.transcript.lock().show_warning(&message),
                    _ => self.transcript.lock().show_status(&message),
                }
                self.mark_dirty();
            }
            // Upstream `setExtensionStatus` (the footer's status line).
            "set_status" => {
                let key = text_arg("key")?;
                let text = args
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                self.footer
                    .lock()
                    .footer_data()
                    .set_extension_status(&key, text.as_deref());
                self.mark_dirty();
            }
            // Upstream `setTitle`.
            "set_title" => {
                let title = text_arg("title")?;
                if let Some(callback) = &self.on_terminal_title {
                    callback(&title);
                }
            }
            // Upstream `setWorkingMessage`.
            "set_working_message" => {
                let message = args
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                self.status.lock().set_working_message(message);
                self.mark_dirty();
            }
            // Upstream `setWorkingVisible`.
            "set_working_visible" => {
                let visible = bool_arg("visible")?;
                let streaming = self.session.is_streaming();
                self.status.lock().set_working_visible(visible, streaming);
                self.mark_dirty();
            }
            // Upstream `setWorkingIndicator`.
            "set_working_indicator" => {
                let options = match args.get("options") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(options) => Some(working_indicator_options(options)?),
                };
                self.status.lock().set_working_indicator(options);
                self.mark_dirty();
            }
            // Upstream `setHiddenThinkingLabel`.
            "set_hidden_thinking_label" => {
                let label = args
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(crate::modes::interactive::transcript::DEFAULT_HIDDEN_THINKING_LABEL)
                    .to_string();
                self.transcript.lock().set_hidden_thinking_label(&label);
                self.mark_dirty();
            }
            // Upstream `setEditorText`.
            "set_editor_text" => {
                let text = text_arg("text")?;
                self.set_editor_text(&text);
            }
            // Upstream `pasteToEditor`: the editor's paste handling (large
            // content collapses into a paste marker).
            "paste_to_editor" => {
                let text = text_arg("text")?;
                self.editor.lock().handle_paste(&text);
                self.mark_dirty();
            }
            // Upstream `setToolsExpanded`: the setting plus every rendered
            // tool component.
            "set_tools_expanded" => {
                let expanded = bool_arg("expanded")?;
                let mut transcript = self.transcript.lock();
                transcript.set_tool_output_expanded(expanded);
                transcript.set_all_tools_expanded(expanded);
                drop(transcript);
                self.mark_dirty();
            }
            other => {
                return Err(format!("ctx.ui.{other}: not supported"));
            }
        }
        Ok(())
    }

    /// The editor's current text (upstream `this.editor.getText()`).
    pub fn editor_text(&self) -> String {
        self.editor.lock().get_text()
    }

    /// Whether the editor is empty (upstream `!this.editor.getText().trim()`).
    pub fn editor_is_empty(&self) -> bool {
        self.editor.lock().get_text().trim().is_empty()
    }

    /// Upstream the `onChange` handler: track the `!` bash mode and refresh
    /// the editor border colour when it flips.
    pub fn on_editor_change(&self) {
        let was = self.bash_mode.load(Ordering::SeqCst);
        let is = self.editor.lock().get_text().trim_start().starts_with('!');
        self.bash_mode.store(is, Ordering::SeqCst);
        if was != is {
            self.update_editor_border_color();
        }
        // Upstream the editor re-requests or refreshes the menu on every text
        // change (`insertCharacter` / the deletion paths).
        self.update_autocomplete_on_change();
    }

    /// Upstream `updateEditorBorderColor`.
    pub fn update_editor_border_color(&self) {
        let prefix = if self.bash_mode.load(Ordering::SeqCst) {
            theme().bash_mode_border_color()
        } else {
            theme().thinking_border_color(&self.session.thinking_level())
        };
        let mut editor_theme = get_editor_theme();
        editor_theme.border_color = Box::new(move |text: &str| format!("{prefix}{text}\u{1b}[39m"));
        self.editor.lock().set_theme(editor_theme);
        self.mark_dirty();
    }

    /// The merged queue view (upstream `getAllQueuedMessages`).
    fn update_pending_display(&self) {
        let (steering, follow_up) = self.session_queues();
        self.pending.lock().update_display(&steering, &follow_up);
        self.mark_dirty();
    }

    /// Upstream `clearAllQueues`: drain the session and compaction queues.
    fn clear_all_queues(&self) -> (Vec<String>, Vec<String>) {
        let (mut steering, mut follow_up) = self.session.clear_queue();
        for (text, mode) in self.pending.lock().take_compaction_queue() {
            match mode {
                QueueMode::Steer => steering.push(text),
                QueueMode::FollowUp => follow_up.push(text),
            }
        }
        (steering, follow_up)
    }

    /// Upstream `restoreQueuedMessagesToEditor`: put every queued message back
    /// into the editor, above the current text. Returns how many were restored.
    pub fn restore_queued_messages_to_editor(&self) -> usize {
        let (steering, follow_up) = self.clear_all_queues();
        let all_queued: Vec<String> = steering.into_iter().chain(follow_up).collect();
        if all_queued.is_empty() {
            self.update_pending_display();
            return 0;
        }
        let queued_text = all_queued.join("\n\n");
        let current_text = self.editor.lock().get_text();
        let combined = [queued_text, current_text]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.set_editor_text(&combined);
        self.update_pending_display();
        all_queued.len()
    }

    /// Upstream `handleCtrlC`: first press clears, a second within 500 ms
    /// shuts down.
    pub fn handle_ctrl_c(&self) -> Vec<ModeAction> {
        let now = now_ms();
        let last = self.last_sigint_ms.load(Ordering::SeqCst);
        self.set_editor_text("");
        self.mark_dirty();
        if now.saturating_sub(last) < 500 {
            return vec![ModeAction::Shutdown];
        }
        self.last_sigint_ms.store(now, Ordering::SeqCst);
        Vec::new()
    }

    /// Upstream `handleCtrlD` (only called with an empty editor).
    pub fn handle_ctrl_d(&self) -> Vec<ModeAction> {
        vec![ModeAction::Shutdown]
    }

    /// Upstream the `onEscape` handler: abort a streaming turn (restoring the
    /// queued messages), cancel bash, leave bash mode, or arm the
    /// double-escape action.
    pub fn handle_escape(&self) -> Vec<ModeAction> {
        if self.session.is_streaming() {
            // Upstream calls `agent.abort()` right here; routing it through
            // the action executor would only deliver it after the running
            // turn ended (the executor is awaiting that turn), so Escape
            // could never interrupt a response.
            self.restore_queued_messages_to_editor();
            self.session.signal_abort();
            self.mark_dirty();
            return Vec::new();
        }
        if self.session.is_bash_running() {
            self.session.abort_bash();
            self.transcript.lock().show_status("Bash command cancelled");
            return Vec::new();
        }
        // Upstream the tree selector's summary path swaps the escape handler
        // to abort the branch summarization.
        if self.session.is_branch_summarizing() {
            self.session.abort_branch_summary();
            return Vec::new();
        }
        if self.bash_mode.load(Ordering::SeqCst) {
            self.set_editor_text("");
            self.bash_mode.store(false, Ordering::SeqCst);
            self.update_editor_border_color();
            return Vec::new();
        }
        if self.editor_is_empty() {
            let action = self
                .session
                .settings_manager()
                .lock()
                .expect("settings lock")
                .double_escape_action();
            if action != DoubleEscapeAction::None {
                let now = now_ms();
                let last = self.last_escape_ms.load(Ordering::SeqCst);
                if now.saturating_sub(last) < 500 {
                    self.last_escape_ms.store(0, Ordering::SeqCst);
                    match action {
                        DoubleEscapeAction::Tree => return self.show_tree_selector(None),
                        DoubleEscapeAction::Fork => {
                            return self.show_user_message_selector(None);
                        }
                        DoubleEscapeAction::None => {}
                    }
                } else {
                    self.last_escape_ms.store(now, Ordering::SeqCst);
                }
            }
        }
        Vec::new()
    }

    /// Upstream `handleHotkeysCommand`: append a bordered Markdown table of
    /// the resolved keybindings to the transcript.
    ///
    /// divergence: upstream also lists extension-registered shortcuts
    /// (`extensionRunner.getShortcuts`); the port needs the resolved
    /// keybinding map for the conflict check, which the mode does not hold yet.
    pub fn handle_hotkeys_command(&self) -> Vec<ModeAction> {
        let key = |keybinding: &str| {
            crate::modes::interactive::components::keybinding_hints::key_display_text(keybinding)
        };
        let windows_newline_hint = if cfg!(windows) {
            " (Ctrl+Enter on Windows Terminal)"
        } else {
            ""
        };
        let hotkeys = format!(
            "**Navigation**\n\
             | Key | Action |\n\
             |-----|--------|\n\
             | `{up}` / `{down}` / `{left}` / `{right}` | Move cursor / browse history |\n\
             | `{word_left}` / `{word_right}` | Move by word |\n\
             | `{line_start}` | Start of line |\n\
             | `{line_end}` | End of line |\n\
             | `{jump_forward}` | Jump forward to character |\n\
             | `{jump_backward}` | Jump backward to character |\n\
             | `{page_up}` / `{page_down}` | Scroll by page |\n\n\
             **Editing**\n\
             | Key | Action |\n\
             |-----|--------|\n\
             | `{submit}` | Send message |\n\
             | `{new_line}` | New line{windows_newline_hint} |\n\
             | `{delete_word_backward}` | Delete word backwards |\n\
             | `{delete_word_forward}` | Delete word forwards |\n\
             | `{delete_to_line_start}` | Delete to start of line |\n\
             | `{delete_to_line_end}` | Delete to end of line |\n\
             | `{yank}` | Paste the most-recently-deleted text |\n\
             | `{yank_pop}` | Cycle through the deleted text after pasting |\n\
             | `{undo}` | Undo |\n\n\
             **Other**\n\
             | Key | Action |\n\
             |-----|--------|\n\
             | `{tab}` | Path completion / accept autocomplete |\n\
             | `{interrupt}` | Cancel autocomplete / abort streaming |\n\
             | `{clear}` | Clear editor (first) / exit (second) |\n\
             | `{exit}` | Exit (when editor is empty) |\n\
             | `{suspend}` | Suspend to background |\n\
             | `{cycle_thinking_level}` | Cycle thinking level |\n\
             | `{cycle_model_forward}` / `{cycle_model_backward}` | Cycle models |\n\
             | `{select_model}` | Open model selector |\n\
             | `{expand_tools}` | Toggle tool output expansion |\n\
             | `{toggle_thinking}` | Toggle thinking block visibility |\n\
             | `{external_editor}` | Edit message in external editor |\n\
             | `{copy_message}` | Copy last assistant message |\n\
             | `{follow_up}` | Queue follow-up message |\n\
             | `{dequeue}` | Restore queued messages |\n\
             | `{paste_image}` | Paste image or text from clipboard |\n\
             | `/` | Slash commands |\n\
             | `!` | Run bash command |\n\
             | `!!` | Run bash command (excluded from context) |\n",
            up = key("tui.editor.cursorUp"),
            down = key("tui.editor.cursorDown"),
            left = key("tui.editor.cursorLeft"),
            right = key("tui.editor.cursorRight"),
            word_left = key("tui.editor.cursorWordLeft"),
            word_right = key("tui.editor.cursorWordRight"),
            line_start = key("tui.editor.cursorLineStart"),
            line_end = key("tui.editor.cursorLineEnd"),
            jump_forward = key("tui.editor.jumpForward"),
            jump_backward = key("tui.editor.jumpBackward"),
            page_up = key("tui.editor.pageUp"),
            page_down = key("tui.editor.pageDown"),
            submit = key("tui.input.submit"),
            new_line = key("tui.input.newLine"),
            delete_word_backward = key("tui.editor.deleteWordBackward"),
            delete_word_forward = key("tui.editor.deleteWordForward"),
            delete_to_line_start = key("tui.editor.deleteToLineStart"),
            delete_to_line_end = key("tui.editor.deleteToLineEnd"),
            yank = key("tui.editor.yank"),
            yank_pop = key("tui.editor.yankPop"),
            undo = key("tui.editor.undo"),
            tab = key("tui.input.tab"),
            interrupt = key("app.interrupt"),
            clear = key("app.clear"),
            exit = key("app.exit"),
            suspend = key("app.suspend"),
            cycle_thinking_level = key("app.thinking.cycle"),
            cycle_model_forward = key("app.model.cycleForward"),
            cycle_model_backward = key("app.model.cycleBackward"),
            select_model = key("app.model.select"),
            expand_tools = key("app.tools.expand"),
            toggle_thinking = key("app.thinking.toggle"),
            external_editor = key("app.editor.external"),
            copy_message = key("app.message.copy"),
            follow_up = key("app.message.followUp"),
            dequeue = key("app.message.dequeue"),
            paste_image = key("app.clipboard.pasteImage"),
        );
        self.transcript
            .lock()
            .add_markdown_panel("Keyboard Shortcuts", &hotkeys);
        self.mark_dirty();
        Vec::new()
    }

    /// Upstream `toggleToolOutputExpansion` / `setToolsExpanded`.
    pub fn toggle_tool_output_expansion(&self) {
        let expanded = !self.transcript.lock().tool_output_expanded();
        self.transcript.lock().set_all_tools_expanded(expanded);
        self.transcript.lock().show_status(&format!(
            "Tool output: {}",
            if expanded { "expanded" } else { "collapsed" }
        ));
        self.mark_dirty();
    }

    /// Upstream `toggleThinkingBlockVisibility` (the settings write is not
    /// ported; the toggle stays in-memory).
    pub fn toggle_thinking_block_visibility(&self) {
        let hide = !self.transcript.lock().hide_thinking_block();
        self.transcript.lock().set_all_hide_thinking_block(hide);
        self.transcript.lock().show_status(&format!(
            "Thinking blocks: {}",
            if hide { "hidden" } else { "visible" }
        ));
        self.mark_dirty();
    }

    /// Upstream `cycleThinkingLevel`.
    pub fn cycle_thinking_level(&self) {
        match self.session.cycle_thinking_level() {
            None => self
                .transcript
                .lock()
                .show_status("Current model does not support thinking"),
            Some(level) => {
                // upstream `this.footer.invalidate()` is a no-op
                self.update_editor_border_color();
                self.transcript
                    .lock()
                    .show_status(&format!("Thinking level: {level}"));
            }
        }
        self.mark_dirty();
    }

    /// Upstream `handleBashCommand`'s component setup: create the block (in
    /// the pending area while the agent streams, in the chat otherwise) and
    /// remember it so streamed chunks and the completion land on it.
    pub fn begin_bash(&self, command: &str, exclude_from_context: bool) {
        self.begin_bash_deferred(command, exclude_from_context, self.session.is_streaming());
    }

    /// [`begin_bash`] with the deferral decision supplied (upstream reads
    /// `session.isStreaming` at the `handleBashCommand` call site).
    pub fn begin_bash_deferred(&self, command: &str, exclude_from_context: bool, deferred: bool) {
        let component = Shared::new(BashExecutionComponent::new(
            &theme(),
            command,
            exclude_from_context,
        ));
        if deferred {
            self.pending
                .lock()
                .container
                .add_child(Box::new(component.clone()));
            self.pending_bash_components
                .lock()
                .expect("pending bash")
                .push(component.clone());
        } else {
            self.transcript
                .lock()
                .chat
                .add_child(Box::new(component.clone()));
        }
        *self.bash_component.lock().expect("bash component") = Some(component);
        self.mark_dirty();
    }

    /// Upstream the `executeBash` chunk callback: append streamed output.
    pub fn append_bash_output(&self, chunk: &str) {
        let component = self.bash_component.lock().expect("bash component").clone();
        let Some(component) = component else {
            return;
        };
        component.lock().append_output(chunk);
        self.mark_dirty();
    }

    /// Upstream `setComplete(...)` after `executeBash` returns (or in its
    /// error path).
    pub fn complete_bash(
        &self,
        exit_code: Option<i32>,
        cancelled: bool,
        truncation: Option<TruncationResult>,
        full_output_path: Option<String>,
    ) {
        let component = self.bash_component.lock().expect("bash component").take();
        if let Some(component) = component {
            component
                .lock()
                .set_complete(exit_code, cancelled, truncation, full_output_path);
        }
        // Upstream resets the `!` mode once the command finished
        // (`isBashMode = false; updateEditorBorderColor()`).
        self.bash_mode.store(false, Ordering::SeqCst);
        self.update_editor_border_color();
        self.mark_dirty();
    }

    /// Upstream `flushPendingBashComponents`: move the blocks that ran while
    /// streaming into the chat.
    ///
    /// divergence: the port removes the child from the pending container
    /// instead of relying on the next `updatePendingMessagesDisplay` clear
    /// (upstream leaves it there until then, which renders it twice).
    pub fn flush_pending_bash_components(&self) {
        let pending = {
            let mut guard = self.pending_bash_components.lock().expect("pending bash");
            std::mem::take(&mut *guard)
        };
        if pending.is_empty() {
            return;
        }
        let mut transcript = self.transcript.lock();
        let mut pending_ui = self.pending.lock();
        for component in pending {
            let taken = crate::modes::interactive::transcript::take_shared_child(
                &mut pending_ui.container,
                &component,
            );
            match taken {
                Some(child) => transcript.chat.add_child(child),
                None => transcript.chat.add_child(Box::new(component)),
            }
        }
        drop(pending_ui);
        drop(transcript);
        self.mark_dirty();
    }

    /// The component that belongs in the editor slot (upstream the
    /// `editorContainer` child): the active selector, else the editor.
    pub fn editor_slot_component(&self) -> Box<dyn pillar_tui::tui::Component> {
        let selector = self.active_selector.lock().expect("active selector");
        match selector.as_ref() {
            Some(selector) => selector.mount(),
            None => Box::new(crate::modes::interactive::transcript::FocusHandle::new(
                self.editor.clone(),
            )),
        }
    }

    /// Whether a selector currently occupies the editor slot.
    pub fn has_active_selector(&self) -> bool {
        self.active_selector
            .lock()
            .expect("active selector")
            .is_some()
    }

    /// Upstream `showSelector`: put `selector` in the editor slot and tell the
    /// host to re-mount it.
    fn show_selector(&self, selector: ActiveSelector) -> Vec<ModeAction> {
        // Upstream `disposeActiveSelector()` before installing the new one.
        *self.active_selector.lock().expect("active selector") = Some(selector);
        self.mark_dirty();
        vec![ModeAction::EditorSlotChanged]
    }

    /// Upstream the `done` callback: close the active selector (guarded by the
    /// token so a stale close cannot dismiss a newer selector) and restore the
    /// editor.
    fn close_selector(&self, token: Option<u64>) -> Vec<ModeAction> {
        let mut guard = self.active_selector.lock().expect("active selector");
        match (guard.as_ref(), token) {
            (Some(selector), Some(token)) if selector.token() != token => return Vec::new(),
            (None, _) => return Vec::new(),
            _ => {}
        }
        *guard = None;
        drop(guard);
        self.mark_dirty();
        vec![ModeAction::EditorSlotChanged]
    }

    /// Upstream the thinking selector factory: build it, remember it, and put
    /// it in the editor slot.
    pub fn show_thinking_selector(&self) -> Vec<ModeAction> {
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let current = self.session.thinking_level();
        let default_level = self
            .session
            .settings_manager()
            .lock()
            .expect("settings lock")
            .default_thinking_level();
        let component = Shared::new(ThinkingSelectorComponent::new(
            &current,
            &self.session.available_thinking_levels(),
            default_level.as_deref(),
        ));
        self.show_selector(ActiveSelector::Thinking { token, component })
    }

    /// Upstream `showModelsSelector` (`/scoped-models`): the searchable list
    /// that enables/disables and orders the models Ctrl+P cycles through.
    /// The enabled set is session-only until Ctrl+S persists it.
    ///
    /// divergence: upstream refreshes the model catalogs while the selector
    /// is open (`refreshStatus: "Refreshing model catalogs…"`, 15 s timeout,
    /// `updateModels` / `setRefreshStatus` afterwards). The port has no
    /// catalog refresh yet (the same as the `/model` selector), so the
    /// selector opens on the current runtime snapshot only.
    pub fn show_scoped_models_selector(&self) -> Vec<ModeAction> {
        let available_models = self.session.model_runtime().get_available_snapshot();
        let configured_patterns = self
            .session
            .settings_manager()
            .lock()
            .expect("settings lock")
            .enabled_models();
        let current_enabled_ids = self.initial_enabled_ids(&available_models, configured_patterns);
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(ScopedModelsSelectorComponent::new(
            available_models,
            current_enabled_ids,
        ));
        self.show_selector(ActiveSelector::ScopedModels { token, component })
    }

    /// Upstream the `currentEnabledIds` computation of `showModelsSelector`:
    /// the session scope wins, else the settings patterns resolve against the
    /// available models (unmatched patterns stay listed as unavailable).
    fn initial_enabled_ids(
        &self,
        available_models: &[pillar_ai::types::Model],
        configured_patterns: Option<Vec<String>>,
    ) -> Option<Vec<String>> {
        let session_scoped = self.session.scoped_models();
        if !session_scoped.is_empty() {
            return Some(
                session_scoped
                    .iter()
                    .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
                    .collect(),
            );
        }
        let patterns = configured_patterns?;
        if patterns.is_empty() {
            return None;
        }
        let resolved = crate::core::model_resolver::resolve_model_scope_from_models(
            &patterns,
            available_models,
        );
        let mut ids = resolved
            .scoped_models
            .iter()
            .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
            .collect::<Vec<_>>();
        for diagnostic in resolved.diagnostics {
            if diagnostic.code == crate::core::model_resolver::ModelScopeDiagnosticCode::NoMatch
                && !ids.contains(&diagnostic.pattern)
            {
                ids.push(diagnostic.pattern);
            }
        }
        Some(ids)
    }

    /// Upstream the `updateSessionModels` closure (the selector's `onChange`):
    /// translate the enabled set into the session's cycle scope. The scope is
    /// set only while an explicit list enables at least one available model
    /// but not all of them (upstream the same condition).
    ///
    /// divergence: upstream applies it synchronously from the component
    /// callback; the port applies it in the same synchronous dispatch (the
    /// same precedent as [`Self::select_thinking_level`]).
    pub fn apply_scoped_model_change(&self, enabled_ids: &Option<Vec<String>>) {
        let available_models = self.session.model_runtime().get_available_snapshot();
        let available_ids: std::collections::HashSet<String> = available_models
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect();
        let new_scope = match enabled_ids {
            Some(ids) => {
                let has_enabled_available = ids.iter().any(|id| available_ids.contains(id));
                let all_available_enabled = available_ids.iter().all(|id| ids.contains(id));
                if has_enabled_available && !all_available_enabled {
                    crate::core::model_resolver::resolve_model_scope_from_models(
                        ids,
                        &available_models,
                    )
                    .scoped_models
                    .into_iter()
                    .map(crate::core::model_mutation::ScopedModel::from)
                    .collect()
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        };
        self.session.set_scoped_models(new_scope);
        self.update_available_provider_count();
        self.mark_dirty();
    }

    /// Upstream the selector's `onPersist` callback (Ctrl+S): write the
    /// enabled patterns to settings, or clear them when everything is
    /// enabled, and report the save in the transcript.
    ///
    /// divergence: the settings write runs synchronously in the pump
    /// dispatch (the settings manager is behind a mutex; the same precedent
    /// as `selectThinkingLevel` persisting through the session).
    pub fn save_scoped_models(&self, enabled_ids: &Option<Vec<String>>) {
        let available_models = self.session.model_runtime().get_available_snapshot();
        let available_ids: std::collections::HashSet<String> = available_models
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect();
        let all_enabled = enabled_ids.as_ref().is_some_and(|ids| {
            ids.len() == available_models.len() && ids.iter().all(|id| available_ids.contains(id))
        });
        let patterns = match enabled_ids {
            Some(ids) if !all_enabled => Some(ids.clone()),
            _ => None,
        };
        self.session
            .settings_manager()
            .lock()
            .expect("settings lock")
            .set_enabled_models(patterns);
        self.transcript
            .lock()
            .show_status("Model selection saved to settings");
        self.mark_dirty();
    }

    /// Upstream `showSessionSelector` (`/resume` and the
    /// `app.session.resume` binding): the searchable resume list. The listing
    /// itself is async host work — the selector starts empty and the host
    /// fills it through [`ModeAction::LoadSessions`] /
    /// [`UiCommand::SessionsLoaded`] (upstream the async loaders with a
    /// progress callback).
    pub fn show_session_selector(&self) -> Vec<ModeAction> {
        let current_session_path = self
            .session
            .session_manager()
            .lock()
            .expect("session lock")
            .session_file()
            .map(|path| path.to_string_lossy().to_string());
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(SessionSelectorComponent::new(current_session_path));
        let mut actions = self.show_selector(ActiveSelector::Session { token, component });
        // Upstream the constructor's `loadCurrentSessions()`.
        actions.push(ModeAction::LoadSessions {
            scope: SessionScope::Current,
        });
        actions
    }

    /// A load's intermediate progress (upstream the `onProgress` callback
    /// feeding the header's `Loading n/m`).
    pub fn session_load_progress(&self, scope: SessionScope, loaded: usize, total: usize) {
        let guard = self.active_selector.lock().expect("active selector");
        let Some(ActiveSelector::Session { component, .. }) = guard.as_ref() else {
            return;
        };
        if component.lock().scope() != scope {
            return;
        }
        component.lock().set_progress(loaded, total);
        drop(guard);
        self.mark_dirty();
    }

    /// A load settled (upstream `loadScope`'s resolution): fill the list,
    /// clear the loading state, and repaint.
    pub fn session_list_loaded(&self, scope: SessionScope, sessions: Vec<SessionInfo>) {
        let guard = self.active_selector.lock().expect("active selector");
        let Some(ActiveSelector::Session { component, .. }) = guard.as_ref() else {
            return;
        };
        if component.lock().scope() != scope {
            return;
        }
        component.lock().finish_load(scope, sessions);
        drop(guard);
        self.mark_dirty();
    }

    /// Upstream the `onDeleteSession` continuation: drop the deleted file
    /// from the caches, report the outcome, and schedule the reload.
    pub fn complete_session_delete(
        &self,
        path: &str,
        ok: bool,
        moved_to_trash: bool,
        error: Option<String>,
    ) -> Vec<ModeAction> {
        let scope = {
            let guard = self.active_selector.lock().expect("active selector");
            let Some(ActiveSelector::Session { component, .. }) = guard.as_ref() else {
                return Vec::new();
            };
            let mut component = component.lock();
            if ok {
                component.remove_session(path);
                let message = if moved_to_trash {
                    "Session moved to trash"
                } else {
                    "Session deleted"
                };
                component.set_status(StatusKind::Info, message, Some(2000));
            } else {
                let message = format!(
                    "Failed to delete: {}",
                    error.as_deref().unwrap_or("Unknown error")
                );
                component.set_status(StatusKind::Error, &message, Some(3000));
            }
            component.scope()
        };
        self.mark_dirty();
        // Upstream `refreshSessionsAfterMutation`.
        vec![ModeAction::LoadSessions { scope }]
    }

    /// Upstream `confirmRename`'s continuation: report failures and refresh
    /// the list (upstream `refreshSessionsAfterMutation`).
    pub fn complete_session_rename(&self, error: Option<String>) -> Vec<ModeAction> {
        let scope = {
            let guard = self.active_selector.lock().expect("active selector");
            let Some(ActiveSelector::Session { component, .. }) = guard.as_ref() else {
                return Vec::new();
            };
            if let Some(error) = error {
                component
                    .lock()
                    .set_status(StatusKind::Error, &error, Some(4000));
            }
            component.lock().scope()
        };
        self.mark_dirty();
        vec![ModeAction::LoadSessions { scope }]
    }

    /// Upstream `showTreeSelector`: open the session-tree navigator (the
    /// `/tree` command, `app.session.tree`, and double-Escape with the
    /// `tree` setting). An empty session reports a status instead.
    pub fn show_tree_selector(&self, initial_selected_id: Option<&str>) -> Vec<ModeAction> {
        let tree = self.session.get_tree();
        if tree.is_empty() {
            self.transcript.lock().show_status("No entries in session");
            self.mark_dirty();
            return Vec::new();
        }
        let current_leaf_id = self.session.get_leaf_id();
        let filter_mode = self
            .session
            .settings_manager()
            .lock()
            .expect("settings lock")
            .tree_filter_mode();
        let terminal_rows = self.terminal_rows.load(Ordering::SeqCst);
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(TreeSelectorComponent::new(
            &tree,
            current_leaf_id,
            terminal_rows,
            initial_selected_id,
            filter_mode,
        ));
        self.show_selector(ActiveSelector::Tree { token, component })
    }

    /// Upstream `showExtensionSelector`: a generic option dialog in the
    /// editor slot (the port uses it for the "Summarize branch?" prompt).
    pub fn show_extension_selector(&self, title: &str, options: &[String]) -> Vec<ModeAction> {
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(ExtensionSelectorComponent::new(title, options));
        self.show_selector(ActiveSelector::ExtensionSelector { token, component })
    }

    /// Show an extension's `ctx.ui.confirm(title, message)` dialog (upstream
    /// `showExtensionConfirm`): Yes/No in the editor slot, answered through
    /// `reply` when the user picks (Escape answers "no", like upstream's
    /// cancelled confirm).
    pub fn show_extension_confirm(
        &self,
        title: &str,
        message: &str,
        reply: std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>,
    ) -> Vec<ModeAction> {
        *self.pending_confirm.lock().expect("pending confirm") = Some(reply);
        let title = if message.trim().is_empty() {
            title.to_string()
        } else {
            format!("{title}\n\n{message}")
        };
        self.show_extension_selector(&title, &["Yes".to_string(), "No".to_string()])
    }

    /// Route one `ctx.ui` dialog request (the pump's `ExtensionUiAsk`).
    pub fn begin_extension_ask(
        &self,
        request: crate::core::extensions_types::ExtensionUiRequest,
        reply: std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>,
    ) {
        let title = request
            .args
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Extension");
        let message = request
            .args
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match request.op.as_str() {
            "confirm" => {
                self.show_extension_confirm(title, message, reply);
            }
            other => {
                let _ = reply.send(Err(format!("ctx.ui.{other}: not supported")));
            }
        }
    }

    /// Answer a pending `ctx.ui.confirm` (upstream the selector callbacks).
    fn answer_pending_confirm(&self, yes: bool) -> bool {
        let Some(reply) = self.pending_confirm.lock().expect("pending confirm").take() else {
            return false;
        };
        let _ = reply.send(Ok(serde_json::Value::Bool(yes)));
        self.mark_dirty();
        true
    }

    /// Upstream `showExtensionInput`: a single-line text dialog in the editor
    /// slot (the port uses it for custom branch-summary instructions).
    pub fn show_extension_input(&self, title: &str) -> Vec<ModeAction> {
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(ExtensionInputComponent::new(title, None));
        self.show_selector(ActiveSelector::ExtensionInput { token, component })
    }

    /// Upstream the tree selector's `onSelect` handler: the selected entry
    /// either reports "already here", navigates immediately
    /// (`branchSummary.skipPrompt`), or asks about summarization first.
    fn tree_selection_committed(&self, token: u64, entry_id: &str) -> Vec<ModeAction> {
        if self.session.get_leaf_id().as_deref() == Some(entry_id) {
            let actions = self.close_selector(Some(token));
            self.transcript.lock().show_status("Already at this point");
            return actions;
        }

        let mut actions = self.close_selector(Some(token));
        let skip_prompt = self
            .session
            .settings_manager()
            .lock()
            .expect("settings lock")
            .branch_summary_settings()
            .skip_prompt;
        if skip_prompt {
            actions.push(ModeAction::NavigateTree {
                target_id: entry_id.to_string(),
                summarize: false,
                custom_instructions: None,
            });
            return actions;
        }

        *self.pending_tree_flow.lock().expect("tree flow") = Some(PendingTreeFlow::SummaryChoice {
            target_id: entry_id.to_string(),
        });
        actions.extend(self.show_extension_selector(
            "Summarize branch?",
            &[
                "No summary".to_string(),
                "Summarize".to_string(),
                "Summarize with custom prompt".to_string(),
            ],
        ));
        actions
    }

    /// Upstream the summary-choice continuation.
    fn complete_tree_summary_choice(&self, token: u64, option: &str) -> Vec<ModeAction> {
        let pending = self.pending_tree_flow.lock().expect("tree flow").take();
        let Some(PendingTreeFlow::SummaryChoice { target_id }) = pending else {
            return self.close_selector(Some(token));
        };
        let mut actions = self.close_selector(Some(token));
        match option {
            "No summary" => actions.push(ModeAction::NavigateTree {
                target_id,
                summarize: false,
                custom_instructions: None,
            }),
            "Summarize" => actions.push(ModeAction::NavigateTree {
                target_id,
                summarize: true,
                custom_instructions: None,
            }),
            "Summarize with custom prompt" => {
                *self.pending_tree_flow.lock().expect("tree flow") =
                    Some(PendingTreeFlow::CustomInstructions { target_id });
                actions.extend(self.show_extension_input("Custom summarization instructions"));
            }
            _ => {}
        }
        actions
    }

    /// Upstream escaping the summary choice: re-open the tree at the same
    /// entry.
    fn cancel_tree_summary_choice(&self, token: u64) -> Vec<ModeAction> {
        let pending = self.pending_tree_flow.lock().expect("tree flow").take();
        let mut actions = self.close_selector(Some(token));
        if let Some(PendingTreeFlow::SummaryChoice { target_id }) = pending {
            actions.extend(self.show_tree_selector(Some(&target_id)));
        }
        actions
    }

    /// Upstream the custom-instructions continuation.
    fn complete_tree_custom_instructions(&self, token: u64, value: &str) -> Vec<ModeAction> {
        let pending = self.pending_tree_flow.lock().expect("tree flow").take();
        let mut actions = self.close_selector(Some(token));
        if let Some(PendingTreeFlow::CustomInstructions { target_id }) = pending {
            actions.push(ModeAction::NavigateTree {
                target_id,
                summarize: true,
                custom_instructions: Some(value.to_string()),
            });
        }
        actions
    }

    /// Upstream cancelling the custom-instructions editor: loop back to the
    /// summary choice.
    fn cancel_tree_custom_instructions(&self, token: u64) -> Vec<ModeAction> {
        let pending = self.pending_tree_flow.lock().expect("tree flow").take();
        match pending {
            Some(PendingTreeFlow::CustomInstructions { target_id }) => {
                let mut actions = self.close_selector(Some(token));
                *self.pending_tree_flow.lock().expect("tree flow") =
                    Some(PendingTreeFlow::SummaryChoice { target_id });
                actions.extend(self.show_extension_selector(
                    "Summarize branch?",
                    &[
                        "No summary".to_string(),
                        "Summarize".to_string(),
                        "Summarize with custom prompt".to_string(),
                    ],
                ));
                actions
            }
            _ => self.close_selector(Some(token)),
        }
    }

    /// Upstream the tree's `onCopy`: write the selected entry's text to the
    /// clipboard (OSC 52 as the fallback).
    fn copy_tree_selection(&self, text: Option<String>) -> Vec<ModeAction> {
        let Some(text) = text else {
            self.transcript
                .lock()
                .show_error("Selected entry has no text to copy");
            self.mark_dirty();
            return Vec::new();
        };
        match crate::utils::clipboard::copy_to_clipboard(&text, &mut std::io::stdout()) {
            Ok(()) => {
                self.transcript
                    .lock()
                    .show_status("Copied selected message to clipboard");
            }
            Err(error) => {
                self.transcript.lock().show_error(&error);
            }
        }
        self.mark_dirty();
        Vec::new()
    }

    /// Upstream the tree's `onLabelChange`: append the label change entry.
    fn apply_tree_label(&self, entry_id: &str, label: Option<String>) -> Vec<ModeAction> {
        let result = self
            .session
            .session_manager()
            .lock()
            .expect("session lock")
            .append_label_change(entry_id, label.as_deref());
        if let Err(error) = result {
            self.transcript.lock().show_error(&error);
        }
        self.mark_dirty();
        Vec::new()
    }

    /// Upstream the block before `session.navigateTree(...)`: stop a
    /// streaming response, restore its queued messages, and show the branch
    /// summary spinner.
    pub fn begin_tree_navigation(&self, summarize: bool) -> Vec<ModeAction> {
        let actions = Vec::new();
        if self.session.is_streaming() {
            self.restore_queued_messages_to_editor();
            self.session.signal_abort();
        }
        if summarize {
            self.status
                .lock()
                .show_status_indicator(branch_summary_status_indicator());
        }
        self.mark_dirty();
        actions
    }

    /// Upstream the code after `await session.navigateTree(...)`: report the
    /// outcome, rebuild the transcript on success, and flush the compaction
    /// queue.
    pub fn complete_tree_navigation(
        &self,
        target_id: &str,
        editor_text: Option<String>,
        cancelled: bool,
        aborted: bool,
        error: Option<String>,
    ) -> Vec<ModeAction> {
        self.status
            .lock()
            .clear_status_indicator(Some(StatusIndicatorKind::BranchSummary));
        if let Some(error) = error {
            self.transcript.lock().show_error(&error);
            self.mark_dirty();
            return Vec::new();
        }
        if aborted {
            self.transcript
                .lock()
                .show_status("Branch summarization cancelled");
            // Upstream re-opens the tree with the same selection.
            return self.show_tree_selector(Some(target_id));
        }
        if cancelled {
            self.transcript.lock().show_status("Navigation cancelled");
            self.mark_dirty();
            return Vec::new();
        }

        // Upstream `chatContainer.clear(); renderInitialMessages();`.
        self.transcript.lock().clear_conversation();
        self.render_initial_messages();
        if let Some(editor_text) = editor_text {
            if self.editor().lock().get_text().trim().is_empty() {
                self.set_editor_text(&editor_text);
            }
        }
        self.transcript
            .lock()
            .show_status("Navigated to selected point");
        self.mark_dirty();
        self.flush_compaction_queue_actions(false)
    }

    /// Upstream `showUserMessageSelector` (`/fork` and `app.session.fork`):
    /// pick a user message to branch from. The initial selection is the most
    /// recent message.
    pub fn show_user_message_selector(&self, initial_selected_id: Option<&str>) -> Vec<ModeAction> {
        let user_messages = self.session.user_messages_for_forking();
        if user_messages.is_empty() {
            self.transcript
                .lock()
                .show_status("No messages to fork from");
            self.mark_dirty();
            return Vec::new();
        }
        let initial = initial_selected_id
            .map(str::to_string)
            .or_else(|| user_messages.last().map(|(entry_id, _)| entry_id.clone()));
        let items: Vec<UserMessageItem> = user_messages
            .into_iter()
            .map(|(entry_id, text)| UserMessageItem { id: entry_id, text })
            .collect();
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(UserMessageSelectorComponent::new(items, initial.as_deref()));
        self.show_selector(ActiveSelector::UserMessage { token, component })
    }

    /// Upstream `showUserMessageSelector`'s `onSelect`: fork before the
    /// selected user message, restoring its text to the editor.
    fn fork_selected_user_message(&self, token: u64, entry_id: &str) -> Vec<ModeAction> {
        let editor_text = self
            .session
            .user_messages_for_forking()
            .into_iter()
            .find(|(id, _)| id == entry_id)
            .map(|(_, text)| text);
        let mut actions = self.close_selector(Some(token));
        actions.push(ModeAction::ForkSession {
            entry_id: entry_id.to_string(),
            position: "before".to_string(),
            editor_text,
        });
        actions
    }

    /// Upstream `handleCloneCommand`: fork at the current leaf (no selector).
    pub fn clone_session(&self) -> Vec<ModeAction> {
        let Some(leaf_id) = self.session.get_leaf_id() else {
            self.transcript.lock().show_status("Nothing to clone yet");
            self.mark_dirty();
            return Vec::new();
        };
        vec![ModeAction::ForkSession {
            entry_id: leaf_id,
            position: "at".to_string(),
            editor_text: None,
        }]
    }

    /// Upstream `showSettingsSelector` (`/settings`): the full settings panel.
    /// The config snapshot is read from the session, the settings manager and
    /// the theme registry; the changes come back through
    /// [`Self::apply_setting_change`].
    pub fn show_settings_selector(&self) -> Vec<ModeAction> {
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(SettingsSelectorComponent::new(self.settings_config()));
        self.show_selector(ActiveSelector::Settings { token, component })
    }

    /// The `/settings` panel's config snapshot (upstream the `SettingsConfig`
    /// literal in `showSettingsSelector`).
    fn settings_config(&self) -> SettingsConfig {
        // Collect the session-derived values first: several of these helpers
        // lock the settings manager, which must not be held here (the port's
        // settings mutex is not reentrant).
        let auto_compact = self.session.auto_compaction_enabled();
        let current_model = self.session.current_model();
        let available_default_models = self.session.model_runtime().get_available_snapshot();
        let terminal_theme = *self.terminal_theme.lock().expect("terminal theme");
        let active_theme_name = current_theme_name();
        let available_themes = crate::modes::interactive::theme::get_available_themes();
        let supports_images = pillar_tui::terminal_image::get_capabilities()
            .images
            .is_some();
        let tui_mode = match self.tui_mode {
            TuiMode::Fullscreen => "fullscreen",
            TuiMode::Regular => "regular",
        }
        .to_string();

        let settings = self.session.settings_manager();
        let settings = settings.lock().expect("settings lock");
        let current_theme = settings
            .theme_setting()
            .or(active_theme_name)
            .unwrap_or_else(|| "dark".to_string());
        SettingsConfig {
            auto_compact,
            default_model: match settings.default_model_and_provider() {
                Some((provider, id)) => format!("{provider}/{id}"),
                None => "not set".to_string(),
            },
            current_model,
            available_default_models,
            show_images: settings.show_images(),
            image_width_cells: settings.image_width_cells(),
            auto_resize_images: settings.image_auto_resize(),
            block_images: settings.block_images(),
            enable_skill_commands: settings.enable_skill_commands(),
            steering_mode: settings.steering_mode().to_string(),
            follow_up_mode: settings.follow_up_mode().to_string(),
            transport: settings.transport().to_string(),
            http_idle_timeout_ms: settings
                .http_idle_timeout_ms()
                .unwrap_or(crate::core::http_dispatcher::DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            thinking_level: settings
                .default_thinking_level()
                .unwrap_or_else(|| "medium".to_string()),
            model_thinking_levels: settings.all_model_thinking_levels(),
            current_theme,
            terminal_theme,
            available_themes,
            hide_thinking_block: settings.hide_thinking_block(),
            mermaid_rendering_mode: settings.mermaid_rendering_mode().to_string(),
            show_cache_miss_notices: settings.show_cache_miss_notices(),
            collapse_changelog: settings.collapse_changelog(),
            enable_install_telemetry: settings.enable_install_telemetry(),
            double_escape_action: match settings.double_escape_action() {
                DoubleEscapeAction::Fork => "fork",
                DoubleEscapeAction::Tree => "tree",
                DoubleEscapeAction::None => "none",
            }
            .to_string(),
            tree_filter_mode: match settings.tree_filter_mode() {
                crate::core::settings_manager::TreeFilterMode::Default => "default",
                crate::core::settings_manager::TreeFilterMode::NoTools => "no-tools",
                crate::core::settings_manager::TreeFilterMode::UserOnly => "user-only",
                crate::core::settings_manager::TreeFilterMode::LabeledOnly => "labeled-only",
                crate::core::settings_manager::TreeFilterMode::All => "all",
            }
            .to_string(),
            show_hardware_cursor: settings.show_hardware_cursor(),
            editor_padding_x: settings.editor_padding_x(),
            output_pad: settings.output_pad(),
            autocomplete_max_visible: settings.autocomplete_max_visible(),
            quiet_startup: settings.quiet_startup(),
            default_project_trust: match settings.default_project_trust() {
                crate::core::settings_manager::DefaultProjectTrust::Ask => "ask",
                crate::core::settings_manager::DefaultProjectTrust::Always => "always",
                crate::core::settings_manager::DefaultProjectTrust::Never => "never",
            }
            .to_string(),
            clear_on_shrink: settings.clear_on_shrink(),
            show_terminal_progress: settings.show_terminal_progress(),
            tui_mode,
            fullscreen_exit_output: settings.fullscreen_exit_output().to_string(),
            fullscreen_scrollbar: settings.fullscreen_scrollbar().to_string(),
            fullscreen_copy_on_select: settings.fullscreen_copy_on_select(),
            warnings: settings.warnings(),
            supports_images,
        }
    }

    /// The terminal background the theme controller last detected (the pump
    /// keeps it in sync).
    pub fn set_terminal_theme(&self, theme: TerminalTheme) {
        *self.terminal_theme.lock().expect("terminal theme") = theme;
    }

    /// Upstream the `showSettingsSelector` callbacks: apply one settings
    /// change to the settings manager and the UI.
    ///
    /// divergences: the transport / HTTP-timeout runtime hooks
    /// (`agent.transport`, `configureHttpDispatcher`) are not ported yet, and
    /// switching TUI mode reports a status instead of re-layouting
    /// (fullscreen is not ported).
    pub fn apply_setting_change(&self, id: &str, value: &str) -> Vec<ModeAction> {
        let mut actions: Vec<ModeAction> = Vec::new();
        // These write through the session, which locks the settings manager
        // itself (holding the lock here would deadlock).
        match id {
            "autocompact" => {
                let enabled = value == "true";
                self.session.set_auto_compaction_enabled(enabled);
                self.footer.lock().set_auto_compact_enabled(enabled);
                self.mark_dirty();
                return actions;
            }
            "steering-mode" => {
                self.session.set_steering_mode(queue_mode(value));
                self.mark_dirty();
                return actions;
            }
            "follow-up-mode" => {
                self.session.set_follow_up_mode(queue_mode(value));
                self.mark_dirty();
                return actions;
            }
            _ => {}
        }
        {
            let mut settings = self
                .session
                .settings_manager()
                .lock()
                .expect("settings lock");
            match id {
                "show-images" => settings.set_show_images(value == "true"),
                "image-width-cells" => settings.set_image_width_cells(value.parse().unwrap_or(60)),
                "auto-resize-images" => settings.set_image_auto_resize(value == "true"),
                "block-images" => settings.set_block_images(value == "true"),
                "skill-commands" => settings.set_enable_skill_commands(value == "true"),
                "transport" => settings.set_transport(value),
                "http-idle-timeout" => {
                    let timeout = value.parse().unwrap_or(0);
                    settings.set_http_idle_timeout_ms(timeout);
                    drop(settings);
                    self.transcript.lock().show_status(&format!(
                        "HTTP idle timeout: {}",
                        crate::core::http_dispatcher::format_http_idle_timeout_ms(timeout)
                    ));
                    self.mark_dirty();
                    return actions;
                }
                "hide-thinking" => settings.set_hide_thinking_block(value == "true"),
                "mermaid-rendering" => settings.set_mermaid_rendering_mode(value),
                "cache-miss-notices" => settings.set_show_cache_miss_notices(value == "true"),
                "collapse-changelog" => settings.set_collapse_changelog(value == "true"),
                "quiet-startup" => settings.set_quiet_startup(value == "true"),
                "install-telemetry" => settings.set_enable_install_telemetry(value == "true"),
                "default-project-trust" => {
                    settings.set_default_project_trust(match value {
                        "always" => crate::core::settings_manager::DefaultProjectTrust::Always,
                        "never" => crate::core::settings_manager::DefaultProjectTrust::Never,
                        _ => crate::core::settings_manager::DefaultProjectTrust::Ask,
                    });
                }
                "double-escape-action" => {
                    settings.set_double_escape_action(match value {
                        "fork" => DoubleEscapeAction::Fork,
                        "none" => DoubleEscapeAction::None,
                        _ => DoubleEscapeAction::Tree,
                    });
                }
                "tree-filter-mode" => {
                    settings.set_tree_filter_mode(match value {
                        "no-tools" => crate::core::settings_manager::TreeFilterMode::NoTools,
                        "user-only" => crate::core::settings_manager::TreeFilterMode::UserOnly,
                        "labeled-only" => {
                            crate::core::settings_manager::TreeFilterMode::LabeledOnly
                        }
                        "all" => crate::core::settings_manager::TreeFilterMode::All,
                        _ => crate::core::settings_manager::TreeFilterMode::Default,
                    });
                }
                "show-hardware-cursor" => {
                    settings.set_show_hardware_cursor(value == "true");
                    actions.push(ModeAction::SetShowHardwareCursor(value == "true"));
                }
                "editor-padding" => {
                    let padding = value.parse().unwrap_or(0);
                    settings.set_editor_padding_x(padding);
                    drop(settings);
                    self.editor().lock().set_padding_x(padding);
                    self.mark_dirty();
                    return actions;
                }
                "output-padding" => {
                    let padding: u8 = value.parse().unwrap_or(1);
                    settings.set_output_pad(padding);
                    {
                        let mut transcript = self.transcript.lock();
                        transcript.settings_mut().output_pad = padding as usize;
                    }
                    drop(settings);
                    self.render_after_rebuild();
                    return actions;
                }
                "autocomplete-max-visible" => {
                    let max_visible = value.parse().unwrap_or(5);
                    settings.set_autocomplete_max_visible(max_visible);
                    drop(settings);
                    self.editor()
                        .lock()
                        .set_autocomplete_max_visible(max_visible as usize);
                    self.autocomplete
                        .lock()
                        .expect("autocomplete")
                        .set_max_visible(max_visible as usize);
                    self.mark_dirty();
                    return actions;
                }
                "clear-on-shrink" => {
                    settings.set_clear_on_shrink(value == "true");
                    actions.push(ModeAction::SetClearOnShrink(value == "true"));
                }
                "terminal-progress" => {
                    settings.set_show_terminal_progress(value == "true");
                    self.show_terminal_progress
                        .store(value == "true", Ordering::SeqCst);
                }
                "tui-mode" => {
                    // divergence: switching to fullscreen is not ported; the
                    // row is reverted with a status, like upstream's failure
                    // path for open overlays.
                    let current = match self.tui_mode {
                        TuiMode::Fullscreen => "fullscreen",
                        TuiMode::Regular => "regular",
                    };
                    if value != current {
                        drop(settings);
                        self.refresh_setting_value("tui-mode", current);
                        self.transcript
                            .lock()
                            .show_status("TUI mode switching is not ported yet");
                        self.mark_dirty();
                        return actions;
                    }
                }
                "fullscreen-exit-output" => settings.set_fullscreen_exit_output(value),
                "fullscreen-scrollbar" => settings.set_fullscreen_scrollbar(value),
                "fullscreen-copy-on-select" => {
                    settings.set_fullscreen_copy_on_select(value == "true");
                }
                "warnings" => {
                    if let Ok(warnings) = serde_json::from_str(value) {
                        settings.set_warnings(warnings);
                    }
                }
                "theme" => {
                    settings.set_theme(value);
                    actions.push(ModeAction::ThemeApplied(value.to_string()));
                }
                _ => {}
            }
        }

        // The effects that rebuild or re-render transcript content.
        match id {
            "show-images" | "image-width-cells" => {
                let (show, width) = {
                    let settings = self
                        .session
                        .settings_manager()
                        .lock()
                        .expect("settings lock");
                    (
                        settings.show_images(),
                        settings.image_width_cells() as usize,
                    )
                };
                self.transcript.lock().set_tool_image_settings(show, width);
            }
            "skill-commands" => self.rebuild_autocomplete(),
            "hide-thinking" => {
                let hide = value == "true";
                self.transcript.lock().set_all_hide_thinking_block(hide);
            }
            "mermaid-rendering" | "cache-miss-notices" => self.render_after_rebuild(),
            _ => {}
        }
        self.mark_dirty();
        actions
    }

    /// Upstream `onModelThinkingLevelChange` / `onModelThinkingLevelRemove`.
    pub fn apply_model_thinking_level(&self, provider: &str, model_id: &str, level: Option<&str>) {
        {
            let mut settings = self
                .session
                .settings_manager()
                .lock()
                .expect("settings lock");
            match level {
                Some(level) => settings.set_model_thinking_level(provider, model_id, level),
                None => settings.remove_model_thinking_level(provider, model_id),
            }
        }
        // Apply to the running session when the override is for the current
        // model (a removal reverts to the global default).
        let is_current = self
            .session
            .current_model()
            .is_some_and(|model| model.provider == provider && model.id == model_id);
        if is_current {
            let effective = match level {
                Some(level) => level.to_string(),
                None => self
                    .session
                    .settings_manager()
                    .lock()
                    .expect("settings lock")
                    .default_thinking_level()
                    .unwrap_or_else(|| "medium".to_string()),
            };
            self.select_thinking_level(&effective, false);
        }
    }

    /// Rebuild the transcript from the session entries (upstream
    /// `rebuildChatFromMessages`).
    fn render_after_rebuild(&self) {
        self.transcript.lock().clear_conversation();
        self.render_initial_messages();
        self.mark_dirty();
    }

    /// Update one row's value in the open settings selector (upstream
    /// `selector?.getSettingsList().updateValue(...)`).
    fn refresh_setting_value(&self, id: &str, value: &str) {
        let guard = self.active_selector.lock().expect("active selector");
        if let Some(ActiveSelector::Settings { component, .. }) = guard.as_ref() {
            component.lock().refresh_value(id, value);
        }
    }

    /// Upstream `handleModelCommand`, minus the built-in selector: an exact
    /// model reference switches directly and anything else falls through to the
    /// 2-column picker (`/m`).
    ///
    /// divergence: the user replaced upstream's `ModelSelectorComponent` with
    /// the `pi-model-picker` UX, so a non-matching argument opens `/m` filtered
    /// by that argument (the picker filters by provider id / display name, not
    /// by model name). Upstream also refreshes the catalogs before giving up on
    /// the exact match (`findExactModelMatch`); the port has no catalog refresh
    /// yet, so it matches the cached scope / runtime snapshot only. The Anthropic
    /// subscription warning and the `daxnuts` easter egg are not ported (see
    /// docs/TASKS.md).
    pub fn handle_model_command(&self, text: &str) -> Vec<ModeAction> {
        let search = text
            .strip_prefix("/model")
            .map(str::trim)
            .filter(|search| !search.is_empty());
        let Some(search) = search else {
            return self.show_model_picker(None);
        };
        let scoped = self.session.scoped_models();
        let cached_models: Vec<pillar_ai::types::Model> = if scoped.is_empty() {
            self.session.model_runtime().get_available_snapshot()
        } else {
            scoped.into_iter().map(|scoped| scoped.model).collect()
        };
        match crate::core::model_resolver::find_exact_model_reference_match(search, &cached_models)
        {
            Some(model) => vec![ModeAction::SelectModel {
                provider: model.provider,
                id: model.id,
                persist: false,
            }],
            None => self.show_model_picker(Some(search)),
        }
    }

    /// Upstream the model selector's `selectModel` continuation: close the
    /// selector (`done()`), refresh the provider count / footer / editor
    /// border, and report the switch (or the error) in the transcript.
    pub fn complete_model_selection(
        &self,
        provider: &str,
        id: &str,
        persist: bool,
        error: Option<String>,
    ) -> Vec<ModeAction> {
        // Upstream `done()` is a no-op once the token no longer matches (the
        // selector was cancelled or replaced while the switch was in flight).
        let token = {
            let guard = self.active_selector.lock().expect("active selector");
            match guard.as_ref() {
                Some(ActiveSelector::ModelPicker { token, .. }) => Some(*token),
                _ => None,
            }
        };
        let actions = match token {
            Some(token) => self.close_selector(Some(token)),
            None => Vec::new(),
        };
        self.update_available_provider_count();
        self.update_editor_border_color();
        match error {
            Some(error) => self.transcript.lock().show_error(&error),
            None => {
                self.record_recent_model(provider, id);
                self.transcript.lock().show_status(&if persist {
                    format!("Default model: {provider}/{id}")
                } else {
                    format!("Model: {id}")
                });
            }
        }
        self.mark_dirty();
        actions
    }

    /// Report a model change the mode did not initiate (upstream Ctrl+P
    /// cycling): refresh the footer and record the recent-model history, like
    /// the extension's `model_select` listener.
    pub fn complete_model_cycle(&self, provider: &str, id: &str) {
        self.record_recent_model(provider, id);
        self.update_available_provider_count();
        self.update_editor_border_color();
        self.mark_dirty();
    }

    /// Upstream the extension's `recordRecent`: every model change counts
    /// toward the picker's history (session restores are never reported to the
    /// mode, so they do not).
    pub fn record_recent_model(&self, provider: &str, id: &str) {
        self.recent_models
            .lock()
            .expect("recent models")
            .record(provider, id);
    }

    /// Reload the recent-model history from disk (upstream `session_start` and
    /// each `/m` open).
    pub fn reload_recent_models(&self, agent_dir: &std::path::Path) {
        *self.recent_models.lock().expect("recent models") = RecentModels::load(agent_dir);
    }

    /// The terminal height the 2-column picker lays out against (the pump
    /// refreshes it on resize).
    pub fn set_terminal_rows(&self, rows: usize) {
        self.terminal_rows
            .store(rows, std::sync::atomic::Ordering::SeqCst);
    }

    /// The model groups the 2-column picker shows (upstream `getGroups`): the
    /// session scope when set, else every available model, grouped per
    /// provider and sorted like the built-in selector.
    fn picker_groups(&self) -> Vec<(String, String, Vec<pillar_ai::types::Model>)> {
        let scoped = self.session.scoped_models();
        let models: Vec<pillar_ai::types::Model> = if scoped.is_empty() {
            self.session.model_runtime().get_available_snapshot()
        } else {
            scoped.into_iter().map(|scoped| scoped.model).collect()
        };
        let runtime = self.session.model_runtime();
        let mut groups: Vec<(String, String, Vec<pillar_ai::types::Model>)> = Vec::new();
        for model in models {
            let group = match groups.iter_mut().find(|(id, _, _)| *id == model.provider) {
                Some(group) => group,
                None => {
                    let display_name = runtime
                        .get_provider(&model.provider)
                        .map(|provider| provider.name.clone())
                        .unwrap_or_else(|| model.provider.clone());
                    groups.push((model.provider.clone(), display_name, Vec::new()));
                    groups.last_mut().expect("just pushed")
                }
            };
            group.2.push(model);
        }
        for group in &mut groups {
            group.2.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        groups.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        groups
    }

    /// Upstream `/m`'s category list: `RECENT` (filtered to available models)
    /// when there is history, then one category per provider.
    fn picker_categories(&self, query: Option<&str>) -> Result<Vec<PickerCategory>, String> {
        let groups = self.picker_groups();
        if groups.is_empty() {
            return Err("No models available".to_string());
        }
        let groups: Vec<_> = match query {
            Some(query) if !query.is_empty() => {
                let needle = query.to_lowercase();
                groups
                    .into_iter()
                    .filter(|(provider, display_name, _)| {
                        provider.to_lowercase().contains(&needle)
                            || display_name.to_lowercase().contains(&needle)
                    })
                    .collect()
            }
            _ => groups,
        };
        if groups.is_empty() {
            return Err(format!(
                "No provider matching \"{}\"",
                query.unwrap_or_default()
            ));
        }

        let mut categories: Vec<PickerCategory> = Vec::new();
        let recents = self.recent_models.lock().expect("recent models");
        let recent_models: Vec<pillar_ai::types::Model> = recents
            .entries()
            .iter()
            .filter_map(|entry| {
                groups.iter().find_map(|(provider, _, models)| {
                    (provider == &entry.provider)
                        .then(|| models.iter().find(|model| model.id == entry.id).cloned())
                        .flatten()
                })
            })
            .collect();
        drop(recents);
        if !recent_models.is_empty() {
            categories.push(PickerCategory {
                kind: CategoryKind::Recent,
                id: RECENT_CATEGORY_ID.to_string(),
                label: "RECENT".to_string(),
                models: recent_models,
            });
        }
        for (provider, display_name, models) in groups {
            categories.push(PickerCategory {
                kind: CategoryKind::Provider,
                id: provider,
                label: display_name,
                models,
            });
        }
        Ok(categories)
    }

    /// Upstream the extension's `/m` handler: open the 2-column picker, with
    /// an optional provider filter for the left column.
    pub fn show_model_picker(&self, query: Option<&str>) -> Vec<ModeAction> {
        // Upstream reloads the history when the picker opens (the file may have
        // been changed by a pi install sharing the agent directory).
        if let Some(agent_dir) = &self.agent_dir {
            self.reload_recent_models(agent_dir);
        }
        let categories = match self.picker_categories(query) {
            Ok(categories) => categories,
            Err(error) => {
                self.transcript.lock().show_error(&error);
                self.mark_dirty();
                return Vec::new();
            }
        };
        let token = self.next_selector_token.fetch_add(1, Ordering::SeqCst);
        let component = Shared::new(ModelPickerComponent::new(
            categories,
            self.session.current_model(),
            std::sync::Arc::clone(&self.terminal_rows),
        ));
        self.show_selector(ActiveSelector::ModelPicker { token, component })
    }

    /// Upstream `updateAvailableProviderCount`: the footer shows the provider
    /// name only when the active scope spans more than one provider.
    pub fn update_available_provider_count(&self) {
        let scoped = self.session.scoped_models();
        let models: Vec<pillar_ai::types::Model> = if scoped.is_empty() {
            self.session.model_runtime().get_available_snapshot()
        } else {
            scoped.into_iter().map(|scoped| scoped.model).collect()
        };
        let providers: std::collections::HashSet<&str> =
            models.iter().map(|model| model.provider.as_str()).collect();
        self.footer
            .lock()
            .footer_data()
            .set_available_provider_count(providers.len());
        self.mark_dirty();
    }

    /// Upstream `selectThinkingLevel`: apply (and optionally persist) a level.
    pub fn select_thinking_level(&self, level: &str, persist: bool) {
        self.session.set_thinking_level(level, persist);
        self.update_editor_border_color();
        self.transcript.lock().show_status(&format!(
            "{} thinking level: {level}",
            if persist { "Default" } else { "" }
        ));
        self.mark_dirty();
    }

    /// Route a key to the active selector (upstream the selector being the
    /// focused component).
    ///
    /// Returns `Some(actions)` when the key was handled by the selector,
    /// `None` when no selector is active.
    pub fn handle_selector_key(&self, data: &str) -> Option<Vec<ModeAction>> {
        enum Handle {
            Thinking(u64, Shared<ThinkingSelectorComponent>),
            ModelPicker(u64, Shared<ModelPickerComponent>),
            ScopedModels(u64, Shared<ScopedModelsSelectorComponent>),
            Session(u64, Shared<SessionSelectorComponent>),
            Tree(u64, Shared<TreeSelectorComponent>),
            ExtensionSelector(u64, Shared<ExtensionSelectorComponent>),
            ExtensionInput(u64, Shared<ExtensionInputComponent>),
            UserMessage(u64, Shared<UserMessageSelectorComponent>),
            Settings(u64, Shared<SettingsSelectorComponent>),
        }
        let handle = {
            let guard = self.active_selector.lock().expect("active selector");
            match guard.as_ref() {
                Some(ActiveSelector::Thinking { token, component }) => {
                    Handle::Thinking(*token, component.clone())
                }
                Some(ActiveSelector::ModelPicker { token, component }) => {
                    Handle::ModelPicker(*token, component.clone())
                }
                Some(ActiveSelector::ScopedModels { token, component }) => {
                    Handle::ScopedModels(*token, component.clone())
                }
                Some(ActiveSelector::Session { token, component }) => {
                    Handle::Session(*token, component.clone())
                }
                Some(ActiveSelector::Tree { token, component }) => {
                    Handle::Tree(*token, component.clone())
                }
                Some(ActiveSelector::ExtensionSelector { token, component }) => {
                    Handle::ExtensionSelector(*token, component.clone())
                }
                Some(ActiveSelector::ExtensionInput { token, component }) => {
                    Handle::ExtensionInput(*token, component.clone())
                }
                Some(ActiveSelector::UserMessage { token, component }) => {
                    Handle::UserMessage(*token, component.clone())
                }
                Some(ActiveSelector::Settings { token, component }) => {
                    Handle::Settings(*token, component.clone())
                }
                None => return None,
            }
        };
        Some(match handle {
            Handle::Thinking(token, component) => match component.lock().handle_key(data) {
                ThinkingSelectorOutcome::Consumed => Vec::new(),
                ThinkingSelectorOutcome::Select(level) => {
                    self.select_thinking_level(&level, false);

                    self.close_selector(Some(token))
                }
                ThinkingSelectorOutcome::SelectAsDefault(level) => {
                    self.select_thinking_level(&level, true);

                    self.close_selector(Some(token))
                }
                ThinkingSelectorOutcome::Cancel => self.close_selector(Some(token)),
            },
            // The picker stays open until the async `session.setModel`
            // settles; the host reports back through `UiCommand::ModelSelected`
            // and [`Self::complete_model_selection`] closes it (upstream
            // `done()`).
            Handle::ModelPicker(token, component) => match component.lock().handle_key(data) {
                ModelPickerOutcome::Consumed => Vec::new(),
                ModelPickerOutcome::Select(model) => vec![ModeAction::SelectModel {
                    provider: model.provider.clone(),
                    id: model.id.clone(),
                    persist: false,
                }],
                ModelPickerOutcome::SelectAsDefault(model) => vec![ModeAction::SelectModel {
                    provider: model.provider.clone(),
                    id: model.id.clone(),
                    persist: true,
                }],
                ModelPickerOutcome::Cancel => self.close_selector(Some(token)),
            },
            // Toggles apply immediately (upstream `onChange` →
            // `updateSessionModels`); Ctrl+S persists to settings and the
            // component clears its unsaved marker (upstream inside the save
            // branch).
            Handle::ScopedModels(token, component) => match component.lock().handle_key(data) {
                ScopedModelsOutcome::Consumed => Vec::new(),
                ScopedModelsOutcome::Change(enabled_ids) => {
                    self.apply_scoped_model_change(&enabled_ids);
                    Vec::new()
                }
                ScopedModelsOutcome::Persist(enabled_ids) => {
                    self.save_scoped_models(&enabled_ids);
                    Vec::new()
                }
                ScopedModelsOutcome::Cancel => self.close_selector(Some(token)),
            },
            // Toggles apply synchronously; loads / mutations travel through
            // the executor and report back through the `UiCommand`s (upstream
            // the async loaders and `onDeleteSession` / `renameSession`).
            Handle::Session(token, component) => match component.lock().handle_key(data) {
                SessionSelectorOutcome::Consumed => Vec::new(),
                SessionSelectorOutcome::Resume(path) => {
                    let mut actions = self.close_selector(Some(token));
                    actions.push(ModeAction::ResumeSession { session_path: path });
                    actions
                }
                SessionSelectorOutcome::ScopeToggled { scope, needs_load } => {
                    if needs_load {
                        vec![ModeAction::LoadSessions { scope }]
                    } else {
                        Vec::new()
                    }
                }
                SessionSelectorOutcome::Delete(path) => vec![ModeAction::DeleteSession { path }],
                SessionSelectorOutcome::Rename { path, name } => {
                    vec![ModeAction::RenameSession { path, name }]
                }
                SessionSelectorOutcome::Cancel => self.close_selector(Some(token)),
            },
            // The session tree: navigation keys are internal; Select / Copy /
            // LabelChanged / Cancel are host work (upstream the `onSelect` /
            // `onCopy` / `onLabelChange` / `onCancel` callbacks).
            Handle::Tree(token, component) => match component.lock().handle_key(data) {
                TreeSelectorOutcome::Consumed => Vec::new(),
                TreeSelectorOutcome::Select(entry_id) => {
                    self.tree_selection_committed(token, &entry_id)
                }
                TreeSelectorOutcome::Copy(text) => self.copy_tree_selection(text),
                TreeSelectorOutcome::LabelChanged { entry_id, label } => {
                    self.apply_tree_label(&entry_id, label)
                }
                TreeSelectorOutcome::Cancel => self.close_selector(Some(token)),
            },
            // The "Summarize branch?" / custom-instructions dialogs.
            Handle::ExtensionSelector(token, component) => {
                match component.lock().handle_key(data) {
                    ExtensionSelectorOutcome::Consumed => Vec::new(),
                    ExtensionSelectorOutcome::Select(option) => {
                        if self.answer_pending_confirm(option == "Yes") {
                            return Some(self.close_selector(Some(token)));
                        }
                        self.complete_tree_summary_choice(token, &option)
                    }
                    ExtensionSelectorOutcome::ToggleToolsExpanded => {
                        self.toggle_tool_output_expansion();
                        Vec::new()
                    }
                    ExtensionSelectorOutcome::Cancel => {
                        if self.answer_pending_confirm(false) {
                            return Some(self.close_selector(Some(token)));
                        }
                        self.cancel_tree_summary_choice(token)
                    }
                }
            }
            Handle::ExtensionInput(token, component) => match component.lock().handle_key(data) {
                ExtensionInputOutcome::Consumed => Vec::new(),
                ExtensionInputOutcome::Submit(value) => {
                    self.complete_tree_custom_instructions(token, &value)
                }
                ExtensionInputOutcome::Cancel => self.cancel_tree_custom_instructions(token),
            },
            // `/fork`: the pump intercepts the commit and rebuilds the run
            // loop as a branched session (upstream `runtimeHost.fork`).
            Handle::UserMessage(token, component) => match component.lock().handle_key(data) {
                UserMessageSelectorOutcome::Consumed => Vec::new(),
                UserMessageSelectorOutcome::Select(entry_id) => {
                    self.fork_selected_user_message(token, &entry_id)
                }
                UserMessageSelectorOutcome::Cancel => self.close_selector(Some(token)),
            },
            // `/settings`: the panel stays open while changes apply (upstream
            // the callbacks mutate the live settings).
            Handle::Settings(token, component) => match component.lock().handle_key(data) {
                SettingsSelectorOutcome::Consumed => Vec::new(),
                SettingsSelectorOutcome::Change { id, value } => {
                    self.apply_setting_change(&id, &value)
                }
                SettingsSelectorOutcome::ThemePreview(setting) => {
                    vec![ModeAction::ThemePreview(setting)]
                }
                SettingsSelectorOutcome::ModelThinkingLevelChange {
                    provider,
                    model_id,
                    level,
                } => {
                    self.apply_model_thinking_level(&provider, &model_id, Some(&level));
                    Vec::new()
                }
                SettingsSelectorOutcome::ModelThinkingLevelRemove { provider, model_id } => {
                    self.apply_model_thinking_level(&provider, &model_id, None);
                    Vec::new()
                }
                SettingsSelectorOutcome::Close => self.close_selector(Some(token)),
            },
        })
    }

    // --- Autocomplete (host-side provider) ---------------------------------

    /// Upstream `setupAutocompleteProvider`: (re)build the command table from
    /// the session and the current settings, and hand the dropdown to the
    /// editor.
    pub fn rebuild_autocomplete(&self) {
        let (enable_skill_commands, max_visible) = {
            let settings = self
                .session
                .settings_manager()
                .lock()
                .expect("settings lock");
            (
                settings.enable_skill_commands(),
                settings.autocomplete_max_visible() as usize,
            )
        };
        {
            let mut autocomplete = self.autocomplete.lock().expect("autocomplete");
            let max_visible = if max_visible == 0 {
                autocomplete.max_visible()
            } else {
                max_visible
            };
            *autocomplete = InteractiveAutocomplete::new(
                crate::modes::interactive::autocomplete::commands(
                    &self.session,
                    enable_skill_commands,
                ),
                crate::modes::interactive::autocomplete::argument_completers(&self.session),
                self.session.cwd(),
                max_visible,
            );
        }
        let max_visible = self
            .autocomplete
            .lock()
            .expect("autocomplete")
            .max_visible();
        self.editor.lock().set_autocomplete_max_visible(max_visible);
        self.close_autocomplete();
    }

    /// Close the dropdown (upstream `cancelAutocomplete`).
    pub fn close_autocomplete(&self) {
        let mut autocomplete = self.autocomplete.lock().expect("autocomplete");
        if !autocomplete.is_open() {
            return;
        }
        autocomplete.set_open(None, false);
        drop(autocomplete);
        self.editor.lock().set_autocomplete_list(None);
        self.mark_dirty();
    }

    /// Set the editor text programmatically and drop the dropdown: upstream
    /// `editor.setText` cancels the autocomplete first.
    /// Upstream `editor.setText` (also used by the host to restore editor
    /// text after a fork rebuilds the run loop).
    pub fn set_editor_text(&self, text: &str) {
        self.close_autocomplete();
        self.editor.lock().set_text(text);
    }

    /// Whether the dropdown is showing (upstream `isShowingAutocomplete`).
    pub fn autocomplete_is_open(&self) -> bool {
        self.autocomplete.lock().expect("autocomplete").is_open()
    }

    /// The open menu's prefix (tests / hosts).
    pub fn autocomplete_prefix(&self) -> Option<String> {
        self.autocomplete
            .lock()
            .expect("autocomplete")
            .open_prefix()
            .map(str::to_string)
    }

    /// The visible completion values (tests).
    pub fn autocomplete_items(&self) -> Vec<String> {
        self.editor
            .lock()
            .autocomplete_list()
            .map(|list| {
                list.filtered_items()
                    .iter()
                    .map(|item| item.value.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Upstream the editor's autocomplete request after a text change.
    pub fn update_autocomplete_on_change(&self) {
        let (lines, cursor_line, cursor_col) = {
            let editor = self.editor.lock();
            (
                editor.get_lines(),
                editor.get_cursor().0,
                editor.get_cursor().1,
            )
        };
        let should_request = self
            .autocomplete
            .lock()
            .expect("autocomplete")
            .should_request_on_change(&lines, cursor_line, cursor_col);
        if should_request {
            self.request_autocomplete(&lines, cursor_line, cursor_col, false, false);
        }
    }

    /// Upstream `handleTabCompletion`: apply the highlighted completion when
    /// the menu is open, else request suggestions (slash commands without
    /// `force`, file paths with it).
    pub fn autocomplete_tab(&self) -> Vec<ModeAction> {
        if self.autocomplete_is_open() {
            self.accept_autocomplete();
            self.close_autocomplete();
            self.mark_dirty();
            return Vec::new();
        }
        let (lines, cursor_line, cursor_col) = {
            let editor = self.editor.lock();
            (
                editor.get_lines(),
                editor.get_cursor().0,
                editor.get_cursor().1,
            )
        };
        let text_before = lines
            .get(cursor_line)
            .map(|line| line[..cursor_col.min(line.len())].to_string())
            .unwrap_or_default();
        let force = !pillar_tui::editor_autocomplete::SlashMenuContext::is_in_slash_command_context(
            cursor_line,
            &text_before,
        );
        if force {
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            if !pillar_tui::autocomplete::CombinedAutocompleteProvider::should_trigger_file_completion(
                &refs,
                cursor_line,
                cursor_col,
            ) {
                return Vec::new();
            }
        }
        self.request_autocomplete(&lines, cursor_line, cursor_col, force, true);
        Vec::new()
    }

    /// Upstream the editor's `tui.select.confirm` handling while the menu is
    /// open: apply the completion; a `/command` name completion falls through
    /// to submit (so `/help` + Enter runs the command), arguments do not.
    pub fn autocomplete_accept(&self) -> Vec<ModeAction> {
        let Some(prefix) = self.autocomplete_prefix() else {
            return Vec::new();
        };
        let applied = self.accept_autocomplete();
        // Upstream: a completed `/command` name falls through to submit (so
        // `/help` + Enter runs the command); a completed argument does not, and
        // an empty menu submits whatever is typed.
        let submit = if applied {
            prefix.starts_with('/')
        } else {
            true
        };
        self.close_autocomplete();
        if !submit {
            return Vec::new();
        }
        let text = self.editor.lock().get_text();
        self.handle_submit(&text)
    }

    /// Apply the highlighted completion to the editor (upstream
    /// `applyCompletion`); answers whether a completion was applied. The menu is
    /// left to the caller to close.
    fn accept_autocomplete(&self) -> bool {
        let (lines, cursor_line, cursor_col, item, prefix) = {
            let autocomplete = self.autocomplete.lock().expect("autocomplete");
            let Some(prefix) = autocomplete.open_prefix().map(str::to_string) else {
                return false;
            };
            let editor = self.editor.lock();
            let Some(item) = editor
                .autocomplete_list()
                .and_then(|list| list.get_selected_item())
                .map(|item| pillar_tui::autocomplete::AutocompleteItem {
                    value: item.value.clone(),
                    label: item.label.clone(),
                    description: item.description.clone(),
                })
            else {
                return false;
            };
            (
                editor.get_lines(),
                editor.get_cursor().0,
                editor.get_cursor().1,
                item,
                prefix,
            )
        };
        let (applied, line, col) = self.autocomplete.lock().expect("autocomplete").apply(
            &lines,
            cursor_line,
            cursor_col,
            &item,
            &prefix,
        );
        self.editor.lock().set_lines_and_cursor(&applied, line, col);
        self.mark_dirty();
        true
    }

    /// Upstream `runAutocompleteRequest` + `applyAutocompleteSuggestions`.
    fn request_autocomplete(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
        explicit_tab: bool,
    ) {
        let suggestions = {
            let autocomplete = self.autocomplete.lock().expect("autocomplete");
            // The ported provider is synchronous; the (unwired) `@`-attachment
            // debounce is the only one upstream would apply here.
            if autocomplete.debounce_ms(force, explicit_tab, lines, cursor_line, cursor_col) > 0 {
                return;
            }
            autocomplete.suggestions(lines, cursor_line, cursor_col, force)
        };
        let Some(suggestions) = suggestions else {
            self.close_autocomplete();
            return;
        };
        let (max_visible, best_match) = {
            let autocomplete = self.autocomplete.lock().expect("autocomplete");
            (
                autocomplete.max_visible(),
                autocomplete.best_match_index(&suggestions.items, &suggestions.prefix),
            )
        };
        let items: Vec<(String, String, Option<String>)> = suggestions
            .items
            .iter()
            .map(|item| {
                (
                    item.value.clone(),
                    item.label.clone(),
                    item.description.clone(),
                )
            })
            .collect();
        let mut list = pillar_tui::editor_autocomplete::create_autocomplete_list(
            &suggestions.prefix,
            &items,
            max_visible,
        );
        if best_match >= 0 {
            list.set_selected_index(best_match as usize);
        }
        {
            let mut autocomplete = self.autocomplete.lock().expect("autocomplete");
            autocomplete.set_open(Some(suggestions.prefix.clone()), force);
        }
        self.editor.lock().set_autocomplete_list(Some(list));
        self.mark_dirty();
    }

    /// Upstream `handleDequeue`.
    pub fn handle_dequeue(&self) {
        let restored = self.restore_queued_messages_to_editor();
        if restored == 0 {
            self.transcript
                .lock()
                .show_status("No queued messages to restore");
        } else {
            self.transcript.lock().show_status(&format!(
                "Restored {restored} queued message{} to editor",
                if restored > 1 { "s" } else { "" }
            ));
        }
        self.mark_dirty();
    }

    /// Upstream the `CustomEditor` app-action dispatch. Returns the actions the
    /// host must execute.
    ///
    /// divergence: selector-backed actions (model select, session tree / fork /
    /// resume / new, copy, suspend, clipboard paste, external editor) are not
    /// ported and answer a warning.
    pub fn handle_app_action(&self, action: &str) -> Vec<ModeAction> {
        match action {
            "app.interrupt" => self.handle_escape(),
            "app.clear" => self.handle_ctrl_c(),
            "app.exit" => self.handle_ctrl_d(),
            "app.tools.expand" => {
                self.toggle_tool_output_expansion();
                Vec::new()
            }
            "app.thinking.toggle" => {
                self.toggle_thinking_block_visibility();
                Vec::new()
            }
            "app.thinking.cycle" => {
                self.cycle_thinking_level();
                Vec::new()
            }
            "app.model.cycleForward" => vec![ModeAction::CycleModel { forward: true }],
            "app.model.cycleBackward" => vec![ModeAction::CycleModel { forward: false }],
            "app.model.select" => self.show_model_picker(None),
            "app.session.resume" => self.show_session_selector(),
            "app.session.tree" => self.show_tree_selector(None),
            "app.session.fork" => self.show_user_message_selector(None),
            "app.message.dequeue" => {
                self.handle_dequeue();
                Vec::new()
            }
            other => {
                self.transcript
                    .lock()
                    .show_warning(&format!("{other} is not ported yet"));
                self.mark_dirty();
                Vec::new()
            }
        }
    }
}

/// Convert a `ctx.ui.setWorkingIndicator` argument into
/// [`WorkingIndicatorOptions`] (upstream `WorkingIndicatorOptions`:
/// `frames: []` hides the indicator, `nil` restores the default).
fn working_indicator_options(
    value: &serde_json::Value,
) -> Result<crate::core::extensions_types::WorkingIndicatorOptions, String> {
    let frames = match value.get("frames") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(frames)) => Some(
            frames
                .iter()
                .map(|frame| {
                    frame.as_str().map(str::to_string).ok_or_else(|| {
                        "ctx.ui.set_working_indicator: frames must be strings".to_string()
                    })
                })
                .collect::<Result<Vec<String>, String>>()?,
        ),
        Some(_) => {
            return Err("ctx.ui.set_working_indicator: `frames` must be an array".to_string());
        }
    };
    let interval_ms = match value.get("intervalMs") {
        None | Some(serde_json::Value::Null) => None,
        Some(interval) => Some(interval.as_u64().ok_or_else(|| {
            "ctx.ui.set_working_indicator: `intervalMs` must be a number".to_string()
        })?),
    };
    Ok(crate::core::extensions_types::WorkingIndicatorOptions {
        frames,
        interval_ms,
    })
}

/// The queue delivery mode for the session (upstream the `"all" |
/// "one-at-a-time"` string).
fn queue_mode(value: &str) -> pillar_agent::types::QueueMode {
    match value {
        "one-at-a-time" => pillar_agent::types::QueueMode::OneAtATime,
        _ => pillar_agent::types::QueueMode::All,
    }
}

/// Upstream `new Date().toISOString()` — the port's summary messages take
/// the resolved millisecond timestamp.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
