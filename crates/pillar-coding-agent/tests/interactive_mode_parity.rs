//! Parity tests for the `InteractiveMode` assembly (pi v0.84.3
//! `interactive-mode.ts`): the event dispatch and the submit router.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_agent::{Agent, AgentOptions, AgentState, FauxModelRef};
use pillar_ai::auth_types::{Credential, CredentialInfo, CredentialStore};
use pillar_ai::error::AiError;
use pillar_ai::types::{Content, Message, StopReason, Usage, UsageCost, UserContent};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, StreamingBehavior,
};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::model_runtime::ModelRuntime;
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};

use pillar_coding_agent::core::session_manager::{SessionInfo, SessionManager};
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use pillar_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions, ModeAction,
};
use pillar_coding_agent::modes::interactive::mode_ui::QueueMode;
use pillar_coding_agent::modes::interactive::theme;
use pillar_coding_agent::modes::interactive::transcript::TranscriptSettings;
use pillar_tui::tui::{Component as _, TuiMode};

static THEME_LOCK: Mutex<()> = Mutex::new(());

/// In-memory credential store so the runtime's availability snapshot can be
/// refreshed without touching a real auth file.
#[derive(Default)]
struct MemCredentials(Mutex<BTreeMap<String, Credential>>);

#[async_trait]
impl CredentialStore for MemCredentials {
    async fn read(
        &self,
        provider_id: &str,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        Ok(self.0.lock().unwrap().get(provider_id).cloned())
    }
    async fn list(
        &self,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, AiError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys()
            .map(|provider_id| CredentialInfo {
                provider_id: provider_id.clone(),
                kind: "api_key".to_string(),
            })
            .collect())
    }
    async fn modify(
        &self,
        provider_id: &str,
        f: pillar_ai::auth_types::CredentialModifier<'_>,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<Option<Credential>, AiError> {
        let current = self.0.lock().unwrap().get(provider_id).cloned();
        let next = f(current)
            .await
            .map_err(|e| AiError::Other(e.to_string()))?;
        let mut map = self.0.lock().unwrap();
        if let Some(credential) = next {
            map.insert(provider_id.to_string(), credential);
        }
        Ok(map.get(provider_id).cloned())
    }
    async fn delete(
        &self,
        provider_id: &str,
        _options: Option<&pillar_ai::auth_types::AuthOperationOptions>,
    ) -> Result<(), AiError> {
        self.0.lock().unwrap().remove(provider_id);
        Ok(())
    }
}

fn install_dark() {
    theme::init_theme(Some("dark"));
}

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

fn usage(cost: f64) -> Usage {
    Usage {
        input: 100,
        output: 100,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 200,
        cost: UsageCost {
            total: cost,
            ..UsageCost::default()
        },
    }
}

fn assistant_message(text: &str, stop_reason: StopReason) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![Content::text(text)],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            response_model: None,
            usage: usage(0.0),
            stop_reason,
            deferred: None,
            error_message: None,
            response_id: None,
            diagnostics: Vec::new(),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    )))
}

fn session() -> Arc<AgentSession> {
    session_with_scoped_models(Vec::new())
}

/// Upstream the `--models` scope: the session starts with these models in
/// scope (the model selector's initial `scoped` scope).
fn session_with_scoped_models(
    scoped_models: Vec<pillar_coding_agent::core::model_mutation::ScopedModel>,
) -> Arc<AgentSession> {
    session_with_models_impl(scoped_models, false)
}

/// Same, but the runtime's availability snapshot also reports the anthropic
/// models (an API key is configured, like a real agent dir). The
/// `/scoped-models` selector reads `getAvailableSnapshot()`.
fn session_with_available_scoped_models(
    scoped_models: Vec<pillar_coding_agent::core::model_mutation::ScopedModel>,
) -> Arc<AgentSession> {
    session_with_models_impl(scoped_models, true)
}

fn session_with_models_impl(
    scoped_models: Vec<pillar_coding_agent::core::model_mutation::ScopedModel>,
    make_available: bool,
) -> Arc<AgentSession> {
    let model = FauxModelRef {
        id: "claude-sonnet-4-5".to_string(),
        name: "Claude Sonnet 4.5".to_string(),
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        base_url: String::new(),
        reasoning: true,
        input: vec!["text".to_string()],
        cost: UsageCost::default(),
        context_window: 200_000,
        max_tokens: 8_000,
    };
    let stream_fn = pillar_agent::StreamFn::new(|_context, _options| async {
        unreachable!("mode tests never prompt the agent")
    });
    let mut options = AgentOptions::new(stream_fn);
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model,
        thinking_level: pillar_agent::AgentThinkingLevel::Off,
        tools: Vec::new(),
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("/tmp/pillar-mode-cwd", None).expect("in-memory session"),
    ));
    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({}),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: "/tmp/pillar-mode-agent-dir".to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));
    // A unique directory per session: the tests in this binary run in parallel
    // and a shared `models.json` races a truncating write against another
    // test's read (the loader then reports "EOF while parsing a value").
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-mode-runtime-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let models_path = dir.join("models.json");
    // A valid empty config: `{}` would leave a `ModelRuntime::get_error()`
    // config error that the model selector renders as its error message.
    std::fs::write(&models_path, "{\"providers\":{}}").expect("write models");
    let mut runtime = ModelRuntime::new(
        pillar_coding_agent::core::model_runtime::CreateModelRuntimeOptions {
            models_path: Some(models_path),
            models_store: Some(Arc::new(
                pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore::new(),
            )),
            credentials: Some(Arc::new(MemCredentials::default())),
            ..Default::default()
        },
    )
    .expect("runtime");
    if make_available {
        // Configure anthropic auth and refresh the availability snapshot (the
        // selector reads `getAvailableSnapshot()`).
        tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(async {
                runtime
                    .set_runtime_api_key("anthropic", "sk-test")
                    .await
                    .expect("api key");
                runtime
                    .refresh_availability(None)
                    .await
                    .expect("availability");
            });
    }

    let mut config = AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime),
        Arc::new(Mutex::new(
            pillar_coding_agent::core::extensions_runner::ExtensionRunner::new(Vec::new()),
        )),
    );
    config.scoped_models = scoped_models;
    Arc::new(AgentSession::new(config))
}

fn make_mode(session: &Arc<AgentSession>) -> InteractiveMode {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    InteractiveMode::new(
        Arc::clone(session),
        TranscriptSettings::default(),
        Vec::new(),
        InteractiveModeOptions {
            tui_mode: Some(TuiMode::Regular),
            clear_on_shrink: Some(false),
            show_terminal_progress: Some(false),
            version: Some("0.84.3".to_string()),
            on_terminal_title: Some(Arc::new(|_| {})),
            on_terminal_progress: None,
            cwd_git_paths: None,
            ..Default::default()
        },
    )
}

fn plain(container: &mut pillar_tui::tui::Container, width: usize) -> String {
    container
        .render(width)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

// --- event dispatch -----------------------------------------------------------------------

#[test]
fn turn_start_shows_the_working_indicator_and_agent_end_clears_it() {
    let session = session();
    let mode = make_mode(&session);

    mode.handle_event(&AgentSessionEvent::TurnStart);
    {
        let mut status = mode.status().lock();
        assert_eq!(
            status.active_kind(),
            Some(pillar_coding_agent::modes::interactive::components::status_indicator::StatusIndicatorKind::Working)
        );
        let body = plain(&mut status.status, 50);
        assert!(body.contains("Working..."), "{body:?}");
    }

    // The transcript gets the streaming scaffolding through the session
    // event subset (agent_end clears the working indicator).
    mode.handle_event(&AgentSessionEvent::AgentEnd {
        messages: Vec::new(),
        will_retry: false,
    });
    let mut status = mode.status().lock();
    assert!(status.active_kind().is_none());
    assert!(status.status.render(50).is_empty(), "clear-on-shrink off");
}

#[test]
fn queue_update_fills_the_pending_display() {
    let session = session();
    let mode = make_mode(&session);
    mode.handle_event(&AgentSessionEvent::QueueUpdate {
        steering: vec!["add tests".to_string()],
        follow_up: Vec::new(),
    });
    let mut pending = mode.pending().lock();
    let body = plain(&mut pending.container, 70);
    assert!(body.contains("Steering: add tests"), "{body:?}");
    assert!(body.contains("to edit all queued messages"), "{body:?}");
}

#[test]
fn session_info_changes_the_terminal_title() {
    let session = session();
    let titles = Arc::new(Mutex::new(Vec::<String>::new()));
    let titles_for_options = Arc::clone(&titles);
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mode = InteractiveMode::new(
        Arc::clone(&session),
        TranscriptSettings::default(),
        Vec::new(),
        InteractiveModeOptions {
            on_terminal_title: Some(Arc::new(move |title| {
                titles_for_options
                    .lock()
                    .expect("titles")
                    .push(title.to_string());
            })),
            ..InteractiveModeOptions::default()
        },
    );
    session
        .session_manager()
        .lock()
        .expect("lock")
        .append_session_info("my session")
        .expect("append");
    mode.handle_event(&AgentSessionEvent::SessionInfoChanged {
        name: Some("my session".to_string()),
    });
    let titles = titles.lock().expect("titles");
    assert_eq!(
        titles.last().map(String::as_str),
        Some("pillar - my session - pillar-mode-cwd"),
        "{titles:?}"
    );
}

#[test]
fn compaction_lifecycle_shows_the_indicator_and_rebuilds() {
    let session = session();
    let mode = make_mode(&session);

    // Seed a session: two turns, then a compaction keeping the last one.
    let sm = session.session_manager();
    let mut sm = sm.lock().expect("lock");
    sm.append_message(CodingAgentMessage::Base(Message::User {
        content: UserContent::Text("before compaction".to_string()),
        timestamp: 1,
    }))
    .expect("append");
    let kept = sm
        .append_message(assistant_message("kept after compaction", StopReason::Stop))
        .expect("append");
    sm.append_compaction(
        "summary of old turns",
        &kept,
        12_345,
        None,
        false,
        Some(usage(0.02)),
    )
    .expect("append");
    drop(sm);

    // Expanded so the compaction summary body renders (upstream gates on
    // the toolOutputExpanded setting).
    mode.transcript().lock().set_tool_output_expanded(true);
    mode.handle_event(&AgentSessionEvent::CompactionStart { reason: "manual" });
    {
        let mut status = mode.status().lock();
        assert!(status.active_kind().is_some(), "compaction indicator");
        let body = plain(&mut status.status, 70);
        assert!(body.contains("Compacting context..."), "{body:?}");
    }

    let result = pillar_coding_agent::core::compaction::driver::CompactionResult {
        summary: "the summary".to_string(),
        first_kept_entry_id: kept,
        tokens_before: 12_345,
        estimated_tokens_after: Some(2_000),
        usage: Some(usage(0.02)),
        details: None,
    };
    // Notices off by default; the summary is still appended.
    let actions = mode.handle_event(&AgentSessionEvent::CompactionEnd {
        reason: "manual",
        result: Some(result),
        aborted: false,
        will_retry: false,
        error_message: None,
    });
    {
        let status = mode.status().lock();
        assert!(status.active_kind().is_none(), "indicator cleared");
    }
    let mut transcript = mode.transcript().lock();
    let body = transcript
        .chat
        .render(80)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("kept after compaction"), "{body:?}");
    assert!(body.contains("the summary"), "{body:?}");
    assert!(!body.contains("before compaction"), "{body:?}");

    // The compaction queue was empty, so no actions follow.
    assert!(actions.is_empty(), "{actions:?}");
}

#[test]
fn auto_retry_shows_the_countdown_and_reports_final_failures() {
    let session = session();
    let mode = make_mode(&session);

    mode.handle_event(&AgentSessionEvent::AutoRetryStart {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 2500,
        error_message: "boom".to_string(),
    });
    {
        let mut status = mode.status().lock();
        let body = plain(&mut status.status, 70);
        assert!(body.contains("Retrying (2/3) in 3s"), "{body:?}");
    }

    mode.handle_event(&AgentSessionEvent::AutoRetryEnd {
        success: false,
        attempt: 3,
        final_error: Some("still failing".to_string()),
    });
    let mut transcript = mode.transcript().lock();
    let body = transcript
        .chat
        .render(80)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("Error: Retry failed after 3 attempts: still failing"),
        "{body:?}"
    );
}

// --- submit routing -----------------------------------------------------------------------

#[test]
fn submit_routes_the_non_selector_commands() {
    let session = session();
    let mode = make_mode(&session);

    // Empty submissions do nothing.
    assert!(mode.handle_submit("").is_empty());
    assert!(mode.handle_submit("   ").is_empty());

    // Normal submission goes to the run loop and into the history.
    assert_eq!(
        mode.handle_submit("hello there"),
        vec![ModeAction::SubmitToLoop("hello there".to_string())]
    );
    assert_eq!(mode.editor().lock().get_text(), "");

    // /quit shuts down.
    assert_eq!(mode.handle_submit("/quit"), vec![ModeAction::Shutdown]);

    // /compact reports the compaction action.
    assert_eq!(
        mode.handle_submit("/compact focus on tests"),
        vec![ModeAction::Compact {
            instructions: Some("focus on tests".to_string())
        }]
    );
    assert_eq!(
        mode.handle_submit("/compact"),
        vec![ModeAction::Compact { instructions: None }]
    );

    // /name sets the session name and reports it.
    mode.handle_submit("/name my session");
    assert_eq!(
        session
            .session_manager()
            .lock()
            .expect("lock")
            .session_name()
            .as_deref(),
        Some("my session")
    );
    let body = {
        let mut transcript = mode.transcript().lock();
        transcript
            .chat
            .render(80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(body.contains("Session name set: my session"), "{body:?}");

    // Selector-backed commands answer a warning (divergence until the
    // selectors land). `/model`, `/settings` and `/hotkeys` moved out of that
    // list with their slices.
    mode.handle_submit("/trust");
    let body = {
        let mut transcript = mode.transcript().lock();
        transcript
            .chat
            .render(90)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(body.contains("/trust is not available yet"), "{body:?}");

    // `/settings` now opens the panel in the editor slot.
    assert_eq!(
        mode.handle_submit("/settings"),
        vec![ModeAction::EditorSlotChanged]
    );
    mode.handle_selector_key("\u{1b}").expect("settings");
}

#[test]
fn submit_routes_bash_and_steering() {
    let session = session();
    let mode = make_mode(&session);

    // `!` bash (normal) and `!!` (excluded from context).
    assert_eq!(
        mode.handle_submit("!ls -la"),
        vec![ModeAction::Bash {
            command: "ls -la".to_string(),
            excluded: false
        }]
    );
    assert_eq!(
        mode.handle_submit("!!secret build"),
        vec![ModeAction::Bash {
            command: "secret build".to_string(),
            excluded: true
        }]
    );
    // The editor history got the raw commands.
    assert!(mode.editor().lock().get_text().is_empty());

    // `!` with nothing after falls through to normal submission.
    assert_eq!(
        mode.handle_submit("!"),
        vec![ModeAction::SubmitToLoop("!".to_string())]
    );
}

#[test]
fn compaction_end_flushes_the_compaction_queue_as_actions() {
    let session = session();
    let mode = make_mode(&session);

    // Queue a compaction message directly in the pending UI and end the
    // compaction without a retry.
    mode.pending()
        .lock()
        .queue_compaction_message("queued for after".to_string(), QueueMode::Steer);
    let actions = mode.handle_event(&AgentSessionEvent::CompactionEnd {
        reason: "manual",
        result: None,
        aborted: false,
        will_retry: false,
        error_message: None,
    });
    assert_eq!(
        actions,
        vec![ModeAction::Prompt {
            text: "queued for after".to_string(),
            streaming_behavior: Some(StreamingBehavior::Steer),
        }]
    );

    // will_retry re-queues via steer/followUp actions instead.
    mode.pending()
        .lock()
        .queue_compaction_message("second queued".to_string(), QueueMode::FollowUp);
    let actions = mode.handle_event(&AgentSessionEvent::CompactionEnd {
        reason: "manual",
        result: None,
        aborted: false,
        will_retry: true,
        error_message: None,
    });
    assert_eq!(
        actions,
        vec![ModeAction::FollowUp("second queued".to_string())]
    );
}

#[test]
fn render_initial_messages_populates_the_transcript() {
    let session = session();
    {
        let sm = session.session_manager();
        let mut sm = sm.lock().expect("lock");
        sm.append_message(CodingAgentMessage::Base(Message::User {
            content: UserContent::Text("first message".to_string()),
            timestamp: 1,
        }))
        .expect("append");
        sm.append_message(assistant_message("assistant reply", StopReason::Stop))
            .expect("append");
    }
    let mode = make_mode(&session);
    mode.render_initial_messages();
    let mut transcript = mode.transcript().lock();
    let body = transcript
        .chat
        .render(80)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("first message"), "{body:?}");
    assert!(body.contains("assistant reply"), "{body:?}");
}

#[test]
fn message_entries_flow_through_the_transcript() {
    let session = session();
    let mode = make_mode(&session);

    // A user message start fills the pending display and the transcript.
    mode.handle_event(&AgentSessionEvent::MessageStart {
        message: pillar_agent::types::AgentMessage::Message(Message::User {
            content: UserContent::Text("a question".to_string()),
            timestamp: 1,
        }),
    });
    let mut transcript = mode.transcript().lock();
    let body = transcript
        .chat
        .render(80)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("a question"), "{body:?}");
}

/// Mounting must hand the editor a real focus target: `Shared<C>` cannot
/// forward `as_focusable`, so before `FocusHandle` the TUI's `set_focus` was
/// a no-op, the editor never rendered its hardware-cursor marker, and the
/// terminal placed IME preedit / the cursor at the end of the frame.
#[test]
fn mount_focuses_the_editor_and_emits_the_cursor_marker() {
    use pillar_tui::process_terminal::{NullTerminalIo, ProcessTerminal};
    use pillar_tui::tui::{CURSOR_MARKER, Focusable as _, TuiBase};

    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let session = session();
    let mode = InteractiveMode::new(
        Arc::clone(&session),
        TranscriptSettings::default(),
        Vec::new(),
        InteractiveModeOptions {
            tui_mode: Some(TuiMode::Regular),
            ..Default::default()
        },
    );

    let mut base = TuiBase::new(
        Box::new(ProcessTerminal::with_io(Box::new(NullTerminalIo))),
        TuiMode::Regular,
    );
    let editor = mode.mount(&mut base);
    base.set_focus(Some(editor));
    assert!(
        mode.editor().lock().is_focused(),
        "the mounted editor receives focus"
    );

    let lines = base.render(80);
    let markers = lines
        .iter()
        .filter(|line| line.contains(CURSOR_MARKER))
        .count();
    assert_eq!(markers, 1, "one cursor marker in the frame: {lines:?}");
}

/// Live bash blocks (upstream `handleBashCommand`'s UI half): the block
/// renders while the command runs, streamed output lands on it, and a block
/// created while the agent streams waits in the pending area until the next
/// submission flushes it into the chat.
#[test]
fn bash_blocks_render_live_and_flush_from_the_pending_area() {
    // `make_mode` installs the theme and holds THEME_LOCK itself.
    let session = session();
    let mode = make_mode(&session);

    // Idle: the block goes straight into the chat.
    mode.begin_bash("printf hello", false);
    assert_eq!(mode.pending().lock().container.len(), 0);
    assert_eq!(mode.transcript().lock().chat.len(), 1);
    mode.append_bash_output("hello\n");
    mode.complete_bash(Some(0), false, None, None);
    let body = plain(&mut mode.transcript().lock().chat, 60);
    assert!(body.contains("printf hello"), "{body:?}");
    assert!(body.contains("hello"), "{body:?}");

    // Streaming: the block waits in the pending area and moves to the chat
    // when the next submission flushes it.
    let mode = make_mode(&session);
    mode.begin_bash_deferred("echo pending", false, true);
    assert_eq!(mode.transcript().lock().chat.len(), 0);
    assert_eq!(mode.pending().lock().container.len(), 1);
    mode.flush_pending_bash_components();
    assert_eq!(mode.pending().lock().container.len(), 0);
    assert_eq!(mode.transcript().lock().chat.len(), 1);
    let body = plain(&mut mode.transcript().lock().chat, 60);
    assert!(body.contains("echo pending"), "{body:?}");
}

// --- thinking selector (upstream `/thinking` + ThinkingSelectorComponent) --------

fn editor_slot_body(mode: &InteractiveMode, width: usize) -> String {
    let mut slot = mode.editor_slot_component();
    let lines = slot.render(width);
    lines
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn thinking_command_shows_the_selector_and_enter_selects_a_level() {
    let session = session();
    let mode = make_mode(&session);

    // `/thinking` opens the selector in the editor slot.
    let actions = mode.handle_submit("/thinking");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 60);
    assert!(body.contains("Thinking Level"), "{body:?}");
    assert!(body.contains("Enter to select"), "{body:?}");
    assert!(body.contains("high"), "{body:?}");

    // Enter picks the highlighted level (the current one) and closes.
    let actions = mode.handle_selector_key("\r").expect("selector active");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(!mode.has_active_selector());
    assert_eq!(session.thinking_level(), "off", "current level applied");

    // Down + Enter selects the next level.
    mode.handle_submit("/thinking");
    assert_eq!(
        mode.handle_selector_key("\u{1b}[B")
            .expect("selector active"),
        Vec::new(),
        "down"
    );
    let actions = mode.handle_selector_key("\r").expect("selector active");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert_eq!(session.thinking_level(), "minimal");
}

#[test]
fn thinking_selector_search_filters_and_escape_cancels() {
    let session = session();
    let mode = make_mode(&session);
    let before = session.thinking_level();

    mode.handle_submit("/thinking");
    // Typing filters the list (fuzzy match on the label).
    for ch in "min".chars() {
        assert_eq!(
            mode.handle_selector_key(&ch.to_string())
                .expect("selector active"),
            Vec::new()
        );
    }
    let body = editor_slot_body(&mode, 60);
    assert!(body.contains("minimal"), "{body:?}");
    // Fuzzy matching keeps any level whose label/description contains the
    // query characters; the unrelated ones drop out.
    assert!(!body.contains("No reasoning"), "off filtered out: {body:?}");
    assert!(
        !body.contains("Deep reasoning"),
        "high filtered out: {body:?}"
    );

    // Escape cancels without changing the level.
    let actions = mode.handle_selector_key("\u{1b}").expect("selector active");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(!mode.has_active_selector());
    assert_eq!(session.thinking_level(), before);
}

#[test]
fn thinking_selector_ctrl_s_persists_the_default_and_command_takes_a_level() {
    let session = session();
    let mode = make_mode(&session);

    mode.handle_submit("/thinking");
    mode.handle_selector_key("\u{1b}[B")
        .expect("selector active"); // down
    let actions = mode.handle_selector_key("\u{13}").expect("selector active"); // ctrl+s
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    let persisted = session
        .settings_manager()
        .lock()
        .expect("settings")
        .default_thinking_level();
    assert_eq!(
        persisted.as_deref(),
        Some(session.thinking_level().as_str())
    );

    // `/thinking <level>` sets it directly without opening the selector.
    let actions = mode.handle_submit("/thinking low");
    assert!(actions.is_empty(), "{actions:?}");
    assert!(!mode.has_active_selector());
    assert_eq!(session.thinking_level(), "low");

    // An unknown level reports the available ones.
    mode.handle_submit("/thinking nope");
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Unknown thinking level"), "{body:?}");
}

// --- model selector (upstream `/model` + ModelSelectorComponent) ---------------

/// A scoped model entry for the `--models` scope (`core::model_mutation`).
fn scoped_model(id: &str) -> pillar_coding_agent::core::model_mutation::ScopedModel {
    pillar_coding_agent::core::model_mutation::ScopedModel {
        model: pillar_ai::types::Model {
            id: id.to_string(),
            name: format!("{id} name"),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Default::default(),
            context_window: 200_000,
            max_tokens: 8_000,
            sampling_params: None,
            headers: None,
            compat: None,
        },
        thinking_level: None,
    }
}

#[test]
fn model_without_arguments_opens_the_two_column_picker() {
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode(&session);

    // The user replaced upstream's `ModelSelectorComponent` with the
    // `pi-model-picker` UX, so bare `/model` is now `/m`.
    let actions = mode.handle_submit("/model");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 80);
    assert!(body.contains("←→ category"), "{body:?}");
    assert!(body.contains("PROVIDERS"), "{body:?}");
    assert!(body.contains("Ctrl+S default"), "{body:?}");

    // Enter reports the highlighted model; the picker stays open until the
    // session reports back (upstream `selectModel`'s await).
    let actions = mode.handle_selector_key("\r").expect("selector");
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            // Models sort by name, so opus leads the provider category.
            id: "claude-opus-5".to_string(),
            persist: false,
        }]
    );
    assert!(mode.has_active_selector(), "open until the switch settles");

    // The host reports the settled switch back (upstream `done()`).
    let actions = mode.complete_model_selection("anthropic", "claude-opus-5", false, None);
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(!mode.has_active_selector());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Model: claude-opus-5"), "{body:?}");
}

#[test]
fn model_command_with_an_exact_reference_switches_without_a_selector() {
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode(&session);

    // An exact scoped reference switches directly (upstream `findExactModelMatch`).
    let actions = mode.handle_submit("/model anthropic/claude-opus-5");
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            id: "claude-opus-5".to_string(),
            persist: false,
        }]
    );
    assert!(!mode.has_active_selector());

    // A bare model id resolves too when it is unambiguous.
    let actions = mode.handle_submit("/model claude-sonnet-4-5");
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
            persist: false,
        }]
    );

    // Ctrl+S in the picker asks for the persisted default.
    mode.handle_submit("/model");
    let actions = mode.handle_selector_key("\u{13}").expect("selector"); // ctrl+s
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            id: "claude-opus-5".to_string(),
            persist: true,
        }]
    );
    let actions = mode.complete_model_selection(
        "anthropic",
        "claude-opus-5",
        true,
        Some("No API key for anthropic/claude-opus-5".to_string()),
    );
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    let body = plain(&mut mode.transcript().lock().chat, 120);
    assert!(
        body.contains("No API key for anthropic/claude-opus-5"),
        "{body:?}"
    );
}

#[test]
fn model_command_with_a_provider_argument_filters_the_picker() {
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);

    // Not a model reference: the picker opens with the provider filter applied.
    let actions = mode.handle_submit("/model anthropic");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 80);
    assert!(
        body.lines().any(|line| line.contains("› Anthropic")),
        "filtered to the provider: {body:?}"
    );

    // A filter that matches no provider reports it instead of opening.
    let actions = mode.handle_selector_key("\u{1b}").expect("selector");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    let actions = mode.handle_submit("/model zzz");
    assert!(actions.is_empty(), "{actions:?}");
    assert!(!mode.has_active_selector());
    let body = plain(&mut mode.transcript().lock().chat, 120);
    assert!(body.contains("No provider matching \"zzz\""), "{body:?}");
}

#[test]
fn model_select_app_action_opens_the_picker() {
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);

    // The `app.model.select` keybinding opens the picker (`/model`'s bare form).
    let actions = mode.handle_app_action("app.model.select");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 80);
    assert!(body.contains("PROVIDERS"), "{body:?}");
}

// --- 2-column model picker (the `pi-model-picker` extension UX, `/m`) ---------

/// `make_mode` with an agent directory (where the picker's recent-model
/// history lives) and a terminal height.
fn make_mode_with_agent_dir(
    session: &Arc<AgentSession>,
    agent_dir: std::path::PathBuf,
) -> InteractiveMode {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    InteractiveMode::new(
        Arc::clone(session),
        TranscriptSettings::default(),
        Vec::new(),
        InteractiveModeOptions {
            tui_mode: Some(TuiMode::Regular),
            clear_on_shrink: Some(false),
            show_terminal_progress: Some(false),
            version: Some("0.84.3".to_string()),
            on_terminal_title: Some(Arc::new(|_| {})),
            on_terminal_progress: None,
            cwd_git_paths: None,
            agent_dir: Some(agent_dir),
            terminal_rows: None,
        },
    )
}

/// A scoped entry for an arbitrary provider (the fixture's default is
/// `anthropic`).
fn scoped_model_with_provider(
    id: &str,
    provider: &str,
) -> pillar_coding_agent::core::model_mutation::ScopedModel {
    let mut scoped = scoped_model(id);
    scoped.model.provider = provider.to_string();
    scoped
}

/// Write the picker's history file the way the `pi-model-picker` extension
/// does (newest first).
fn seed_recent(dir: &std::path::Path, entries: &[(&str, &str)]) {
    let recent: Vec<serde_json::Value> = entries
        .iter()
        .map(|(provider, id)| serde_json::json!({ "provider": provider, "id": id }))
        .collect();
    let payload = serde_json::to_string_pretty(&serde_json::json!({ "recent": recent }))
        .expect("history json");
    std::fs::write(dir.join("model-picker-recent.json"), payload).expect("write history");
}

fn picker_agent_dir(label: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-picker-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The three scrollable rows of the picker (the up/down marker picks a model
/// per row).
fn picker_rows(mode: &InteractiveMode, width: usize) -> Vec<String> {
    editor_slot_body(mode, width)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn m_opens_the_two_column_picker_and_enter_reports_the_selection() {
    let dir = picker_agent_dir("open");
    // Newest first, like a pi install would leave it.
    seed_recent(
        &dir,
        &[
            ("anthropic", "claude-opus-5"),
            ("anthropic", "claude-sonnet-4-5"),
        ],
    );
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode_with_agent_dir(&session, dir);

    let actions = mode.handle_submit("/m");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());

    let body = editor_slot_body(&mode, 80);
    assert!(body.contains("←→ category"), "{body:?}");
    assert!(body.contains("PROVIDERS"), "{body:?}");
    assert!(body.contains("ctx 200,000"), "footer details: {body:?}");
    assert!(body.contains('✓'), "the current model is marked: {body:?}");

    // Newest first and RECENT selected: it is the first left-column row.
    let rows = picker_rows(&mode, 80);
    let recent_row = rows
        .iter()
        .find(|row| row.contains("RECENT"))
        .unwrap_or_else(|| panic!("recent row in {rows:?}"));
    assert!(
        recent_row.contains('›'),
        "recent is selected: {recent_row:?}"
    );
    assert!(recent_row.contains("claude-opus-5"), "{recent_row:?}");

    // ↓ moves within the recent category, Enter selects.
    assert_eq!(
        mode.handle_selector_key("\u{1b}[B").expect("selector"),
        Vec::new()
    );
    let actions = mode.handle_selector_key("\r").expect("selector");
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
            persist: false,
        }]
    );
}

#[test]
fn m_ctrl_s_selects_the_highlighted_model_as_the_default() {
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode_with_agent_dir(&session, picker_agent_dir("default"));

    mode.handle_submit("/m");
    // Provider category (models sorted by name: opus first), Ctrl+S asks for
    // the persisted default.
    assert_eq!(
        mode.handle_selector_key("\u{1b}[C").expect("selector"),
        Vec::new()
    );
    let actions = mode.handle_selector_key("\u{13}").expect("selector"); // ctrl+s
    assert_eq!(
        actions,
        vec![ModeAction::SelectModel {
            provider: "anthropic".to_string(),
            id: "claude-opus-5".to_string(),
            persist: true,
        }]
    );
    let actions = mode.complete_model_selection("anthropic", "claude-opus-5", true, None);
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    let body = plain(&mut mode.transcript().lock().chat, 120);
    assert!(
        body.contains("Default model: anthropic/claude-opus-5"),
        "{body:?}"
    );
}

#[test]
fn m_right_left_switch_categories_and_escape_cancels() {
    // Two providers: without history the picker opens on the first one and
    // →/← walk the categories.
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model_with_provider("gpt-5.6", "openai"),
    ]);
    let mode = make_mode_with_agent_dir(&session, picker_agent_dir("nav"));

    mode.handle_submit("/m");
    let body = editor_slot_body(&mode, 80);
    assert!(!body.contains("RECENT"), "no history yet: {body:?}");
    assert!(
        body.lines().any(|line| line.contains("› Anthropic")),
        "opens on the first provider category: {body:?}"
    );

    // → moves to the next provider category (both are registry providers).
    assert_eq!(
        mode.handle_selector_key("\u{1b}[C").expect("selector"),
        Vec::new()
    );
    let body = editor_slot_body(&mode, 80);
    assert!(
        body.lines().any(|line| line.contains("› OpenAI")),
        "second provider category selected: {body:?}"
    );

    // ← wraps back to the first category.
    assert_eq!(
        mode.handle_selector_key("\u{1b}[D").expect("selector"),
        Vec::new()
    );
    let body = editor_slot_body(&mode, 80);
    assert!(
        body.lines().any(|line| line.contains("› Anthropic")),
        "left wraps: {body:?}"
    );

    // Escape closes without switching.
    let actions = mode.handle_selector_key("\u{1b}").expect("selector");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(!mode.has_active_selector());
    assert_eq!(session.state().model.id, "claude-sonnet-4-5");
}

#[test]
fn m_filters_the_left_column_by_provider_and_reports_no_match() {
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("gpt-5.6-luna"),
    ]);
    let mode = make_mode_with_agent_dir(&session, picker_agent_dir("filter"));

    // A provider filter that matches nothing reports an error instead of
    // opening (upstream `No provider matching "…"`).
    let actions = mode.handle_submit("/m nope");
    assert!(actions.is_empty(), "{actions:?}");
    assert!(!mode.has_active_selector());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("No provider matching \"nope\""), "{body:?}");
}

#[test]
fn completing_a_picker_selection_records_the_recent_history() {
    let dir = picker_agent_dir("recent");
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode_with_agent_dir(&session, dir.clone());

    // Complete a switch through the shared path (what the pump does when the
    // executor reports back).
    let actions = mode.complete_model_selection("anthropic", "claude-opus-5", false, None);
    assert_eq!(actions, Vec::new(), "no selector was open");
    let history = std::fs::read_to_string(dir.join("model-picker-recent.json")).expect("history");
    assert!(history.contains("claude-opus-5"), "{history}");

    // `/m` now opens on RECENT with that model.
    mode.handle_submit("/m");
    let body = editor_slot_body(&mode, 80);
    assert!(body.contains("› RECENT"), "{body:?}");
    assert!(body.contains("anthropic/claude-opus-5"), "{body:?}");

    // A failed switch is not recorded.
    mode.handle_selector_key("\u{1b}").expect("selector");
    mode.complete_model_selection(
        "anthropic",
        "claude-haiku-4-5",
        false,
        Some("No API key for anthropic/claude-haiku-4-5".to_string()),
    );
    let history = std::fs::read_to_string(dir.join("model-picker-recent.json")).expect("history");
    assert!(!history.contains("claude-haiku-4-5"), "{history}");
}

#[test]
fn cycling_a_model_records_it_and_refreshes_the_footer() {
    let dir = picker_agent_dir("cycle");
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode_with_agent_dir(&session, dir.clone());

    mode.complete_model_cycle("anthropic", "claude-opus-5");
    let history = std::fs::read_to_string(dir.join("model-picker-recent.json")).expect("history");
    assert!(history.contains("claude-opus-5"), "{history}");
}

// --- autocomplete (host-side provider) ---------------------------------------

/// Type `text` into the editor the way the pump does: the editor events are
/// drained through the mode's handlers.
fn type_into_editor(mode: &InteractiveMode, text: &str) {
    for ch in text.chars() {
        mode.editor().lock().handle_input(&ch.to_string());
        drain(mode);
    }
}

fn drain(mode: &InteractiveMode) {
    // Mirrors the run loop's editor-event drain.
    let events = mode.editor().lock().take_input_events();
    for event in events {
        match event {
            pillar_tui::editor::EditorInputEvent::Changed => mode.on_editor_change(),
            pillar_tui::editor::EditorInputEvent::Submitted(_) => {}
        }
    }
}

#[test]
fn autocomplete_offers_commands_and_applies_them() {
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);
    mode.rebuild_autocomplete();

    // Typing a slash command opens the menu with the built-in commands.
    type_into_editor(&mode, "/thi");
    assert!(mode.autocomplete_is_open());
    assert_eq!(mode.autocomplete_prefix().as_deref(), Some("/thi"));
    assert_eq!(mode.autocomplete_items(), vec!["thinking".to_string()]);

    // Tab applies the highlighted completion.
    assert!(mode.autocomplete_tab().is_empty());
    assert_eq!(mode.editor_text(), "/thinking ");
    assert!(!mode.autocomplete_is_open(), "the menu closes");

    // Argument completions come from the host: /thinking levels.
    type_into_editor(&mode, "h");
    assert_eq!(mode.autocomplete_prefix().as_deref(), Some("h"));
    assert_eq!(mode.autocomplete_items(), vec!["high".to_string()]);
    // Enter applies an argument completion without submitting.
    assert!(mode.autocomplete_accept().is_empty());
    assert_eq!(mode.editor_text(), "/thinking high");
    assert!(!mode.has_active_selector(), "no submit happened");
}

#[test]
fn autocomplete_completes_model_references_and_picker_providers() {
    let session = session_with_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode(&session);
    mode.rebuild_autocomplete();

    // `/model <Tab>` completes provider/id references.
    type_into_editor(&mode, "/model op");
    assert_eq!(mode.autocomplete_prefix().as_deref(), Some("op"));
    assert_eq!(
        mode.autocomplete_items().first().map(String::as_str),
        Some("anthropic/claude-opus-5"),
        "the fuzzy best match leads: {:?}",
        mode.autocomplete_items()
    );
    assert!(mode.autocomplete_tab().is_empty());
    assert_eq!(mode.editor_text(), "/model anthropic/claude-opus-5");

    // `/m <Tab>` completes provider names (the picker's filter).
    mode.editor().lock().set_text("");
    drain(&mode);
    type_into_editor(&mode, "/m ant");
    assert_eq!(mode.autocomplete_prefix().as_deref(), Some("ant"));
    assert_eq!(mode.autocomplete_items(), vec!["anthropic".to_string()]);
}

#[test]
fn autocomplete_enter_on_a_command_name_submits_it() {
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);
    mode.rebuild_autocomplete();

    type_into_editor(&mode, "/thinking");
    assert!(mode.autocomplete_is_open());
    // Enter completes `/thinking ` and falls through to submit, which opens the
    // thinking selector (upstream's `/command` fall-through).
    let actions = mode.autocomplete_accept();
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    assert!(!mode.autocomplete_is_open());
}

#[test]
fn autocomplete_ignores_ordinary_text_and_the_bang_prefix() {
    let session = session_with_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);
    mode.rebuild_autocomplete();

    type_into_editor(&mode, "hello world");
    assert!(!mode.autocomplete_is_open(), "plain text does not trigger");

    mode.editor().lock().set_text("");
    drain(&mode);
    type_into_editor(&mode, "!echo hi");
    assert!(
        !mode.autocomplete_is_open(),
        "bash mode keeps the menu closed"
    );
}

// --- scoped-models selector (upstream `/scoped-models` +
// ScopedModelsSelectorComponent) -----------------------------------------------------

/// The component's footer and Ctrl+S matching read the process-global
/// keybindings (upstream `getKeybindings()` returns the merged app + TUI
/// table; the runtime installs it in `run_interactive`). The merged table is
/// a superset of the lazily-installed TUI table, so the other tests in this
/// binary are unaffected.
fn install_app_keybindings() {
    let definitions =
        pillar_coding_agent::core::keybindings::keybindings("darwin", &Default::default());
    pillar_tui::keybindings::set_keybindings(pillar_tui::keybindings::KeybindingsManager::new(
        definitions,
        Default::default(),
    ));
}

#[test]
fn scoped_models_command_shows_the_selector_and_toggles_the_scope() {
    install_app_keybindings();
    let session = session_with_available_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode(&session);

    let actions = mode.handle_submit("/scoped-models");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 90);
    assert!(body.contains("Model Configuration"), "{body:?}");
    assert!(body.contains("Session-only"), "{body:?}");
    assert!(body.contains("claude-sonnet-4-5"), "{body:?}");
    assert!(
        body.contains("2/14 enabled"),
        "the two scoped models are enabled over the full snapshot: {body:?}"
    );
    assert!(body.contains('✓'), "scoped entries start enabled: {body:?}");

    // Enter toggles the highlighted model off and applies the scope
    // synchronously (upstream `onChange` → `updateSessionModels`).
    assert_eq!(
        mode.handle_selector_key("\u{1b}[B").expect("selector"),
        Vec::new(),
        "down consumes the key"
    );
    assert_eq!(
        mode.handle_selector_key("\r").expect("selector"),
        Vec::new(),
        "toggling keeps the selector open"
    );
    assert_eq!(
        session
            .scoped_models()
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-sonnet-4-5"],
        "the cycle scope drops the toggled-off model (opus)"
    );
    let body = editor_slot_body(&mode, 90);
    assert!(body.contains("1/14 enabled"), "{body:?}");
    assert!(body.contains('✗'), "the disabled row is marked: {body:?}");

    // Escape closes and the scope stays.
    let actions = mode.handle_selector_key("\u{1b}").expect("selector");
    assert_eq!(actions, vec![ModeAction::EditorSlotChanged]);
    assert!(!mode.has_active_selector());
    assert_eq!(session.scoped_models().len(), 1);
}

#[test]
fn scoped_models_ctrl_s_persists_to_settings_and_reports_the_status() {
    install_app_keybindings();
    let session = session_with_available_scoped_models(vec![
        scoped_model("claude-sonnet-4-5"),
        scoped_model("claude-opus-5"),
    ]);
    let mode = make_mode(&session);
    mode.handle_submit("/scoped-models");

    // Toggle the highlighted model off, then persist the remaining set.
    mode.handle_selector_key("\u{1b}[B").expect("selector");
    mode.handle_selector_key("\r").expect("selector");
    assert_eq!(
        mode.handle_selector_key("\u{13}").expect("selector"), // ctrl+s
        Vec::new(),
        "Ctrl+S keeps the selector open"
    );
    {
        let settings = session.settings_manager().lock().expect("settings");
        assert_eq!(
            settings.enabled_models(),
            Some(vec!["anthropic/claude-sonnet-4-5".to_string()]),
            "the explicit list is persisted"
        );
    }
    let body = plain(&mut mode.transcript().lock().chat, 120);
    assert!(
        body.contains("Model selection saved to settings"),
        "{body:?}"
    );
    // The unsaved marker is gone after the persist.
    let body = editor_slot_body(&mode, 90);
    assert!(
        !body.contains("(unsaved)"),
        "persist clears the marker: {body:?}"
    );
}

#[test]
fn scoped_models_seeds_from_the_settings_patterns() {
    install_app_keybindings();
    // No session scope: the settings patterns resolve the initial selection
    // (upstream `configuredEnabledIds`).
    let session = session_with_available_scoped_models(Vec::new());
    session
        .settings_manager()
        .lock()
        .expect("settings")
        .set_enabled_models(Some(vec!["anthropic/claude-opus-5".to_string()]));
    let mode = make_mode(&session);

    mode.handle_submit("/scoped-models");
    let body = editor_slot_body(&mode, 90);
    assert!(body.contains("1/14 enabled"), "{body:?}");
    // opus first (enabled ids lead) and marked enabled; the disabled rows
    // follow.
    let opus_line = body
        .lines()
        .find(|line| line.contains("→ claude-opus-5"))
        .expect("selected opus row");
    assert!(opus_line.contains('✓'), "{opus_line:?}");
    assert!(
        body.lines().any(|line| line.contains('✗')),
        "disabled rows are marked: {body:?}"
    );
    assert!(
        !body.contains("(unsaved)"),
        "the seeded list is not dirty: {body:?}"
    );

    // An unknown pattern is kept in the list as `unavailable` (its own
    // session/settings pair).
    let session = session_with_available_scoped_models(Vec::new());
    session
        .settings_manager()
        .lock()
        .expect("settings")
        .set_enabled_models(Some(vec!["ghost/none".to_string()]));
    let mode = make_mode(&session);
    mode.handle_submit("/scoped-models");
    let body = editor_slot_body(&mode, 90);
    assert!(body.contains("[unavailable]"), "{body:?}");
    assert!(body.contains("1 unavailable"), "{body:?}");
}

// --- session selector (upstream `/resume` + SessionSelectorComponent) --------------

#[test]
fn resume_command_shows_the_selector_and_escape_closes_it() {
    install_app_keybindings();
    let session = session_with_available_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);

    let actions = mode.handle_submit("/resume");
    assert_eq!(
        actions,
        vec![ModeAction::EditorSlotChanged, ModeAction::LoadSessions { scope: pillar_coding_agent::modes::interactive::components::session_selector::SessionScope::Current }]
    );
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 120);
    assert!(body.contains("Resume Session (Current Folder)"), "{body:?}");
    assert!(
        body.contains("Loading"),
        "the constructor starts the current-folder load: {body:?}"
    );

    // The load lands (the mode routes the executor's report back).
    mode.session_list_loaded(
        pillar_coding_agent::modes::interactive::components::session_selector::SessionScope::Current,
        vec![SessionInfo {
            path: "/s/one.jsonl".to_string(),
            id: "one".to_string(),
            cwd: session.cwd().to_string(),
            name: None,
            parent_session_path: None,
            created_ms: Some(1000),
            modified_ms: 1000,
            message_count: 1,
            first_message: "first conversation".to_string(),
            all_messages_text: "first conversation".to_string(),
        }],
    );
    let body = editor_slot_body(&mode, 90);
    assert!(body.contains("first conversation"), "{body:?}");

    // Enter reports the resume for the highlighted session.
    let actions = mode.handle_selector_key("\r").expect("selector active");
    assert_eq!(
        actions,
        vec![
            ModeAction::EditorSlotChanged,
            ModeAction::ResumeSession {
                session_path: "/s/one.jsonl".to_string(),
            }
        ]
    );
    assert!(!mode.has_active_selector());
}

#[test]
fn resume_selector_delete_and_rename_route_through_the_executor() {
    install_app_keybindings();
    use pillar_coding_agent::modes::interactive::components::session_selector::SessionScope;
    let session = session_with_available_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);
    mode.handle_submit("/resume");
    mode.session_list_loaded(
        SessionScope::Current,
        vec![SessionInfo {
            path: "/s/one.jsonl".to_string(),
            id: "one".to_string(),
            cwd: "/tmp".to_string(),
            name: Some("old".to_string()),
            parent_session_path: None,
            created_ms: Some(1000),
            modified_ms: 1000,
            message_count: 1,
            first_message: "first".to_string(),
            all_messages_text: "first".to_string(),
        }],
    );

    // Ctrl+D → delete confirmation → Enter → DeleteSession action.
    assert_eq!(
        mode.handle_selector_key("\u{4}").expect("selector"),
        Vec::new()
    );
    let actions = mode.handle_selector_key("\r").expect("selector");
    assert_eq!(
        actions,
        vec![ModeAction::DeleteSession {
            path: "/s/one.jsonl".to_string(),
        }]
    );

    // The delete settles: the list reloads (upstream
    // `refreshSessionsAfterMutation`).
    let actions = mode.complete_session_delete("/s/one.jsonl", true, true, None);
    assert_eq!(
        actions,
        vec![ModeAction::LoadSessions {
            scope: SessionScope::Current
        }]
    );
    let body = editor_slot_body(&mode, 120);
    assert!(body.contains("Session moved to trash"), "{body:?}");

    // The reload lands (the deleted file is gone from the disk list).
    mode.session_list_loaded(
        SessionScope::Current,
        vec![SessionInfo {
            path: "/s/two.jsonl".to_string(),
            id: "two".to_string(),
            cwd: "/tmp".to_string(),
            name: Some("old".to_string()),
            parent_session_path: None,
            created_ms: Some(2000),
            modified_ms: 2000,
            message_count: 1,
            first_message: "second".to_string(),
            all_messages_text: "second".to_string(),
        }],
    );

    // Rename: Ctrl+R → submit → RenameSession → refresh on completion.
    let actions = mode.handle_selector_key("\u{12}").expect("selector");
    assert_eq!(actions, Vec::new());
    let actions = mode.handle_selector_key("\r").expect("selector");
    match actions.as_slice() {
        [ModeAction::RenameSession { path, name }] => {
            assert_eq!(path, "/s/two.jsonl");
            assert!(name.contains("old"), "{name:?}");
        }
        other => panic!("expected RenameSession, got {other:?}"),
    }
    let actions = mode.complete_session_rename(None);
    assert_eq!(
        actions,
        vec![ModeAction::LoadSessions {
            scope: SessionScope::Current
        }]
    );
}

#[test]
fn resume_selector_load_progress_shows_the_counts() {
    install_app_keybindings();
    use pillar_coding_agent::modes::interactive::components::session_selector::SessionScope;
    let session = session_with_available_scoped_models(vec![scoped_model("claude-sonnet-4-5")]);
    let mode = make_mode(&session);
    mode.handle_submit("/resume");

    mode.session_load_progress(SessionScope::Current, 3, 10);
    let body = editor_slot_body(&mode, 120);
    assert!(body.contains("Loading 3/10"), "{body:?}");
    // A stale scope's progress does not leak into the display.
    mode.session_load_progress(SessionScope::All, 1, 2);
    let body = editor_slot_body(&mode, 120);
    assert!(body.contains("Current Folder"), "{body:?}");
    assert!(!body.contains("Loading 1/2"), "{body:?}");
}

// --- session tree (/tree) -----------------------------------------------------------------

/// q1 / a1 / q2 / a2; returns the entry ids.
fn seed_linear_tree(session: &Arc<AgentSession>) -> Vec<String> {
    let mut sm = session.session_manager().lock().expect("lock");
    let mut ids = Vec::new();
    for (text, is_user) in [("q1", true), ("a1", false), ("q2", true), ("a2", false)] {
        let message = if is_user {
            CodingAgentMessage::Base(Message::User {
                content: UserContent::Text(text.to_string()),
                timestamp: 1,
            })
        } else {
            assistant_message(text, StopReason::Stop)
        };
        ids.push(sm.append_message(message).expect("append"));
    }
    ids
}

/// Up-arrow three times: the constructor selects the current leaf (a2, last),
/// so this lands on q1 (the first entry).
fn select_first_tree_entry(mode: &InteractiveMode) {
    for _ in 0..3 {
        assert_eq!(
            mode.handle_selector_key("\u{1b}[A").expect("tree selector"),
            Vec::new()
        );
    }
}

#[test]
fn tree_command_shows_the_selector_and_enter_asks_about_summarization() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    assert_eq!(
        mode.handle_submit("/tree"),
        vec![ModeAction::EditorSlotChanged]
    );
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Session Tree"), "{body:?}");
    assert!(body.contains("q1"), "{body:?}");

    select_first_tree_entry(&mode);
    // Enter closes the tree and opens the summary dialog.
    assert_eq!(
        mode.handle_selector_key("\r").expect("tree"),
        vec![ModeAction::EditorSlotChanged, ModeAction::EditorSlotChanged]
    );
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Summarize branch?"), "{body:?}");
    assert!(body.contains("No summary"), "{body:?}");

    // The default option is "No summary".
    assert_eq!(
        mode.handle_selector_key("\r").expect("dialog"),
        vec![
            ModeAction::EditorSlotChanged,
            ModeAction::NavigateTree {
                target_id: ids[0].clone(),
                summarize: false,
                custom_instructions: None,
            }
        ]
    );
    assert!(!mode.has_active_selector());
}

#[test]
fn tree_summarize_with_custom_prompt_collects_instructions() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    mode.handle_submit("/tree");
    select_first_tree_entry(&mode);
    mode.handle_selector_key("\r").expect("tree");

    // Down twice: "Summarize with custom prompt".
    mode.handle_selector_key("\u{1b}[B").expect("dialog");
    mode.handle_selector_key("\u{1b}[B").expect("dialog");
    assert_eq!(
        mode.handle_selector_key("\r").expect("dialog"),
        vec![ModeAction::EditorSlotChanged, ModeAction::EditorSlotChanged]
    );
    let body = editor_slot_body(&mode, 100);
    assert!(
        body.contains("Custom summarization instructions"),
        "{body:?}"
    );

    for ch in "focus on tests".chars() {
        mode.handle_selector_key(&ch.to_string()).expect("input");
    }
    assert_eq!(
        mode.handle_selector_key("\r").expect("input"),
        vec![
            ModeAction::EditorSlotChanged,
            ModeAction::NavigateTree {
                target_id: ids[0].clone(),
                summarize: true,
                custom_instructions: Some("focus on tests".to_string()),
            }
        ]
    );
}

#[test]
fn tree_escape_from_the_choice_reopens_the_tree() {
    install_app_keybindings();
    let session = session();
    seed_linear_tree(&session);
    let mode = make_mode(&session);

    mode.handle_submit("/tree");
    select_first_tree_entry(&mode);
    mode.handle_selector_key("\r").expect("tree");

    // Cancelling the summary choice closes the dialog and re-opens the tree
    // (upstream `showTreeSelector(entryId)`).
    assert_eq!(
        mode.handle_selector_key("\u{1b}").expect("dialog"),
        vec![ModeAction::EditorSlotChanged, ModeAction::EditorSlotChanged]
    );
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Session Tree"), "{body:?}");
}

#[test]
fn tree_cancelling_custom_instructions_loops_back_to_the_choice() {
    install_app_keybindings();
    let session = session();
    seed_linear_tree(&session);
    let mode = make_mode(&session);

    mode.handle_submit("/tree");
    select_first_tree_entry(&mode);
    mode.handle_selector_key("\r").expect("tree");
    mode.handle_selector_key("\u{1b}[B").expect("dialog");
    mode.handle_selector_key("\u{1b}[B").expect("dialog");
    mode.handle_selector_key("\r").expect("dialog");

    // Escape on the input loops back to the summary choice.
    assert_eq!(
        mode.handle_selector_key("\u{1b}").expect("input"),
        vec![ModeAction::EditorSlotChanged, ModeAction::EditorSlotChanged]
    );
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Summarize branch?"), "{body:?}");
}

#[test]
fn tree_skips_the_summary_prompt_when_configured() {
    install_app_keybindings();
    let session = session();
    session
        .settings_manager()
        .lock()
        .expect("settings")
        .apply_overrides(&serde_json::json!({ "branchSummary": { "skipPrompt": true } }));
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    mode.handle_submit("/tree");
    select_first_tree_entry(&mode);
    assert_eq!(
        mode.handle_selector_key("\r").expect("tree"),
        vec![
            ModeAction::EditorSlotChanged,
            ModeAction::NavigateTree {
                target_id: ids[0].clone(),
                summarize: false,
                custom_instructions: None,
            }
        ]
    );
    assert!(!mode.has_active_selector());
}

#[test]
fn tree_selecting_the_current_leaf_reports_already_at_this_point() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    mode.handle_submit("/tree");
    // The constructor selects the current leaf (a2), so Enter is a no-op.
    assert_eq!(
        mode.handle_selector_key("\r").expect("tree"),
        vec![ModeAction::EditorSlotChanged]
    );
    assert!(!mode.has_active_selector());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Already at this point"), "{body:?}");
    assert_eq!(session.get_leaf_id().as_deref(), Some(ids[3].as_str()));
}

#[test]
fn complete_tree_navigation_rebuilds_the_transcript_and_sets_the_editor() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);
    mode.render_initial_messages();

    // Run the actual navigation (the executor does this in the real loop),
    // then report it like `UiCommand::TreeNavigated`.
    let result = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(session.navigate_tree(
            &ids[0],
            pillar_coding_agent::core::agent_session_class::TreeNavigationOptions::default(),
        ))
        .expect("navigate");
    let actions = mode.complete_tree_navigation(
        &ids[0],
        result.editor_text,
        result.cancelled,
        result.aborted,
        None,
    );
    assert_eq!(actions, Vec::new());

    // The abandoned branch is gone from the transcript.
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(!body.contains("a2"), "{body:?}");
    // The user message text went to the editor.
    assert_eq!(mode.editor().lock().get_text(), "q1");
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Navigated to selected point"), "{body:?}");
}

// --- /fork and /clone ---------------------------------------------------------------------

#[test]
fn fork_command_shows_the_user_message_selector_and_reports_the_fork() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    assert_eq!(
        mode.handle_submit("/fork"),
        vec![ModeAction::EditorSlotChanged]
    );
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Fork from Message"), "{body:?}");
    // The most recent user message (q2) is selected by default; up goes to q1.
    assert!(body.contains("q2"), "{body:?}");
    mode.handle_selector_key("\u{1b}[A").expect("selector");
    assert_eq!(
        mode.handle_selector_key("\r").expect("selector"),
        vec![
            ModeAction::EditorSlotChanged,
            ModeAction::ForkSession {
                entry_id: ids[0].clone(),
                position: "before".to_string(),
                editor_text: Some("q1".to_string()),
            }
        ]
    );
    assert!(!mode.has_active_selector());
}

#[test]
fn clone_command_forks_at_the_leaf() {
    install_app_keybindings();
    let session = session();
    let ids = seed_linear_tree(&session);
    let mode = make_mode(&session);

    assert_eq!(
        mode.handle_submit("/clone"),
        vec![ModeAction::ForkSession {
            entry_id: ids[3].clone(),
            position: "at".to_string(),
            editor_text: None,
        }]
    );
}

#[test]
fn fork_reports_when_there_is_nothing_to_fork_from() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(mode.handle_submit("/fork"), Vec::new());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("No messages to fork from"), "{body:?}");
    let body = editor_slot_body(&mode, 100);
    assert!(!body.contains("Fork from Message"), "{body:?}");

    // `/clone` with no leaf reports the other status.
    assert_eq!(mode.handle_submit("/clone"), Vec::new());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Nothing to clone yet"), "{body:?}");
}

#[test]
fn fork_app_action_opens_the_selector() {
    install_app_keybindings();
    let session = session();
    seed_linear_tree(&session);
    let mode = make_mode(&session);

    assert_eq!(
        mode.handle_app_action("app.session.fork"),
        vec![ModeAction::EditorSlotChanged]
    );
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Fork from Message"), "{body:?}");
}

// --- /hotkeys -----------------------------------------------------------------------------

#[test]
fn hotkeys_command_renders_the_keybinding_table() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(mode.handle_submit("/hotkeys"), Vec::new());
    let body = plain(&mut mode.transcript().lock().chat, 100);
    assert!(body.contains("Keyboard Shortcuts"), "{body:?}");
    for expected in [
        "Navigation",
        "Editing",
        "Other",
        "Send message",
        "Open model selector",
        "Run bash command (excluded from context)",
    ] {
        assert!(body.contains(expected), "{expected:?} not in {body:?}");
    }
    // The resolved keys are rendered (capitalized display form).
    assert!(body.contains("Ctrl+L"), "{body:?}");
    assert!(body.contains("Enter"), "{body:?}");
}

// --- /settings ----------------------------------------------------------------------------

/// Type a query into the open settings panel's search box.
fn search_settings(mode: &InteractiveMode, query: &str) {
    for ch in query.chars() {
        assert_eq!(
            mode.handle_selector_key(&ch.to_string()).expect("settings"),
            Vec::new()
        );
    }
}

#[test]
fn settings_command_shows_the_panel_and_applies_a_cycled_value() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(
        mode.handle_submit("/settings"),
        vec![ModeAction::EditorSlotChanged]
    );
    assert!(mode.has_active_selector());
    let body = editor_slot_body(&mode, 100);
    assert!(body.contains("Auto-compact"), "{body:?}");
    assert!(body.contains("Type to search"), "{body:?}");

    // Filter to the Transport row and cycle it. The fixture's transport is
    // the "auto" default, so the cycle wraps to the first value.
    search_settings(&mode, "transport");
    assert_eq!(mode.handle_selector_key("\r").expect("settings"), Vec::new());
    assert_eq!(
        session.settings_manager().lock().expect("settings").transport(),
        "sse"
    );

    // Escape closes the panel.
    assert_eq!(
        mode.handle_selector_key("\u{1b}").expect("settings"),
        vec![ModeAction::EditorSlotChanged]
    );
    assert!(!mode.has_active_selector());
}

#[test]
fn settings_theme_change_applies_and_previews() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(
        mode.apply_setting_change("theme", "light"),
        vec![ModeAction::ThemeApplied("light".to_string())]
    );
    assert_eq!(
        session
            .settings_manager()
            .lock()
            .expect("settings")
            .theme_setting()
            .as_deref(),
        Some("light")
    );

    // Opening the theme submenu and choosing Automatic previews the automatic
    // setting (the port does not live-preview on highlight). The current theme
    // ("light") is pre-selected, so walk up past "dark" to Automatic.
    mode.handle_submit("/settings");
    search_settings(&mode, "theme");
    mode.handle_selector_key("\r").expect("settings");
    mode.handle_selector_key("\u{1b}[A").expect("settings");
    mode.handle_selector_key("\u{1b}[A").expect("settings");
    let actions = mode.handle_selector_key("\r").expect("settings");
    match actions.as_slice() {
        [ModeAction::ThemePreview(setting)] => {
            assert!(setting.contains('/'), "{setting:?}");
        }
        other => panic!("expected ThemePreview, got {other:?}"),
    }
}

#[test]
fn settings_screen_options_route_through_the_pump() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(
        mode.apply_setting_change("show-hardware-cursor", "true"),
        vec![ModeAction::SetShowHardwareCursor(true)]
    );
    assert_eq!(
        mode.apply_setting_change("clear-on-shrink", "true"),
        vec![ModeAction::SetClearOnShrink(true)]
    );
    let settings = session.settings_manager().lock().expect("settings");
    assert!(settings.show_hardware_cursor());
    assert!(settings.clear_on_shrink());
}

#[test]
fn settings_tui_mode_switch_is_reported_not_applied() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    // The fullscreen mode is not ported: the row is reverted with a status.
    assert_eq!(mode.apply_setting_change("tui-mode", "fullscreen"), Vec::new());
    let body = plain(&mut mode.transcript().lock().chat, 120);
    assert!(body.contains("TUI mode switching is not ported yet"), "{body:?}");
}

#[test]
fn settings_editor_and_output_padding_are_applied() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    assert_eq!(mode.apply_setting_change("editor-padding", "3"), Vec::new());
    assert_eq!(mode.editor().lock().get_padding_x(), 3);
    assert_eq!(
        mode.apply_setting_change("output-padding", "0"),
        Vec::new()
    );
    assert_eq!(mode.transcript().lock().settings_mut().output_pad, 0);
}

#[test]
fn settings_model_thinking_level_applies_to_the_current_model() {
    install_app_keybindings();
    let session = session();
    let mode = make_mode(&session);

    // The fixture's current model is anthropic/claude-sonnet-4-5.
    mode.apply_model_thinking_level("anthropic", "claude-sonnet-4-5", Some("low"));
    let settings = session.settings_manager().lock().expect("settings");
    assert_eq!(
        settings.model_thinking_level("anthropic", "claude-sonnet-4-5"),
        Some("low".to_string())
    );
    drop(settings);
    assert_eq!(session.thinking_level(), "low");

    // Removing it reverts to the global default ("off" in the fixture).
    mode.apply_model_thinking_level("anthropic", "claude-sonnet-4-5", None);
    let settings = session.settings_manager().lock().expect("settings");
    assert!(settings.all_model_thinking_levels().is_empty());
}
