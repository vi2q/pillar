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

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_tui::editor::EditorInputEvent;
use pillar_tui::process_terminal::{ProcessTerminal, Terminal};
use pillar_tui::tui::TuiStopOptions;
use pillar_tui::tui_main_screen::TuiMainScreen;

use crate::core::agent_session_class::{AgentSession, AgentSessionEvent, PromptOptions};
use crate::core::extensions_types::{ExtensionUiSlot, MarkdownTransformer};
use crate::core::keybindings::KeybindingsManager;
use crate::core::model_mutation::CycleDirection;
use crate::core::session_manager::{SessionInfo, SessionListProgress, SessionManager};
use crate::modes::interactive::components::session_selector::SessionScope;
use crate::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions, ModeAction,
};
use crate::modes::interactive::theme;
use crate::modes::interactive::theme::controller::{
    InteractiveThemeController, InteractiveThemeControllerOptions,
};
use crate::modes::interactive::transcript::TranscriptSettings;

/// How often the pump drains the terminal and re-renders (upstream relies on
/// the event loop; the port polls).
pub const PUMP_INTERVAL_MS: u64 = 8;

/// Compartments the executor reports back to the pump; every mode mutation
/// stays on the pump thread.
#[derive(Debug)]
enum UiCommand {
    /// A model switch from the `/model` selector settled (upstream the code
    /// after `await session.setModel(...)` in `selectModel`): the pump closes
    /// the selector and reports the outcome.
    ModelSelected {
        provider: String,
        id: String,
        persist: bool,
        error: Option<String>,
    },
    /// A model switch the mode did not initiate settled (Ctrl+P cycling): the
    /// pump refreshes the footer and records the recent-model history.
    ModelCycled { provider: String, id: String },
    /// A bash command finished (upstream the code after `await
    /// session.executeBash(...)`).
    BashComplete {
        exit_code: Option<i32>,
        cancelled: bool,
        truncation: Option<crate::core::truncate::TruncationResult>,
        full_output_path: Option<String>,
    },
    /// A session-list load's intermediate progress (upstream the loaders'
    /// `onProgress` callback).
    SessionsProgress {
        scope: SessionScope,
        loaded: usize,
        total: usize,
    },
    /// A session-list load settled (upstream the loader promise resolving).
    SessionsLoaded {
        scope: SessionScope,
        sessions: Vec<SessionInfo>,
    },
    /// A session delete settled (upstream `deleteSessionFile`).
    SessionDeleted {
        path: String,
        ok: bool,
        moved_to_trash: bool,
        error: Option<String>,
    },
    /// A session rename settled (upstream the `renameSession` callback).
    SessionRenamed { error: Option<String> },
    /// A tree navigation settled (upstream the code after `await
    /// session.navigateTree(...)` in `showTreeSelector`).
    TreeNavigated {
        target_id: String,
        editor_text: Option<String>,
        cancelled: bool,
        aborted: bool,
        error: Option<String>,
    },
    /// A `/reload` settled (upstream the code after `await
    /// session.reload(...)` in `handleReloadCommand`).
    Reloaded,
    /// An extension asked a `ctx.ui` dialog (upstream the awaited
    /// `ExtensionUIContext` methods): the pump shows the dialog and answers
    /// through `reply`. `id` identifies the request so a timeout can cancel
    /// the dialog.
    ExtensionUiAsk {
        id: u64,
        request: crate::core::extensions_types::ExtensionUiRequest,
        reply: std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>,
    },
    /// The extension stopped waiting for `id` (its timeout expired): close the
    /// dialog if it is still the one showing.
    ExtensionUiAskCancel { id: u64 },
    /// An extension opened a `ctx.ui.custom` component (upstream the mode
    /// mounting the factory's component in the editor slot).
    ExtensionCustom {
        surface: crate::core::extensions_types::ExtensionCustomSurface,
    },
    /// An extension called `ctx.ui.*` (upstream the mode's
    /// `ExtensionUIContext` mutating the UI directly; the port queues the
    /// request because the extension holds the Luau runtime lock).
    ExtensionUi { op: String, args: serde_json::Value },
}

/// How [`run_interactive`] ends (upstream the interactive mode keeps running
/// across session switches; the port rebuilds it).
#[derive(Debug, PartialEq, Eq)]
pub enum InteractiveOutcome {
    /// The exit code (upstream `stop` + `process.exit`).
    Exit(i32),
    /// `/resume` picked a session file: the caller re-enters the run loop
    /// with a session built from it.
    SwitchSession { session_path: String },
    /// `/fork` or `/clone` picked an entry: the caller rebuilds the runtime
    /// as a branched session (upstream `runtimeHost.fork`) and restores
    /// `editor_text` into the new editor.
    ForkSession {
        entry_id: String,
        position: String,
        editor_text: Option<String>,
    },
}

/// Options for [`run_interactive`].
#[derive(Clone)]
pub struct InteractiveRunOptions {
    pub mode: InteractiveModeOptions,
    pub transcript: TranscriptSettings,
    pub markdown_transformers: Vec<MarkdownTransformer>,
    /// The `ctx.ui` bridge the run fills with its pump-backed sender
    /// (upstream the mode owns the extension UI context).
    pub extension_ui: Option<ExtensionUiSlot>,
    /// Sent through `session.prompt` before the loop starts (upstream
    /// `options.initialMessage`).
    pub initial_message: Option<String>,
    /// Prefilled into the editor without submitting (upstream the
    /// `editor.setText(result.selectedText)` after a fork).
    pub initial_editor_text: Option<String>,
    /// Shown as a status line once the loop starts (upstream's `showStatus`
    /// after a fork / clone, which the rebuilt mode would otherwise lose).
    pub initial_status: Option<String>,
    /// Where `<agentDir>/keybindings.json` lives.
    pub agent_dir: PathBuf,
}

impl Default for InteractiveRunOptions {
    fn default() -> Self {
        Self {
            mode: InteractiveModeOptions::default(),
            transcript: TranscriptSettings::default(),
            markdown_transformers: Vec::new(),
            extension_ui: None,
            initial_message: None,
            initial_editor_text: None,
            initial_status: None,
            agent_dir: PathBuf::new(),
        }
    }
}

/// Run the interactive mode until shutdown. Returns the process exit code.
pub async fn run_interactive(
    session: Arc<AgentSession>,
    terminal: Box<dyn Terminal>,
    options: InteractiveRunOptions,
) -> Result<InteractiveOutcome, String> {
    let InteractiveRunOptions {
        mode: mut mode_options,
        transcript,
        markdown_transformers,
        extension_ui,
        initial_message,
        initial_editor_text,
        initial_status,
        agent_dir,
    } = options;

    // Upstream `setKeybindings(this.keybindings)`: the merged app + TUI table
    // is what the editor's `tui.*` bindings and the host's `app.*` matches use.
    let keybindings = Arc::new(KeybindingsManager::create(&agent_dir));
    pillar_tui::keybindings::set_keybindings(keybindings.tui_manager());

    // Upstream `setRegisteredThemes(this.session.resourceLoader.getThemes().themes)`:
    // user/project themes must be registered before the controller resolves
    // the settings theme by name.
    for error in theme::register_resource_themes(&session.resource_loader().snapshot().themes) {
        eprintln!("Warning: {error}");
    }

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
    let mut screen = TuiMainScreen::new(terminal);
    screen.base_mut().set_clear_on_shrink(clear_on_shrink);

    // Host facts the 2-column picker needs: the agent directory (its
    // recent-model history) and the live terminal height.
    if mode_options.agent_dir.is_none() {
        mode_options.agent_dir = Some(agent_dir.clone());
    }
    if mode_options.terminal_rows.is_none() {
        mode_options.terminal_rows = Some(Arc::new(std::sync::atomic::AtomicUsize::new(
            screen.base_mut().terminal_mut().rows(),
        )));
    }

    // Upstream constructs the controller inside the `InteractiveMode`
    // constructor (it initializes the global theme there) and applies the
    // settings after `ui.start()`. Its callbacks cannot borrow the mode, so
    // they record the effect and the pump applies it.
    let theme_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let theme_changed = Arc::new(AtomicBool::new(false));
    let theme_controller = InteractiveThemeController::new(
        screen.base_mut(),
        session.settings_manager_arc(),
        InteractiveThemeControllerOptions {
            show_error: Box::new({
                let cell = Arc::clone(&theme_errors);
                move |message: &str| cell.lock().expect("theme errors").push(message.to_string())
            }),
            on_changed: Box::new({
                let flag = Arc::clone(&theme_changed);
                move || flag.store(true, Ordering::SeqCst)
            }),
            initial_theme_setting: None,
        },
    );

    let mode = Arc::new(InteractiveMode::new(
        Arc::clone(&session),
        transcript,
        markdown_transformers,
        mode_options,
    ));
    mode.render_initial_messages();
    mode.update_terminal_title();
    mode.update_editor_border_color();
    // Upstream `init()` (the footer needs the provider count before the first
    // render; the model selector keeps it fresh afterwards).
    mode.update_available_provider_count();
    // Upstream `setupAutocompleteProvider()`: build the command table and hand
    // the dropdown the editor renders to the mode.
    mode.rebuild_autocomplete();

    let editor_slot = mode.mount(screen.base_mut());
    screen.base_mut().set_focus(Some(editor_slot));
    screen.base_mut().start();

    // Session events are queued for the pump, which owns all mode mutation.
    let events = Arc::new(Mutex::new(EventBacklog::default()));
    let unsubscribe = session.subscribe({
        let events = Arc::clone(&events);
        Arc::new(move |event| {
            events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event.clone());
        })
    });

    // Pump -> executor work. Unlike the event backlog this stays unbounded on
    // purpose: its producers are user input (`dispatch_input`), per-request
    // completions and the initial message, while session events never queue an
    // action (`handle_event` reports none — streaming tokens therefore cannot
    // generate one action per token). Bounding it would mean dropping an
    // abort or a submission, which is worse than the queue a human can build.
    // `streaming_events_do_not_queue_executor_actions` pins that invariant.
    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<ModeAction>();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
    // Refuses an extension's UI request once the pump is this far behind, so a
    // flooding extension gets an error instead of growing the queue without
    // bound (the executor must never wait on the pump: abort stays live).
    let ui_pending = Arc::new(AtomicUsize::new(0));
    let ask_ids = Arc::new(AtomicU64::new(1));
    // The `ctx.ui` bridge (upstream `createExtensionUIContext` lives on the
    // mode): an extension only queues a request on this channel, so it never
    // blocks on — or locks — the mode while holding the Luau runtime.
    if let Some(slot) = &extension_ui {
        let sender = ui_tx.clone();
        let pending = Arc::clone(&ui_pending);
        let bridge: crate::core::extensions_types::ExtensionUiFn = Arc::new(
            move |request: crate::core::extensions_types::ExtensionUiRequest| {
                if pending.load(Ordering::SeqCst) >= UI_REQUEST_LIMIT {
                    return Err("ctx.ui: the interactive mode is behind on UI requests".to_string());
                }
                sender
                    .send(UiCommand::ExtensionUi {
                        op: request.op,
                        args: request.args,
                    })
                    .map_err(|_| "ctx.ui: the interactive mode has stopped".to_string())?;
                pending.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        // Dialogs block the extension's thread until the pump answers (the
        // mode's dialog components are native, so the pump never needs the
        // runner lock while one is open). A timeout cancels the dialog instead
        // of leaving a stale question in the editor slot.
        // A generation id is unnecessary here: each run owns its own bridge and
        // channel, so a stale generation's request cannot reach this pump — its
        // reply sender is dropped with the previous run's queue.
        let ask_sender = ui_tx.clone();
        let custom_sender = ui_tx.clone();
        let ask_pending = Arc::clone(&ui_pending);
        let ask_cancel_sender = ui_tx.clone();
        let ask_ids = Arc::clone(&ask_ids);
        let ask: crate::core::extensions_types::ExtensionUiAskFn = Arc::new(move |request| {
            if ask_pending.load(Ordering::SeqCst) >= UI_REQUEST_LIMIT {
                return Err("ctx.ui: the interactive mode is behind on UI requests".to_string());
            }
            let id = ask_ids.fetch_add(1, Ordering::SeqCst);
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            ask_sender
                .send(UiCommand::ExtensionUiAsk { id, request, reply })
                .map_err(|_| "ctx.ui: the interactive mode has stopped".to_string())?;
            ask_pending.fetch_add(1, Ordering::SeqCst);
            match answer.recv_timeout(std::time::Duration::from_secs(600)) {
                Ok(result) => result,
                Err(_) => {
                    let _ = ask_cancel_sender.send(UiCommand::ExtensionUiAskCancel { id });
                    Err("ctx.ui: the dialog was not answered".to_string())
                }
            }
        });
        // `ctx.ui.custom`: the render loop runs on the extension's thread and
        // drives the surface itself, so the installer only queues a mount.
        let custom: crate::core::extensions_types::ExtensionCustomFn = {
            let sender = custom_sender.clone();
            Arc::new(move |surface| {
                if surface.closed.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(());
                }
                sender
                    .send(UiCommand::ExtensionCustom { surface })
                    .map_err(|_| "ctx.ui: the interactive mode has stopped".to_string())
            })
        };
        // `session_start` runs before this loop, so an extension's UI setup is
        // queued in the slot: install the bridge and replay the queue.
        let queued = {
            let mut state = slot.lock().expect("extension ui slot");
            state.bridge = Some(Arc::clone(&bridge));
            state.ask = Some(ask);
            state.custom = Some(custom);
            let queued = std::mem::take(&mut state.pending);
            let queued_custom = std::mem::take(&mut state.pending_custom);
            (queued, queued_custom)
        };
        for request in queued.0 {
            let _ = bridge(request);
        }
        for surface in queued.1 {
            let _ = custom_sender.send(UiCommand::ExtensionCustom { surface });
        }
    }
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let pump_shutdown = Arc::new(AtomicBool::new(false));

    if let Some(initial_message) = initial_message {
        let _ = action_tx.send(ModeAction::Prompt {
            text: initial_message,
            streaming_behavior: None,
        });
    }
    if let Some(initial_editor_text) = initial_editor_text {
        mode.set_editor_text(&initial_editor_text);
    }
    if let Some(initial_status) = initial_status {
        mode.transcript().lock().show_status(&initial_status);
        mode.mark_dirty();
    }

    let pump_mode = Arc::clone(&mode);
    let pump_keybindings = Arc::clone(&keybindings);
    let pump_stop = Arc::clone(&pump_shutdown);
    let pump_actions = action_tx.clone();
    let pump_exit = shutdown_tx.clone();
    let pump_title = Arc::clone(&pending_title);
    let pump_progress = Arc::clone(&pending_progress);
    let pump_theme_errors = Arc::clone(&theme_errors);
    let pump_theme_changed = Arc::clone(&theme_changed);
    let pump = std::thread::spawn(move || {
        let mut editor_slot = editor_slot;
        let result = pump_loop(
            screen,
            pump_mode,
            pump_keybindings,
            events,
            pump_actions,
            ui_rx,
            ui_pending,
            &mut editor_slot,
            pump_stop,
            pump_title,
            pump_progress,
            theme_controller,
            pump_theme_errors,
            pump_theme_changed,
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
    if let Some(slot) = &extension_ui {
        let mut state = slot.lock().expect("extension ui slot");
        state.bridge = None;
        state.ask = None;
    }
    unsubscribe();
    session.dispose();
    match pump_result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("the interactive render loop panicked".to_string()),
    }
}

/// [`run_interactive`] over a fresh [`ProcessTerminal`] (the `pillar` binary's
/// path; tests inject their own terminal).
pub async fn run_interactive_process(
    session: Arc<AgentSession>,
    options: InteractiveRunOptions,
) -> Result<InteractiveOutcome, String> {
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
        ModeAction::Reload => {
            session
                .reload(None)
                .await
                .map_err(|error| format!("Reload failed: {error}"))?;
            let _ = ui.send(UiCommand::Reloaded);
            Ok(())
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
            let Some(outcome) = outcome else {
                let scoped = !session.scoped_models().is_empty();
                let message = if scoped {
                    "Only one model in scope"
                } else {
                    "Only one model available"
                };
                return Err(message.to_string());
            };
            let _ = ui.send(UiCommand::ModelCycled {
                provider: outcome.model.provider,
                id: outcome.model.id,
            });
            Ok(())
        }
        // Upstream `selectModel`: resolve the model instance from the runtime
        // (the selector handed over `provider`/`id`), switch, and report back.
        // The error travels inside the command so the pump can close the
        // selector and show it (upstream `done(); showError()`) instead of the
        // executor loop reporting it a second time.
        ModeAction::SelectModel {
            provider,
            id,
            persist,
        } => {
            let result = match session.model_runtime().get_model(&provider, &id) {
                Some(model) => session.set_model(model, persist).await,
                None => Err(format!("Unknown model {provider}/{id}")),
            };
            let _ = ui.send(UiCommand::ModelSelected {
                provider,
                id,
                persist,
                error: result.err(),
            });
            Ok(())
        }
        // Upstream the session-selector loaders (`SessionsLoader`): scan the
        // scope's directories and report the result. The progress callback
        // streams intermediate `SessionsProgress` commands.
        ModeAction::LoadSessions { scope } => {
            let (cwd, session_dir, uses_default) = {
                let manager = session.session_manager().lock().expect("session lock");
                (
                    manager.cwd().to_string(),
                    manager.session_dir().to_path_buf(),
                    manager.uses_default_session_dir(),
                )
            };
            let progress_ui = ui.clone();
            let progress: SessionListProgress = Arc::new(move |loaded, total| {
                let _ = progress_ui.send(UiCommand::SessionsProgress {
                    scope,
                    loaded,
                    total,
                });
            });
            let sessions = match scope {
                SessionScope::Current => {
                    SessionManager::list(&cwd, Some(&session_dir), Some(&progress))
                }
                SessionScope::All => {
                    if uses_default {
                        SessionManager::list_all(None, Some(&progress))
                    } else {
                        SessionManager::list_all(Some(&session_dir), Some(&progress))
                    }
                }
            };
            let _ = ui.send(UiCommand::SessionsLoaded { scope, sessions });
            Ok(())
        }
        // Upstream `deleteSessionFile`: try the `trash` CLI first, then fall
        // back to a permanent unlink.
        ModeAction::DeleteSession { path } => {
            let result = delete_session_file(Path::new(&path));
            let _ = ui.send(UiCommand::SessionDeleted {
                path,
                ok: result.is_ok(),
                moved_to_trash: result == Ok(true),
                error: result.err(),
            });
            Ok(())
        }
        // Upstream the selector's `renameSession`: open the target file and
        // append a `session_info` entry.
        ModeAction::RenameSession { path, name } => {
            let result = SessionManager::open(Path::new(&path), None, None)
                .and_then(|mut manager| manager.append_session_info(&name));
            let _ = ui.send(UiCommand::SessionRenamed {
                error: result.err(),
            });
            Ok(())
        }
        // The pump owns the session switch (it ends the run loop); the
        // executor only sees a no-op.
        ModeAction::ResumeSession { .. } => Ok(()),
        // The pump also owns the fork (it ends the run loop and the caller
        // rebuilds the runtime).
        ModeAction::ForkSession { .. } => Ok(()),
        // The pump owns the TUI screen and the theme controller.
        ModeAction::ThemePreview(_)
        | ModeAction::ThemeApplied(_)
        | ModeAction::SetShowHardwareCursor(_)
        | ModeAction::SetClearOnShrink(_) => Ok(()),
        // Upstream `showTreeSelector`'s `onSelect`: stop a streaming response
        // first, then move the leaf (optionally summarizing the branch).
        ModeAction::NavigateTree {
            target_id,
            summarize,
            custom_instructions,
        } => {
            if session.is_streaming() {
                session.abort().await;
            }
            let result = session
                .navigate_tree(
                    &target_id,
                    crate::core::agent_session_class::TreeNavigationOptions {
                        summarize,
                        custom_instructions: custom_instructions.clone(),
                        ..Default::default()
                    },
                )
                .await;
            let _ = ui.send(UiCommand::TreeNavigated {
                target_id,
                editor_text: result
                    .as_ref()
                    .ok()
                    .and_then(|navigation| navigation.editor_text.clone()),
                cancelled: result.as_ref().map(|r| r.cancelled).unwrap_or(false),
                aborted: result.as_ref().map(|r| r.aborted).unwrap_or(false),
                error: result.err(),
            });
            Ok(())
        }
        ModeAction::Shutdown => Ok(()),
        // Handled by the pump (it owns the TUI); never reaches the executor.
        ModeAction::EditorSlotChanged => Ok(()),
    }
}

/// Upstream `deleteSessionFile`: move the file to the trash with the `trash`
/// CLI when it is installed, else unlink it. `Ok(true)` = trash,
/// `Ok(false)` = unlinked.
fn delete_session_file(path: &Path) -> Result<bool, String> {
    let trash = std::process::Command::new("trash")
        .arg(if path.starts_with("-") {
            format!("--{}", path.to_string_lossy())
        } else {
            path.to_string_lossy().to_string()
        })
        .output();
    let mut trash_hint = None;
    if let Ok(output) = &trash {
        if output.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("").trim().to_string();
        if !first_line.is_empty() {
            trash_hint = Some(first_line);
        }
    }
    // Fallback to permanent deletion (upstream the unlink fallback).
    match fs::remove_file(path) {
        Ok(()) => Ok(false),
        Err(error) => {
            let message = match trash_hint {
                Some(hint) => format!("{error} (trash: {hint})"),
                None => error.to_string(),
            };
            Err(message)
        }
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

/// How many session events one pump iteration handles before it returns to
/// terminal input: a flooding producer must not starve the keyboard, the abort
/// keys or the executor's completion reports.
const EVENT_DRAIN_BATCH: usize = 256;

/// Hard cap on pending session events. It is only reached while the pump is
/// blocked in a dialog and critical events keep arriving — streaming updates
/// are coalesced away long before that.
const EVENT_BACKLOG_LIMIT: usize = 4096;

/// How many `ctx.ui` requests may wait for the pump before an extension's
/// request is refused (the executor must never wait on the pump, so the
/// refusal is free backpressure instead of a blocking send).
const UI_REQUEST_LIMIT: usize = 256;

/// The session events waiting for the pump, with streaming updates coalesced.
///
/// A streaming turn emits one `MessageUpdate` per token and one
/// `ToolExecutionUpdate` per output chunk, and each of them carries the full
/// snapshot (`message` / `partial_result`) the transcript renders, so only the
/// newest update per streaming target matters. Everything else (message and
/// tool boundaries, errors, persistence events) queues in order and is never
/// dropped (docs/ARCHITECTURE-REVIEW-s05c0.md D).
#[derive(Default)]
pub struct EventBacklog {
    queue: VecDeque<AgentSessionEvent>,
}

impl EventBacklog {
    /// Queue an event, replacing the previous update of the same streaming
    /// target: the newer snapshot subsumes the older one.
    pub fn push(&mut self, event: AgentSessionEvent) {
        if let Some(last) = self.queue.back_mut()
            && same_streaming_target(last, &event)
        {
            *last = event;
            return;
        }
        if self.queue.len() >= EVENT_BACKLOG_LIMIT {
            // The pump is stalled (a dialog) and the backlog is at its cap:
            // shed a coalescible update rather than a critical event.
            let _ = self.drop_oldest_update();
        }
        self.queue.push_back(event);
    }

    /// Take up to `limit` events for one pump iteration.
    pub fn take(&mut self, limit: usize) -> Vec<AgentSessionEvent> {
        let count = limit.min(self.queue.len());
        self.queue.drain(..count).collect()
    }

    /// Drop the oldest coalescible update; false when every pending event is
    /// critical (they carry the transcript, so the cap yields to them).
    fn drop_oldest_update(&mut self) -> bool {
        let Some(index) = self.queue.iter().position(is_streaming_update) else {
            return false;
        };
        self.queue.remove(index);
        true
    }
}

/// Whether two events update the same streaming target, so the newer one
/// subsumes the older.
fn same_streaming_target(previous: &AgentSessionEvent, next: &AgentSessionEvent) -> bool {
    match (previous, next) {
        (AgentSessionEvent::MessageUpdate { .. }, AgentSessionEvent::MessageUpdate { .. }) => true,
        (
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id: previous,
                ..
            },
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id: next, ..
            },
        ) => previous == next,
        _ => false,
    }
}

fn is_streaming_update(event: &AgentSessionEvent) -> bool {
    matches!(
        event,
        AgentSessionEvent::MessageUpdate { .. } | AgentSessionEvent::ToolExecutionUpdate { .. }
    )
}

/// The pump: terminal input, session events, and rendering, all on one thread.
#[allow(clippy::too_many_arguments)]
fn pump_loop(
    mut screen: TuiMainScreen,
    mode: Arc<InteractiveMode>,
    keybindings: Arc<KeybindingsManager>,
    events: Arc<Mutex<EventBacklog>>,
    actions: tokio::sync::mpsc::UnboundedSender<ModeAction>,
    mut ui_commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    ui_pending: Arc<AtomicUsize>,
    editor_slot: &mut pillar_tui::tui::ComponentId,
    shutdown: Arc<AtomicBool>,
    pending_title: Arc<Mutex<Option<String>>>,
    pending_progress: Arc<Mutex<Option<bool>>>,
    mut theme_controller: InteractiveThemeController,
    theme_errors: Arc<Mutex<Vec<String>>>,
    theme_changed: Arc<AtomicBool>,
) -> Result<InteractiveOutcome, String> {
    let interval = Duration::from_millis(PUMP_INTERVAL_MS);
    // Upstream `init()` applies the settings theme once the UI is running.
    // It runs inside the loop because the detection queries pump input:
    // keystrokes consumed while waiting for the terminal's reply are
    // dispatched (and their editor events drained) like any other input.
    //
    // divergence: input consumed *during* a query goes through the TUI's
    // focused-component dispatch, so the host's app keybindings (Ctrl-C /
    // Ctrl-D / Escape) do not apply for that ~100ms window (upstream's
    // `CustomEditor` owns those bindings and still sees them). Only the
    // `auto` / unset theme settings query the terminal.
    let mut theme_applied = false;
    let result = loop {
        if shutdown.load(Ordering::SeqCst) {
            break Ok(InteractiveOutcome::Exit(0));
        }

        if !theme_applied {
            theme_applied = true;
            theme_controller.apply_from_settings(screen.base_mut());
            mode.update_editor_border_color();
            let mut actions_out = Vec::new();
            drain_editor_events(&mode, &mut actions_out);
            for action in actions_out {
                if dispatch_action(&mut screen, &mode, editor_slot, &actions, action).is_err() {
                    break;
                }
            }
        }

        // Terminal side effects the mode reported (upstream calls the terminal
        // from `setTitle` / `setProgress`).
        if let Some(title) = pending_title.lock().expect("title cell").take() {
            screen.base_mut().terminal_mut().set_title(&title);
        }
        if let Some(running) = pending_progress.lock().expect("progress cell").take() {
            screen.base_mut().terminal_mut().set_progress(running);
        }

        // Session events (queued by the listener). One bounded batch per
        // iteration: a flooding producer must not starve the terminal, the
        // abort keys or the executor's completion reports.
        let batch = {
            let mut backlog = events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            backlog.take(EVENT_DRAIN_BATCH)
        };
        for event in batch {
            let reported = mode.handle_event(&event);
            mode.mark_dirty();
            for action in reported {
                if actions.send(action).is_err() {
                    break;
                }
            }
        }

        // Executor reports (bash completion, model switches) and the `ctx.ui`
        // requests the bridge admitted. Only those two counted commands release
        // a slot; the executor's own reports were never counted.
        while let Ok(command) = ui_commands.try_recv() {
            if matches!(
                command,
                UiCommand::ExtensionUi { .. } | UiCommand::ExtensionUiAsk { .. }
            ) {
                let _ = ui_pending.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                    Some(pending.saturating_sub(1))
                });
            }
            let reported: Vec<ModeAction> = match command {
                UiCommand::BashComplete {
                    exit_code,
                    cancelled,
                    truncation,
                    full_output_path,
                } => {
                    mode.complete_bash(exit_code, cancelled, truncation, full_output_path);
                    Vec::new()
                }
                UiCommand::ModelSelected {
                    provider,
                    id,
                    persist,
                    error,
                } => mode.complete_model_selection(&provider, &id, persist, error),
                UiCommand::ModelCycled { provider, id } => {
                    mode.complete_model_cycle(&provider, &id);
                    Vec::new()
                }
                UiCommand::SessionsProgress {
                    scope,
                    loaded,
                    total,
                } => {
                    mode.session_load_progress(scope, loaded, total);
                    Vec::new()
                }
                UiCommand::SessionsLoaded { scope, sessions } => {
                    mode.session_list_loaded(scope, sessions);
                    Vec::new()
                }
                UiCommand::SessionDeleted {
                    path,
                    ok,
                    moved_to_trash,
                    error,
                } => mode.complete_session_delete(&path, ok, moved_to_trash, error),
                UiCommand::SessionRenamed { error } => mode.complete_session_rename(error),
                // Upstream `handleReloadCommand`'s status line. The port
                // re-runs settings / resource / runner loading; re-applying
                // keybindings, the theme and the autocomplete provider stays
                // host-side (recorded divergence).
                UiCommand::Reloaded => {
                    mode.transcript().lock().show_status(
                        "Reloaded extensions, skills, prompts, themes, and context files",
                    );
                    mode.mark_dirty();
                    Vec::new()
                }
                UiCommand::ExtensionUiAsk { id, request, reply } => {
                    mode.begin_extension_ask(id, request, reply);
                    Vec::new()
                }
                UiCommand::ExtensionUiAskCancel { id } => mode.cancel_extension_ask(id),
                UiCommand::ExtensionCustom { surface } => mode.begin_extension_custom(surface),
                UiCommand::ExtensionUi { op, args } => {
                    if let Err(error) = mode.handle_extension_ui(
                        &crate::core::extensions_types::ExtensionUiRequest { op, args },
                    ) {
                        eprintln!("Warning: {error}");
                    }
                    Vec::new()
                }
                UiCommand::TreeNavigated {
                    target_id,
                    editor_text,
                    cancelled,
                    aborted,
                    error,
                } => mode.complete_tree_navigation(
                    &target_id,
                    editor_text,
                    cancelled,
                    aborted,
                    error,
                ),
            };
            for action in reported {
                if dispatch_action(&mut screen, &mode, editor_slot, &actions, action).is_err() {
                    break;
                }
            }
        }

        // Theme controller: drain terminal color-scheme reports (auto
        // sync) and apply the recorded effects.
        theme_controller.pump(screen.base_mut());
        // The `/settings` panel shows the detected terminal brightness
        // (upstream `themeController.getTerminalTheme()`).
        mode.set_terminal_theme(theme_controller.get_terminal_theme());
        if theme_changed.swap(false, Ordering::SeqCst) {
            mode.update_editor_border_color();
        }
        let reported_errors: Vec<String> = {
            let mut guard = theme_errors.lock().expect("theme errors");
            std::mem::take(&mut *guard)
        };
        for error in reported_errors {
            mode.transcript().lock().show_error(&error);
            mode.mark_dirty();
        }

        // Host-driven animations (retry countdown, status expiry).
        mode.tick();
        // `ctx.ui.custom` frames the render loop painted since the last
        // iteration (and the close when it finished).
        for action in mode.poll_extension_custom() {
            if dispatch_action(&mut screen, &mode, editor_slot, &actions, action).is_err() {
                break;
            }
        }

        // Terminal input.
        let mut requested_shutdown = false;
        let mut resume_path: Option<String> = None;
        let mut fork_request: Option<(String, String, Option<String>)> = None;
        let data = screen.base_mut().terminal_mut().read_input(interval);
        // On idle, a buffered partial escape sequence flushes once its
        // disambiguation deadline passed (upstream the StdinBuffer's own
        // timer). Without this a lone Escape waits for the next keypress,
        // which breaks every `tui.select.cancel` / `app.interrupt` binding
        // that is a bare Escape.
        let input_actions = match data {
            Some(data) => dispatch_input(&mut screen, &mode, editor_slot, &keybindings, &data),
            None => {
                let flushed = screen
                    .base_mut()
                    .terminal_mut()
                    .flush_pending_input(Instant::now());
                dispatch_sequences(&mut screen, &mode, editor_slot, &keybindings, flushed)
            }
        };
        for action in input_actions {
            if matches!(action, ModeAction::Shutdown) {
                requested_shutdown = true;
            }
            // Upstream `onSelect` → `handleResumeSession`: the session switch
            // replaces the whole runtime, which in this port ends the run
            // loop; the caller re-enters it with the new session. The close
            // action (if any) already dispatched.
            if let ModeAction::ResumeSession { session_path } = action {
                resume_path = Some(session_path);
                break;
            }
            // The theme controller lives on the pump (upstream the mode owns
            // it); the screen options are applied here too.
            match &action {
                ModeAction::ThemePreview(setting) => {
                    theme_controller.preview(screen.base_mut(), setting);
                    continue;
                }
                ModeAction::ThemeApplied(setting) => {
                    theme_controller.set_theme_setting(screen.base_mut(), setting);
                    continue;
                }
                ModeAction::SetShowHardwareCursor(enabled) => {
                    screen.base_mut().set_show_hardware_cursor(*enabled);
                    continue;
                }
                ModeAction::SetClearOnShrink(enabled) => {
                    screen.base_mut().set_clear_on_shrink(*enabled);
                    continue;
                }
                _ => {}
            }
            // Upstream `runtimeHost.fork`: the branched session replaces the
            // whole runtime, which in this port ends the run loop; the caller
            // rebuilds it (and restores the forked-from message text).
            if let ModeAction::ForkSession {
                entry_id,
                position,
                editor_text,
            } = action
            {
                fork_request = Some((entry_id, position, editor_text));
                break;
            }
            if dispatch_action(&mut screen, &mode, editor_slot, &actions, action).is_err() {
                break;
            }
        }
        if let Some(session_path) = resume_path {
            break Ok(InteractiveOutcome::SwitchSession { session_path });
        }
        if let Some((entry_id, position, editor_text)) = fork_request {
            break Ok(InteractiveOutcome::ForkSession {
                entry_id,
                position,
                editor_text,
            });
        }

        // Rendering (a shutdown request still paints the final frame).
        if mode.take_dirty() {
            screen.base_mut().invalidate();
            screen.base_mut().request_render(false);
        }
        if requested_shutdown {
            // Upstream `shutdown()` awaits `drainInput(1000)`, which lets the
            // queued `requestRender()` from `editor.setText("")` paint before
            // the terminal stops. The port breaks out of the loop in this same
            // iteration, so the frame has to be forced: `request_render(false)`
            // above defers by `MIN_RENDER_INTERVAL_MS`, and without this frame
            // the autocomplete popup and the typed text stay on screen after
            // exit (pi repaints them away).
            screen.base_mut().request_render(true);
        }
        if screen.base_mut().terminal_mut().resize_if_changed() {
            screen.base_mut().invalidate();
            // The 2-column picker lays out against the terminal height
            // (upstream reads `tui.terminal.rows` on every render).
            mode.set_terminal_rows(screen.base_mut().terminal_mut().rows());
            // Upstream's terminal calls `requestRender()` from its resize
            // handler; without a frame here the pre-resize lines stay on a
            // terminal that has already reflowed them. The port's `force` only
            // makes the request immediate (it does not reset the render state),
            // so the width/height change still goes through `full_render`.
            screen.base_mut().request_render(true);
        }
        if screen.base_mut().take_render_request(Instant::now()) {
            if let Err(error) = screen.do_render() {
                break Err(error);
            }
        }
        if requested_shutdown {
            break Ok(InteractiveOutcome::Exit(0));
        }
    };

    if matches!(result, Ok(InteractiveOutcome::Exit(_))) {
        // Upstream `shutdown()` drains stdin before stopping (`drainInput(1000)`,
        // stopping after 50 ms idle) so late key releases or the tail of a paste
        // cannot reach the shell after the terminal is restored.
        screen.base_mut().terminal_mut().drain_input(1000, 50);
    }
    screen.stop(TuiStopOptions {
        preserve_screen: false,
    });
    result
}

/// Hand one action to the executor, applying the pump-side bookkeeping first:
/// upstream creates the bash block inside `handleBashCommand` (before the
/// command runs) and swaps the editor slot inside `showSelector` / `done`.
fn dispatch_action(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    editor_slot: &mut pillar_tui::tui::ComponentId,
    actions: &tokio::sync::mpsc::UnboundedSender<ModeAction>,
    action: ModeAction,
) -> Result<(), tokio::sync::mpsc::error::SendError<ModeAction>> {
    if let ModeAction::Bash { command, excluded } = &action {
        mode.begin_bash(command, *excluded);
    }
    // Upstream the block before `session.navigateTree(...)`: stop a streaming
    // response, restore its queued messages, and show the branch summary
    // spinner. The abort is sent before the navigation so the executor sees
    // it first.
    if let ModeAction::NavigateTree { summarize, .. } = &action {
        for pre_action in mode.begin_tree_navigation(*summarize) {
            if actions.send(pre_action).is_err() {
                return Ok(());
            }
        }
    }
    if matches!(action, ModeAction::EditorSlotChanged) {
        // Upstream `editorContainer.clear()` + `addChild(...)` + `setFocus`.
        let base = screen.base_mut();
        base.replace_child(*editor_slot, mode.editor_slot_component());
        base.set_focus(Some(*editor_slot));
        base.invalidate();
        return Ok(());
    }
    actions.send(action)
}

/// Feed raw terminal bytes through the ported input pipeline and dispatch the
/// resulting sequences.
fn dispatch_input(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    editor_slot: &mut pillar_tui::tui::ComponentId,
    keybindings: &KeybindingsManager,
    data: &str,
) -> Vec<ModeAction> {
    let now = Instant::now();
    let sequences = {
        let terminal = screen.base_mut().terminal_mut();
        terminal.feed_input_bytes(data, now)
    };
    dispatch_sequences(screen, mode, editor_slot, keybindings, sequences)
}

/// Feed parsed sequences through the terminal's kitty-negotiation filter and
/// dispatch what survives (upstream the `stdinBuffer.on("data")` handler).
fn dispatch_sequences(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    editor_slot: &mut pillar_tui::tui::ComponentId,
    keybindings: &KeybindingsManager,
    sequences: Vec<String>,
) -> Vec<ModeAction> {
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
        actions.extend(dispatch_sequence(
            screen,
            mode,
            editor_slot,
            keybindings,
            &sequence,
        ));
    }
    actions
}

/// Upstream `CustomEditor.handleInput`: the app keybindings win over the
/// editor, and the rest goes through the TUI's focused-component dispatch.
fn dispatch_sequence(
    screen: &mut TuiMainScreen,
    mode: &Arc<InteractiveMode>,
    editor_slot: &mut pillar_tui::tui::ComponentId,
    keybindings: &KeybindingsManager,
    data: &str,
) -> Vec<ModeAction> {
    // Autocomplete (host-side provider): Tab triggers a request or applies the
    // highlighted completion, and Enter applies it while the menu is open
    // (upstream the editor owns both; the port's editor only renders the
    // dropdown, so the host intercepts them before the editor sees them).
    if keybindings.matches(data, "tui.input.tab") {
        let actions = mode.autocomplete_tab();
        mode.mark_dirty();
        return actions;
    }
    if keybindings.matches(data, "tui.select.confirm") && mode.autocomplete_is_open() {
        let actions = mode.autocomplete_accept();
        mode.mark_dirty();
        return actions;
    }
    // Kitty flag 2 (report event types) makes a kitty-protocol terminal send
    // a release event for every press. Upstream drops those inside
    // `TUI.handleInput` unless the focused component asked for them
    // (`wantsKeyRelease`), so the host-side app keybindings and the selectors
    // must not see them either — otherwise every action runs twice. The TUI's
    // focused dispatch applies that rule, so releases go there directly.
    if pillar_tui::tui::is_key_release(data) {
        return dispatch_to_editor(screen, mode, data);
    }
    // A selector owns the keyboard while it is open (upstream it is the
    // focused component); the host app keybindings do not apply. The port must
    // request the repaint that the TUI's focused dispatch would have made
    // (`handle_terminal_input` renders on every key), otherwise navigation and
    // search typing stay invisible even though the state changes.
    if let Some(actions) = mode.handle_selector_key(data) {
        mode.mark_dirty();
        return actions;
    }
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
    let _ = editor_slot;
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
    drain_editor_events(mode, &mut actions);
    actions
}

/// Report the editor events that accumulated since the last drain (upstream
/// the `onChange` / `onSubmit` callbacks).
fn drain_editor_events(mode: &Arc<InteractiveMode>, actions: &mut Vec<ModeAction>) {
    // Collect first: the guard would deadlock against `handle_submit` /
    // `on_editor_change`, which lock the editor again.
    let events = mode.editor().lock().take_input_events();
    for event in events {
        match event {
            EditorInputEvent::Changed => mode.on_editor_change(),
            EditorInputEvent::Submitted(text) => actions.extend(mode.handle_submit(&text)),
        }
    }
}
