//! Port of packages/coding-agent/src/modes/interactive/interactive-mode.ts
//! (pi v0.84.3), starting with the pure helpers at the top of the module.
//!
//! Progress: the value helpers below are ported. The `InteractiveMode` class
//! (rendering loop, slash-command handling, selectors) is ported
//! incrementally; helpers that need not-yet-ported types (`AuthSelectorProvider`
//! login completions, `ExpandableText`) land with those types.

use std::io::IsTerminal;

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
use crate::core::settings_manager::DoubleEscapeAction;
use crate::core::truncate::TruncationResult;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::footer::FooterComponent;
use crate::modes::interactive::components::status_indicator::{
    CompactionStatusReason, RetryStatusIndicator, StatusIndicatorKind, compaction_status_indicator,
};
use crate::modes::interactive::components::thinking_selector::{
    ThinkingSelectorComponent, ThinkingSelectorOutcome,
};
use crate::modes::interactive::mode_ui::{PendingMessagesUi, QueueMode, StatusUi};
use crate::modes::interactive::theme::get_editor_theme;
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
    /// Upstream `agent.abort()` (Escape while streaming).
    Abort,
    /// Upstream `session.cycleModel(direction)`.
    CycleModel { forward: bool },
    /// The editor slot changed (a selector was shown or closed; upstream
    /// `showSelector`'s `editorContainer` swap + `setFocus`).
    EditorSlotChanged,
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
    show_terminal_progress: bool,
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
    /// Monotonic token so a stale `done` cannot close a newer selector.
    next_selector_token: AtomicU64,
}

/// The selector currently shown in place of the editor (upstream the
/// `editorContainer` child plus `activeSelectorToken`).
pub enum ActiveSelector {
    Thinking {
        token: u64,
        component: Shared<ThinkingSelectorComponent>,
    },
}

impl ActiveSelector {
    fn token(&self) -> u64 {
        match self {
            ActiveSelector::Thinking { token, .. } => *token,
        }
    }

    /// The mountable component (upstream `created.component`).
    fn mount(&self) -> Box<dyn pillar_tui::tui::Component> {
        match self {
            ActiveSelector::Thinking { component, .. } => Box::new(
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

        let transcript =
            InteractiveTranscript::new(transcript_settings, None, markdown_transformers, &cwd);

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
            show_terminal_progress: options.show_terminal_progress.unwrap_or(false),
            on_terminal_title: options.on_terminal_title,
            on_terminal_progress: options.on_terminal_progress,
            bash_mode: AtomicBool::new(false),
            dirty: AtomicBool::new(true),
            last_sigint_ms: AtomicU64::new(0),
            last_escape_ms: AtomicU64::new(0),
            bash_component: std::sync::Mutex::new(None),
            pending_bash_components: std::sync::Mutex::new(Vec::new()),
            active_selector: std::sync::Mutex::new(None),
            next_selector_token: AtomicU64::new(1),
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
        if self.show_terminal_progress {
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
        const SELECTOR_COMMANDS: [&str; 20] = [
            "/settings",
            "/scoped-models",
            "/model",
            "/export",
            "/import",
            "/share",
            "/copy",
            "/session",
            "/changelog",
            "/hotkeys",
            "/fork",
            "/clone",
            "/tree",
            "/trust",
            "/login",
            "/logout",
            "/new",
            "/reload",
            "/debug",
            "/resume",
        ];
        for command in SELECTOR_COMMANDS {
            if text == command || text.starts_with(&format!("{command} ")) {
                self.transcript.lock().show_warning(&format!(
                    "{command} is not available yet (selector UI is not ported)"
                ));
                self.editor.lock().set_text("");
                return Vec::new();
            }
        }

        if text == "/quit" {
            self.editor.lock().set_text("");
            return vec![ModeAction::Shutdown];
        }
        if text == "/arminsayshi" || text == "/dementedelves" {
            // Novelty commands (upstream `handleArminSaysHi` /
            // `handleDementedDelves`) are not ported.
            self.transcript
                .lock()
                .show_warning("This command is not ported");
            self.editor.lock().set_text("");
            return Vec::new();
        }
        if text == "/compact" || text.starts_with("/compact ") {
            let instructions = text
                .strip_prefix("/compact ")
                .map(str::trim)
                .filter(|instructions| !instructions.is_empty())
                .map(str::to_string);
            self.editor.lock().set_text("");
            return vec![ModeAction::Compact { instructions }];
        }
        if text == "/thinking" || text.starts_with("/thinking ") {
            let search = text
                .strip_prefix("/thinking ")
                .map(str::trim)
                .filter(|search| !search.is_empty());
            self.editor.lock().set_text("");
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
        if text == "/name" || text.starts_with("/name ") {
            self.handle_name_command(text);
            self.editor.lock().set_text("");
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
                    self.editor.lock().set_text(text);
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
            self.editor.lock().set_text("");
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

        // Streaming submissions steer the running turn.
        if self.session.is_streaming() {
            self.editor.lock().add_to_history(text);
            self.editor.lock().set_text("");
            let (steering, follow_up) = self.session_queues();
            self.pending.lock().update_display(&steering, &follow_up);
            return vec![ModeAction::Prompt {
                text: text.to_string(),
                streaming_behavior: Some(StreamingBehavior::Steer),
            }];
        }

        // Normal message submission: move any pending bash blocks into the
        // chat first (upstream `flushPendingBashComponents`).
        self.flush_pending_bash_components();
        self.editor.lock().add_to_history(text);
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
        let changed = self.status.lock().tick();
        if changed {
            self.mark_dirty();
        }
        changed
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
        self.editor.lock().set_text(&combined);
        self.update_pending_display();
        all_queued.len()
    }

    /// Upstream `handleCtrlC`: first press clears, a second within 500 ms
    /// shuts down.
    pub fn handle_ctrl_c(&self) -> Vec<ModeAction> {
        let now = now_ms();
        let last = self.last_sigint_ms.load(Ordering::SeqCst);
        self.editor.lock().set_text("");
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
            self.restore_queued_messages_to_editor();
            return vec![ModeAction::Abort];
        }
        if self.session.is_bash_running() {
            self.session.abort_bash();
            self.transcript.lock().show_status("Bash command cancelled");
            return Vec::new();
        }
        if self.bash_mode.load(Ordering::SeqCst) {
            self.editor.lock().set_text("");
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
                    self.transcript
                        .lock()
                        .show_warning("Session tree / fork selectors are not ported yet");
                } else {
                    self.last_escape_ms.store(now, Ordering::SeqCst);
                }
            }
        }
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
        let (token, component) = {
            let guard = self.active_selector.lock().expect("active selector");
            match guard.as_ref() {
                Some(ActiveSelector::Thinking { token, component }) => (*token, component.clone()),
                None => return None,
            }
        };
        let outcome = component.lock().handle_key(data);
        Some(match outcome {
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
        })
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

/// Upstream `new Date().toISOString()` — the port's summary messages take
/// the resolved millisecond timestamp.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
