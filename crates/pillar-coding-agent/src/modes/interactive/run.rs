//! Host-driven interactive run loop (upstream `interactive-mode.ts`
//! `init` / `run` plus the pieces the port deliberately keeps host-side).
//!
//! The loop has two halves:
//!
//! - the **pump**: a dedicated thread that owns the [`TuiMainScreen`], reads
//!   the terminal, dispatches keys through the app keybindings and the focused
//!   editor (upstream `CustomEditor.handleInput`), drives the
//!   [`InteractiveMode`] event handlers, and renders frames;
//! - the **executor**: the async task that runs the [`ModeAction`]s the mode
//!   reported (`session.prompt` / `steer` / `followUp` / `executeBash` /
//!   `compact` / abort / model cycling / shutdown).
//!
//! Everything that mutates the mode (event handling, key handling, submission)
//! happens on the pump thread, so the mode keeps upstream's single-threaded
//! assumptions; the async half only touches the session. The pump reports a
//! render request through [`InteractiveMode::mark_dirty`] instead of calling
//! `ui.requestRender()` from the event handlers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::editor::EditorInputEvent;
use pillar_tui::process_terminal::{ProcessTerminal, Terminal};
use pillar_tui::tui::TuiStopOptions;
use pillar_tui::tui_main_screen::TuiMainScreen;

use crate::core::agent_session_class::{AgentSession, AgentSessionEvent, PromptOptions};
use crate::core::extensions_types::MarkdownTransformer;
use crate::core::keybindings::KeybindingsManager;
use crate::core::model_mutation::CycleDirection;
use crate::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions, ModeAction,
};
use crate::modes::interactive::theme;
use crate::modes::interactive::transcript::TranscriptSettings;

/// How often the pump drains the terminal and re-renders (upstream relies on
/// the event loop; the port polls).
pub const PUMP_INTERVAL_MS: u64 = 8;

/// Compartments the executor reports back to the pump; every mode mutation
/// stays on the pump thread.
#[derive(Debug)]
enum UiCommand {
    /// A bash command finished (upstream the code after `await
    /// session.executeBash(...)`).
    BashComplete {
        exit_code: Option<i32>,
        cancelled: bool,
        truncation: Option<crate::core::truncate::TruncationResult>,
        full_output_path: Option<String>,
    },
}

/// Options for [`run_interactive`].
pub struct InteractiveRunOptions {
    pub mode: InteractiveModeOptions,
    pub transcript: TranscriptSettings,
    pub markdown_transformers: Vec<MarkdownTransformer>,
    /// Sent through `session.prompt` before the loop starts (upstream
    /// `options.initialMessage`).
    pub initial_message: Option<String>,
    /// Where `<agentDir>/keybindings.json` lives.
    pub agent_dir: PathBuf,
}

impl Default for InteractiveRunOptions {
    fn default() -> Self {
        Self {
            mode: InteractiveModeOptions::default(),
            transcript: TranscriptSettings::default(),
            markdown_transformers: Vec::new(),
            initial_message: None,
            agent_dir: PathBuf::new(),
        }
    }
}

/// Run the interactive mode until shutdown. Returns the process exit code.
pub async fn run_interactive(
    session: Arc<AgentSession>,
    terminal: Box<dyn Terminal>,
    options: InteractiveRunOptions,
) -> Result<i32, String> {
    let InteractiveRunOptions {
        mode: mut mode_options,
        transcript,
        markdown_transformers,
        initial_message,
        agent_dir,
    } = options;

    // Upstream `setKeybindings(this.keybindings)`: the merged app + TUI table
    // is what the editor's `tui.*` bindings and the host's `app.*` matches use.
    let keybindings = Arc::new(KeybindingsManager::create(&agent_dir));
    pillar_tui::keybindings::set_keybindings(keybindings.tui_manager());

    // Upstream's module-level default theme plus the settings-selected name;
    // the full controller (`applyFromSettings` with `auto` detection and
    // resource themes) is the next theme slice.
    let theme_name = session
        .settings_manager()
        .lock()
        .expect("settings lock")
        .theme();
    theme::init_theme(theme_name.as_deref());

    // The terminal lives on the pump thread, so the mode's terminal callbacks
    // hand the values to the pump through shared cells (upstream calls
    // `ui.terminal.setTitle` directly).
    let pending_title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pending_progress: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    if mode_options.on_terminal_title.is_none() {
        let cell = Arc::clone(&pending_title);
        mode_options.on_terminal_title = Some(Arc::new(move |title: &str| {
            *cell.lock().expect("title cell") = Some(title.to_string());
        }));
    }
    if mode_options.on_terminal_progress.is_none() {
        let cell = Arc::clone(&pending_progress);
        mode_options.on_terminal_progress = Some(Arc::new(move |running: bool| {
            *cell.lock().expect("progress cell") = Some(running);
        }));
    }

    let clear_on_shrink = mode_options.clear_on_shrink.unwrap_or(false);
    let mode = Arc::new(InteractiveMode::new(
        Arc::clone(&session),
        transcript,
        markdown_transformers,
        mode_options,
    ));
    mode.render_initial_messages();
    mode.update_terminal_title();
    mode.update_editor_border_color();

    let mut screen = TuiMainScreen::new(terminal);
    screen.base_mut().set_clear_on_shrink(clear_on_shrink);
    let editor_id = mode.mount(screen.base_mut());
    screen.base_mut().set_focus(Some(editor_id));
    screen.base_mut().start();

    // Session events are queued for the pump, which owns all mode mutation.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSessionEvent>();
    let unsubscribe = session.subscribe(Arc::new(move |event| {
        let _ = event_tx.send(event.clone());
    }));

    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<ModeAction>();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let pump_shutdown = Arc::new(AtomicBool::new(false));

    if let Some(initial_message) = initial_message {
        let _ = action_tx.send(ModeAction::Prompt {
            text: initial_message,
            streaming_behavior: None,
        });
    }

    let pump_mode = Arc::clone(&mode);
    let pump_keybindings = Arc::clone(&keybindings);
    let pump_stop = Arc::clone(&pump_shutdown);
    let pump_actions = action_tx.clone();
    let pump_exit = shutdown_tx.clone();
    let pump_title = Arc::clone(&pending_title);
    let pump_progress = Arc::clone(&pending_progress);
    let pump = std::thread::spawn(move || {
        let result = pump_loop(
            screen,
            pump_mode,
            pump_keybindings,
            event_rx,
            pump_actions,
            ui_rx,
            pump_stop,
            pump_title,
            pump_progress,
        );
        // Wake the executor so it stops waiting for actions.
        let _ = pump_exit.send(true);
        result
    });

    // The executor owns the async side: one action at a time, abortable by a
    // shutdown request.
    loop {
        let action = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => break,
            action = action_rx.recv() => match action {
                Some(action) => action,
                None => break,
            },
        };
        if matches!(action, ModeAction::Shutdown) {
            break;
        }
        let result = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => break,
            result = execute_action(&session, &ui_tx, action) => result,
        };
        if let Err(error) = result {
            mode.transcript().lock().show_error(&error);
            mode.mark_dirty();
        }
    }

    pump_shutdown.store(true, Ordering::SeqCst);
    let _ = shutdown_tx.send(true);
    let pump_result = pump.join();
    unsubscribe();
    session.dispose();
    match pump_result {
        Ok(Ok(())) => Ok(0),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("the interactive render loop panicked".to_string()),
    }
}

/// [`run_interactive`] over a fresh [`ProcessTerminal`] (the `pillar` binary's
/// path; tests inject their own terminal).
pub async fn run_interactive_process(
    session: Arc<AgentSession>,
    options: InteractiveRunOptions,
) -> Result<i32, String> {
    run_interactive(session, Box::new(ProcessTerminal::new()), options).await
}

/// Run one [`ModeAction`] (upstream the awaits inside `handleEvent` and the
/// submit handler).
async fn execute_action(
    session: &AgentSession,
    ui: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    action: ModeAction,
) -> Result<(), String> {
    match action {
        ModeAction::Prompt {
            text,
            streaming_behavior,
        } => {
            session
                .prompt(
                    &text,
                    Some(&PromptOptions {
                        streaming_behavior,
                        ..Default::default()
                    }),
                )
                .await
        }
        ModeAction::Steer(text) => session.steer(&text, None).await,
        ModeAction::FollowUp(text) => session.follow_up(&text, None).await,
        ModeAction::Bash { command, excluded } => {
            // The block itself is created by the pump (`begin_bash`) before
            // this runs; streamed output arrives as `bash_execution_update`
            // events and the completion is reported back to the pump.
            let result = session.execute_bash(&command, excluded, None).await;
            let completion = match &result {
                Ok(result) => UiCommand::BashComplete {
                    exit_code: result.exit_code,
                    cancelled: result.cancelled,
                    truncation: result.truncation.clone(),
                    full_output_path: result
                        .full_output_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().to_string()),
                },
                // Upstream `setComplete(undefined, false)` in the catch block.
                Err(_) => UiCommand::BashComplete {
                    exit_code: None,
                    cancelled: false,
                    truncation: None,
                    full_output_path: None,
                },
            };
            let _ = ui.send(completion);
            result.map(|_| ())
        }
        ModeAction::Compact { instructions } => {
            session.compact(instructions.as_deref()).await.map(|_| ())
        }
        ModeAction::SubmitToLoop(text) => session.prompt(&text, None).await,
        ModeAction::Abort => {
            session.abort().await;
            Ok(())
        }
        ModeAction::CycleModel { forward } => {
            let direction = if forward {
                CycleDirection::Forward
            } else {
                CycleDirection::Backward
            };
            let outcome = session.cycle_model(direction).await?;
            if outcome.is_none() {
                let scoped = !session.scoped_models().is_empty();
                let message = if scoped {
                    "Only one model in scope"
                } else {
                    "Only one model available"
                };
                return Err(message.to_string());
            }
            Ok(())
        }
        ModeAction::Shutdown => Ok(()),
    }
}

/// The app keybindings in the precedence upstream's `CustomEditor` checks
/// them (declaration order of the `onAction` registrations).
const APP_ACTION_PRECEDENCE: [&str; 17] = [
    "app.clear",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "app.message.dequeue",
    "app.session.new",
    "app.session.tree",
    "app.session.fork",
    "app.session.resume",
    "app.session.toggleNamedFilter",
];

/// The pump: terminal input, session events, and rendering, all on one thread.
#[allow(clippy::too_many_arguments)]
fn pump_loop(
    mut screen: TuiMainScreen,
    mode: Arc<InteractiveMode>,
    keybindings: Arc<KeybindingsManager>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<AgentSessionEvent>,
    actions: tokio::sync::mpsc::UnboundedSender<ModeAction>,
    mut ui_commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    shutdown: Arc<AtomicBool>,
    pending_title: Arc<Mutex<Option<String>>>,
    pending_progress: Arc<Mutex<Option<bool>>>,
) -> Result<(), String> {
    let interval = Duration::from_millis(PUMP_INTERVAL_MS);
    let result = loop {
        if shutdown.load(Ordering::SeqCst) {
            break Ok(());
        }

        // Terminal side effects the mode reported (upstream calls the terminal
        // from `setTitle` / `setProgress`).
        if let Some(title) = pending_title.lock().expect("title cell").take() {
            screen.base_mut().terminal_mut().set_title(&title);
        }
        if let Some(running) = pending_progress.lock().expect("progress cell").take() {
            screen.base_mut().terminal_mut().set_progress(running);
        }

        // Session events (the listener cloned them onto the channel).
        while let Ok(event) = events.try_recv() {
            let reported = mode.handle_event(&event);
            mode.mark_dirty();
            for action in reported {
                if actions.send(action).is_err() {
                    break;
                }
            }
        }

        // Executor reports (bash completion).
        while let Ok(command) = ui_commands.try_recv() {
            match command {
                UiCommand::BashComplete {
                    exit_code,
                    cancelled,
                    truncation,
                    full_output_path,
                } => mode.complete_bash(exit_code, cancelled, truncation, full_output_path),
            }
        }

        // Host-driven animations (retry countdown, status expiry).
        mode.tick();

        // Terminal input.
        let mut requested_shutdown = false;
        let data = screen.base_mut().terminal_mut().read_input(interval);
        if let Some(data) = data {
            for action in dispatch_input(&mut screen, &mode, &keybindings, &data) {
                if matches!(action, ModeAction::Shutdown) {
                    requested_shutdown = true;
                }
                if let ModeAction::Bash { command, excluded } = &action {
                    // Upstream creates the block inside `handleBashCommand`
                    // before executing; the executor only runs the command.
                    mode.begin_bash(command, *excluded);
                }
                if actions.send(action).is_err() {
                    break;
                }
            }
        }

        // Rendering (a shutdown request still paints the final frame).
        if mode.take_dirty() {
            screen.base_mut().invalidate();
            screen.base_mut().request_render(false);
        }
        if screen.base_mut().terminal_mut().resize_if_changed() {
            screen.base_mut().invalidate();
        }
        if screen.base_mut().take_render_request(Instant::now()) {
            if let Err(error) = screen.do_render() {
                break Err(error);
            }
        }
        if requested_shutdown {
            break Ok(());
        }
    };

    screen.stop(TuiStopOptions {
        preserve_screen: false,
    });
    result
}

/// Feed raw terminal bytes through the ported input pipeline and dispatch the
/// resulting sequences.
fn dispatch_input(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    keybindings: &KeybindingsManager,
    data: &str,
) -> Vec<ModeAction> {
    let now = Instant::now();
    let sequences = {
        let terminal = screen.base_mut().terminal_mut();
        terminal.feed_input_bytes(data, now)
    };
    let mut forwarded = Vec::new();
    {
        let terminal = screen.base_mut().terminal_mut();
        for sequence in sequences {
            if let Some(sequence) = terminal.handle_sequence(&sequence) {
                forwarded.push(terminal.normalize_input(&sequence));
            }
        }
    }

    let mut actions = Vec::new();
    for sequence in forwarded {
        actions.extend(dispatch_sequence(screen, mode, keybindings, &sequence));
    }
    actions
}

/// Upstream `CustomEditor.handleInput`: the app keybindings win over the
/// editor, and the rest goes through the TUI's focused-component dispatch.
fn dispatch_sequence(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    keybindings: &KeybindingsManager,
    data: &str,
) -> Vec<ModeAction> {
    // Upstream checks extension shortcuts and the clipboard-paste binding
    // first; neither is ported (both answer a warning).
    if keybindings.matches(data, "app.clipboard.pasteImage") {
        return run_app_action(mode, "app.clipboard.pasteImage");
    }
    // Escape / interrupt - only when the autocomplete menu is closed.
    if keybindings.matches(data, "app.interrupt") && !mode.editor().lock().is_showing_autocomplete()
    {
        return run_app_action(mode, "app.interrupt");
    }
    // Exit (Ctrl+D) - only when the editor is empty; otherwise it falls
    // through to the editor's delete-char-forward.
    if keybindings.matches(data, "app.exit") && mode.editor_is_empty() {
        return run_app_action(mode, "app.exit");
    }
    // Explicit history bindings take precedence over other app actions.
    if keybindings.matches(data, "tui.editor.historyPrevious")
        || keybindings.matches(data, "tui.editor.historyNext")
    {
        return dispatch_to_editor(screen, mode, data);
    }
    for action in APP_ACTION_PRECEDENCE {
        if keybindings.matches(data, action) {
            return run_app_action(mode, action);
        }
    }
    dispatch_to_editor(screen, mode, data)
}

/// Run one app keybinding action. Every consumed key repaints (upstream's
/// `CustomEditor` leaves the render request to the assignment sites; the port
/// takes the whole action as one paint-worthy input).
fn run_app_action(mode: &Arc<InteractiveMode>, action: &str) -> Vec<ModeAction> {
    let actions = mode.handle_app_action(action);
    mode.mark_dirty();
    actions
}

/// Route one sequence to the focused editor and report its events.
fn dispatch_to_editor(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    data: &str,
) -> Vec<ModeAction> {
    screen.base_mut().handle_terminal_input(data);
    let mut actions = Vec::new();
    // Collect first: the guard would deadlock against `handle_submit` /
    // `on_editor_change`, which lock the editor again.
    let events = mode.editor().lock().take_input_events();
    for event in events {
        match event {
            EditorInputEvent::Changed => mode.on_editor_change(),
            EditorInputEvent::Submitted(text) => actions.extend(mode.handle_submit(&text)),
        }
    }
    actions
}
