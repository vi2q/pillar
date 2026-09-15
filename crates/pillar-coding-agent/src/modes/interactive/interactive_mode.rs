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
use pillar_tui::tui::TuiMode;

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

use std::sync::atomic::AtomicBool;

use pillar_tui::components::{Spacer, Text};

use crate::core::agent_session_class::{AgentSession, AgentSessionEvent, StreamingBehavior};
use crate::core::footer_data_provider::FooterDataProvider;
use crate::core::messages::{CodingAgentMessage, create_compaction_summary_message};
use crate::core::resource_loader::GitPaths;
use crate::core::session_entries::SessionEntry;
use crate::modes::interactive::components::footer::FooterComponent;
use crate::modes::interactive::components::status_indicator::{
    CompactionStatusReason, RetryStatusIndicator, StatusIndicatorKind, compaction_status_indicator,
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
        const SELECTOR_COMMANDS: [&str; 21] = [
            "/settings",
            "/scoped-models",
            "/model",
            "/thinking",
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

        // Normal message submission.
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

/// Upstream `new Date().toISOString()` — the port's summary messages take
/// the resolved millisecond timestamp.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
