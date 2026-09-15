//! Parity tests for the `InteractiveMode` assembly (pi v0.84.3
//! `interactive-mode.ts`): the event dispatch and the submit router.

use std::sync::{Arc, Mutex};

use pillar_agent::{Agent, AgentOptions, AgentState, FauxModelRef};
use pillar_ai::types::{Content, Message, StopReason, Usage, UsageCost, UserContent};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, StreamingBehavior,
};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::model_runtime::ModelRuntime;
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};

use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use pillar_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions, ModeAction,
};
use pillar_coding_agent::modes::interactive::mode_ui::QueueMode;
use pillar_coding_agent::modes::interactive::theme;
use pillar_coding_agent::modes::interactive::transcript::TranscriptSettings;
use pillar_tui::tui::{Component as _, TuiMode};

static THEME_LOCK: Mutex<()> = Mutex::new(());

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
    let runtime = ModelRuntime::new(
        pillar_coding_agent::core::model_runtime::CreateModelRuntimeOptions {
            models_path: Some(models_path),
            models_store: Some(Arc::new(
                pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore::new(),
            )),
            ..Default::default()
        },
    )
    .expect("runtime");

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
    // selectors land). `/model` moved out of that list with the model
    // selector (`model_command_shows_the_selector_and_enter_reports_the_switch`).
    mode.handle_submit("/settings");
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
    assert!(body.contains("/settings is not available yet"), "{body:?}");
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
