//! Parity tests for the host-driven interactive run loop (pi v0.84.3
//! `interactive-mode.ts` `init` / `run` plus the terminal pump): mounting the
//! mode into the renderer, the editor key path into `handle_submit`, the
//! `ModeAction` execution, and the shutdown handshake.
//!
//! The scripted terminal goes through the real `ProcessTerminal` over injected
//! `TerminalIo`, so `StdinBuffer` splits the raw bytes like upstream.
//!
//! The fixtures set an explicit `theme` setting: the theme controller's
//! detection path queries the terminal (pumping input), and input consumed
//! during that query goes through the TUI dispatch rather than the host's app
//! keybindings (port divergence; see the live-bash/theme TASKS entry).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pillar_agent::{Agent, AgentOptions, AgentState, AgentThinkingLevel, FauxModelRef};
use pillar_ai::auth_types::{ApiKeyCredential, Credential, CredentialInfo, CredentialStore};
use pillar_ai::error::AiError;
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Message, StopReason, Usage, UsageCost,
};
use pillar_coding_agent::core::agent_session_class::{AgentSession, AgentSessionConfig};
use pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore;
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use pillar_coding_agent::modes::interactive::interactive_mode::InteractiveModeOptions;
use pillar_coding_agent::modes::interactive::run::{
    InteractiveOutcome, InteractiveRunOptions, run_interactive,
};
use pillar_coding_agent::modes::interactive::theme;
use pillar_coding_agent::modes::interactive::transcript::TranscriptSettings;
use pillar_tui::process_terminal::{ProcessTerminal, TerminalIo};
use pillar_tui::text_utils::strip_terminal_sequences;

fn install_dark() {
    theme::init_theme(Some("dark"));
}

fn create_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost::default(),
    }
}

fn mock_model() -> FauxModelRef {
    FauxModelRef {
        id: "claude-sonnet-4-5".into(),
        name: "Claude Sonnet 4.5".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec!["text".to_string()],
        cost: UsageCost::default(),
        context_window: 200_000,
        max_tokens: 8_000,
    }
}

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::text(text)],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        response_model: None,
        usage: create_usage(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// Stream fn that answers every prompt with one text block.
fn echo_stream(reply: &'static str) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| async move {
        let stream = assistant_message_event_stream();
        let message = assistant_message(reply);
        stream.push(AssistantMessageEvent::Start {
            partial: message.clone(),
        });
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            partial: message.clone(),
            delta: reply.to_string(),
        });
        stream.push(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message,
        });
        stream
    })
}

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

fn temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-interactive-run-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn runtime_with_anthropic_key(label: &str) -> ModelRuntime {
    let credentials = Arc::new(MemCredentials::default());
    credentials.0.lock().unwrap().insert(
        "anthropic".to_string(),
        Credential::ApiKey(ApiKeyCredential {
            key: Some("test-key".to_string()),
            env: None,
        }),
    );
    let dir = temp_dir(label);
    let models_path = dir.join("models.json");
    // A valid empty config: `{}` would leave a `ModelRuntime::get_error()`
    // config error that the model selector renders as its error message.
    std::fs::write(&models_path, "{\"providers\":{}}").unwrap();
    ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        credentials: Some(credentials),
        ..Default::default()
    })
    .unwrap()
}

fn session(stream_fn: pillar_agent::StreamFn, label: &str) -> Arc<AgentSession> {
    session_with_scoped_models(stream_fn, label, Vec::new())
}

/// A session whose `--models` scope is populated (the model selector's initial
/// `scoped` scope).
fn session_with_scoped_models(
    stream_fn: pillar_agent::StreamFn,
    label: &str,
    scoped_models: Vec<pillar_coding_agent::core::model_mutation::ScopedModel>,
) -> Arc<AgentSession> {
    let mut options = AgentOptions::new(stream_fn);
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model: mock_model(),
        thinking_level: AgentThinkingLevel::Off,
        tools: Vec::new(),
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory(&temp_dir(label).to_string_lossy(), None).expect("in-memory"),
    ));
    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({ "theme": "dark" }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir(label).join("agent").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));
    let mut config = AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        temp_dir(label).to_string_lossy().to_string(),
        resource_loader,
        Arc::new(runtime_with_anthropic_key(label)),
        Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
    );
    config.scoped_models = scoped_models;
    Arc::new(AgentSession::new(config))
}

/// Terminal I/O serving scripted input chunks and recording output.
struct ScriptedIo {
    writes: Arc<Mutex<String>>,
    chunks: Arc<Mutex<Vec<String>>>,
    /// Gate for every chunk after the first; `None` releases immediately.
    release: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    first_taken: bool,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl TerminalIo for ScriptedIo {
    fn write(&mut self, data: &str) {
        self.writes.lock().unwrap().push_str(data);
    }
    fn size(&self) -> (usize, usize) {
        (80, 24)
    }
    fn enable_raw_mode(&mut self) -> bool {
        self.started.store(true, Ordering::SeqCst);
        true
    }
    fn disable_raw_mode(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
    fn read_input(&mut self, buffer: &mut [u8], _timeout: Duration) -> Option<usize> {
        let mut chunks = self.chunks.lock().unwrap();
        if chunks.is_empty() {
            return None;
        }
        let allowed = if !self.first_taken {
            self.first_taken = true;
            true
        } else {
            match &self.release {
                Some(release) => release(),
                None => true,
            }
        };
        if !allowed {
            return None;
        }
        let chunk = chunks.remove(0);
        let bytes = chunk.as_bytes();
        let count = buffer.len().min(bytes.len());
        buffer[..count].copy_from_slice(&bytes[..count]);
        Some(count)
    }
}

struct Harness {
    terminal: ProcessTerminal,
    writes: Arc<Mutex<String>>,
    /// The scripted input queue: a test may append chunks while the loop runs
    /// (used when the next input depends on state the pump only reaches after
    /// draining an executor report).
    chunks: Arc<Mutex<Vec<String>>>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

fn harness(chunks: Vec<String>, release: Option<Arc<dyn Fn() -> bool + Send + Sync>>) -> Harness {
    harness_with_writes(Arc::new(Mutex::new(String::new())), chunks, release)
}

/// [`harness`] with a caller-owned output buffer, so a release gate can watch
/// the painted frames (the pump's writes land in the same `Arc`).
fn harness_with_writes(
    writes: Arc<Mutex<String>>,
    chunks: Vec<String>,
    release: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Harness {
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let chunks = Arc::new(Mutex::new(chunks));
    let io = ScriptedIo {
        writes: Arc::clone(&writes),
        chunks: Arc::clone(&chunks),
        release,
        first_taken: false,
        started: Arc::clone(&started),
        stopped: Arc::clone(&stopped),
    };
    Harness {
        terminal: ProcessTerminal::with_io(Box::new(io)),
        writes,
        chunks,
        started,
        stopped,
    }
}

fn run_options(agent_dir: PathBuf) -> InteractiveRunOptions {
    InteractiveRunOptions {
        mode: InteractiveModeOptions {
            tui_mode: None,
            clear_on_shrink: Some(true),
            show_terminal_progress: None,
            version: Some("0.0.0-test".to_string()),
            on_terminal_title: None,
            on_terminal_progress: None,
            cwd_git_paths: None,
            ..Default::default()
        },
        transcript: TranscriptSettings::default(),
        markdown_transformers: Vec::new(),
        initial_message: None,
        initial_editor_text: None,
        initial_status: None,
        agent_dir,
    }
}

fn rendered(writes: &Arc<Mutex<String>>) -> String {
    strip_terminal_sequences(&writes.lock().unwrap())
}

#[tokio::test]
async fn typing_a_prompt_runs_it_and_quit_shuts_down() {
    install_dark();
    let session = session(echo_stream("pong"), "prompt");
    // `/quit` is only released once both messages were *painted*: the session
    // state can be ahead of the pump's event drain (the executor runs on its
    // own task), so a state-based gate lets the shutdown win the race and the
    // frames are never rendered.
    let writes = Arc::new(Mutex::new(String::new()));
    let gate = Arc::clone(&writes);
    let release: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        let frame = strip_terminal_sequences(&gate.lock().unwrap());
        frame.contains("hi") && frame.contains("pong")
    });
    let mut harness = harness_with_writes(
        writes,
        vec!["hi\r".to_string(), "/quit\r".to_string()],
        Some(release),
    );

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    assert!(harness.started.load(Ordering::SeqCst), "terminal started");
    assert!(harness.stopped.load(Ordering::SeqCst), "terminal stopped");

    let output = rendered(&harness.writes);
    assert!(output.contains("hi"), "user message rendered: {output:?}");
    assert!(
        output.contains("pong"),
        "assistant message rendered: {output:?}"
    );

    let messages = session.state().messages;
    assert!(
        messages.iter().any(|message| matches!(
            message,
            pillar_agent::types::AgentMessage::Message(Message::User { .. })
        )),
        "user message recorded"
    );
    assert!(
        messages.iter().any(|message| matches!(
            message,
            pillar_agent::types::AgentMessage::Message(Message::Assistant(_))
        )),
        "assistant message recorded"
    );
}

#[tokio::test]
async fn ctrl_d_on_an_empty_editor_shuts_down_without_prompting() {
    install_dark();
    let session = session(echo_stream("pong"), "ctrl-d");
    let mut harness = harness(vec!["\u{4}".to_string()], None);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    assert!(session.state().messages.is_empty(), "no prompt was sent");
}

#[tokio::test]
async fn bash_submission_executes_and_records_the_result() {
    install_dark();
    let session = session(echo_stream("pong"), "bash");
    // Release `/quit` once the bash *message* is in the session: the block
    // header and its output are painted while the command runs (live bash), so
    // a frame-based gate would shut the run down before `executeBash` returned
    // and recorded the result. The completion is drained before the next input
    // read, so the final painted frame still carries the output.
    let state = Arc::clone(&session);
    let release: Arc<dyn Fn() -> bool + Send + Sync> =
        Arc::new(move || {
            state.state().messages.iter().any(|message| {
                matches!(message, pillar_agent::types::AgentMessage::BashExecution(_))
            })
        });
    let mut harness = harness(
        vec!["!printf hello\r".to_string(), "/quit\r".to_string()],
        Some(release),
    );

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    let messages = session.state().messages;
    let bash = messages.iter().find_map(|message| match message {
        pillar_agent::types::AgentMessage::BashExecution(bash) => Some(bash.clone()),
        _ => None,
    });
    let bash = bash.expect("bash execution recorded");
    assert_eq!(bash.command, "printf hello");
    assert_eq!(bash.output, "hello");
    assert_eq!(bash.exit_code, Some(0));

    // The block is rendered live: command header plus captured output.
    let output = rendered(&harness.writes);
    assert!(
        output.contains("printf hello"),
        "the bash block shows the command: {output:?}"
    );
    assert!(
        output.contains("hello"),
        "the bash block shows the output: {output:?}"
    );
}

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

/// A bare Escape reaches the mode: the terminal releases the buffered partial
/// sequence once its disambiguation window passed (upstream the StdinBuffer's
/// own timer), so `tui.select.cancel` closes the selector and the next command
/// goes to the editor.
#[tokio::test]
async fn escape_cancels_the_selector() {
    install_dark();
    let session = session_with_scoped_models(
        echo_stream("pong"),
        "escape",
        vec![scoped_model("claude-sonnet-4-5")],
    );
    let mut harness = harness(vec!["/model\r".to_string(), "\u{1b}".to_string()], None);
    // `/quit` only after the escape window elapsed; before the fix the Escape
    // stayed buffered, so it would be typed into the still-open selector.
    let chunks = Arc::clone(&harness.chunks);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        chunks.lock().unwrap().push("/quit\r".to_string());
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await;
    let output = rendered(&harness.writes);
    let result = match outcome {
        Ok(result) => result.expect("run loop ok"),
        Err(_) => {
            let tail: String = output
                .chars()
                .rev()
                .take(600)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            panic!("the run loop did not finish; the Escape never reached the mode. tail={tail:?}");
        }
    };

    assert_eq!(result, InteractiveOutcome::Exit(0));
    assert_eq!(
        session.state().model.id,
        "claude-sonnet-4-5",
        "the cancelled selector did not switch the model"
    );
}

/// The 2-column picker (`/m`) end to end under the kitty protocol: ←/→ walk
/// the categories, ↑/↓ the models, the release events are ignored (a leaked
/// release would move the highlight twice), the navigation repaints, and Enter
/// switches the session model through the same path as `/model`.
#[tokio::test]
async fn kitty_protocol_arrows_drive_the_model_picker() {
    install_dark();
    let session = session_with_scoped_models(
        echo_stream("pong"),
        "picker",
        vec![
            scoped_model("claude-sonnet-4-5"),
            scoped_model("claude-opus-5"),
        ],
    );
    // One provider category, models sorted by name (opus first): a single ↓
    // highlights sonnet, so the picker's footer line names it. Enter is fed
    // only after that frame was painted, which also proves the repaint of a
    // consumed selector key.
    let mut harness = harness(
        vec![
            "\u{1b}[?7u".to_string(),    // kitty flags reply
            "/m\r".to_string(),          // open the picker
            "\u{1b}[1;1:1C".to_string(), // → next category (wraps: one provider)
            "\u{1b}[1;1:3C".to_string(), // → release
            "\u{1b}[1;1:1B".to_string(), // ↓ to sonnet
            "\u{1b}[1;1:3B".to_string(), // ↓ release
        ],
        None,
    );
    let chunks = Arc::clone(&harness.chunks);
    let writes = Arc::clone(&harness.writes);
    let state = Arc::clone(&session);
    std::thread::spawn(move || {
        for _ in 0..600 {
            if strip_terminal_sequences(&writes.lock().unwrap())
                .contains("anthropic/claude-sonnet-4-5 · ctx")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        chunks.lock().unwrap().push("\r".to_string());
        for _ in 0..600 {
            if state.state().model.id == "claude-sonnet-4-5" {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        chunks.lock().unwrap().push("/quit\r".to_string());
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    assert_eq!(
        session.state().model.id,
        "claude-sonnet-4-5",
        "one row down: the release event must not move the highlight again"
    );
    let output = rendered(&harness.writes);
    assert!(output.contains("PROVIDERS"), "picker rendered: {output:?}");
    assert!(
        output.contains("←→ category"),
        "picker hint rendered: {output:?}"
    );
    assert!(
        output.contains("anthropic/claude-sonnet-4-5 · ctx"),
        "the ↓ repainted the highlighted model's footer: {output:?}"
    );
    assert!(
        output.contains("Model: claude-sonnet-4-5"),
        "the status reports the new model: {output:?}"
    );
}

/// The autocomplete wiring end to end: typing a slash command opens the menu,
/// Tab completes it, and Enter on a completed `/command` falls through to
/// submit (here `/quit`, which shuts the loop down).
#[tokio::test]
async fn typing_a_slash_command_completes_it_and_enter_submits() {
    install_dark();
    let session = session(echo_stream("pong"), "autocomplete");
    // `/thi` + Tab completes to `/thinking `; Enter falls through to submit it,
    // which opens the thinking selector; Ctrl+C cancels the selector (a single
    // byte, so the harness needs no escape-flush gap); `/quit` then completes
    // and submits the same way.
    let mut harness = harness(
        vec![
            "/thi".to_string(),
            "\t".to_string(),
            "\r".to_string(),
            "\u{3}".to_string(),
            "/quit\r".to_string(),
        ],
        None,
    );

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    let output = rendered(&harness.writes);
    assert!(
        output.contains("thinking"),
        "the menu listed the command and Tab completed it: {output:?}"
    );
    assert!(
        output.contains("Thinking Level"),
        "the completed command ran: {output:?}"
    );
    assert!(
        session.state().messages.is_empty(),
        "nothing was submitted as a prompt"
    );
}

/// `/scoped-models` end to end: the selector renders in the editor slot, a
/// kitty ↓ + Enter toggle applies to the session's cycle scope and repaints
/// (the release event must not move the highlight again), Escape closes, and
/// the last frame returns to the editor before `/quit`.
#[tokio::test]
async fn scoped_models_selector_toggles_the_scope_and_closes() {
    install_dark();
    let session = session_with_scoped_models(
        echo_stream("pong"),
        "scoped-models",
        vec![
            scoped_model("claude-sonnet-4-5"),
            scoped_model("claude-opus-5"),
        ],
    );
    // The selector's enable-state and scope resolution read the runtime's
    // availability snapshot; refresh it (the fixture's runtime has the
    // anthropic key) like the startup path does.
    session
        .model_runtime()
        .refresh_availability(None)
        .await
        .expect("availability");

    let mut harness = harness(vec!["/scoped-models\r".to_string()], None);
    let chunks = Arc::clone(&harness.chunks);
    let writes = Arc::clone(&harness.writes);
    std::thread::spawn(move || {
        // Wait for the selector frame, then ↓ (press + release) → opus.
        for _ in 0..600 {
            if strip_terminal_sequences(&writes.lock().unwrap()).contains("Model Configuration") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        chunks.lock().unwrap().push("\u{1b}[1;1:1B".to_string());
        for _ in 0..600 {
            if strip_terminal_sequences(&writes.lock().unwrap())
                .contains("Model Name: Claude Opus 5")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        chunks.lock().unwrap().push("\u{1b}[1;1:3B".to_string());
        std::thread::sleep(Duration::from_millis(50));
        // Enter toggles opus off (the footer repaints to 1 enabled)…
        chunks.lock().unwrap().push("\r".to_string());
        for _ in 0..600 {
            if strip_terminal_sequences(&writes.lock().unwrap()).contains("1/14 enabled") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        // …and Escape closes the selector.
        chunks.lock().unwrap().push("\u{1b}".to_string());
        std::thread::sleep(Duration::from_millis(100));
        chunks.lock().unwrap().push("/quit\r".to_string());
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("scoped-models-run")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    assert_eq!(
        session
            .scoped_models()
            .iter()
            .map(|scoped| scoped.model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-sonnet-4-5"],
        "the toggle dropped opus from the cycle scope"
    );
    let output = rendered(&harness.writes);
    assert!(
        output.contains("Model Configuration"),
        "selector rendered: {output:?}"
    );
    assert!(
        output.contains("Session-only"),
        "the session-only hint rendered: {output:?}"
    );
    assert!(
        output.contains("2/14 enabled"),
        "both scoped models start enabled: {output:?}"
    );
    assert!(
        output.contains("1/14 enabled"),
        "the toggle repainted the count: {output:?}"
    );
}

/// `/resume` end to end: the selector renders in the editor slot with the
/// loading header, the empty current folder falls through to the hint, Tab
/// switches the scope (the load runs on the executor), and Escape closes
/// without resuming anything.
#[tokio::test]
async fn resume_opens_the_session_selector_and_escape_cancels() {
    install_dark();
    let session = session(echo_stream("pong"), "resume");
    let mut harness = harness(vec!["/resume\r".to_string()], None);
    let chunks = Arc::clone(&harness.chunks);
    let writes = Arc::clone(&harness.writes);
    std::thread::spawn(move || {
        // Wait for the selector frame, then Escape closes it.
        for _ in 0..600 {
            if strip_terminal_sequences(&writes.lock().unwrap())
                .contains("Resume Session (Current Folder)")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        chunks.lock().unwrap().push("\u{1b}".to_string());
        std::thread::sleep(Duration::from_millis(100));
        chunks.lock().unwrap().push("/quit\r".to_string());
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("resume-run")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    let output = rendered(&harness.writes);
    assert!(
        output.contains("Resume Session (Curr"),
        "selector rendered: {output:?}"
    );
    assert!(
        output.contains("re:<pattern> regex"),
        "the search hints rendered: {output:?}"
    );
    assert!(
        output.contains("No sessions in current folder. Press Tab to view all."),
        "the empty current folder shows the hint: {output:?}"
    );
    // The loaded (empty) list leaves the loading state.
    assert!(output.contains("◉ Current Folder"), "{output:?}");
    // Escape cancelled; nothing was resumed.
    assert!(
        !output.contains("Resumed session"),
        "Esc cancelled the selector: {output:?}"
    );
}

/// The session tree end to end: `/tree` paints the tree, the arrows move the
/// highlight, Enter opens the "Summarize branch?" dialog, choosing "No
/// summary" navigates (rebuilding the transcript) and `/quit` exits.
#[tokio::test]
async fn tree_navigation_rebuilds_the_transcript_in_the_run_loop() {
    install_dark();
    let session = session(echo_stream("pong"), "tree");
    let ids: Vec<String> = {
        let mut sm = session.session_manager().lock().unwrap();
        let mut ids = Vec::new();
        for (text, is_user) in [
            ("first question", true),
            ("first answer", false),
            ("second question", true),
            ("second answer", false),
        ] {
            let message = if is_user {
                pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::User {
                    content: pillar_ai::types::UserContent::Text(text.to_string()),
                    timestamp: 1,
                })
            } else {
                pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::Assistant(
                    Box::new(assistant_message(text)),
                ))
            };
            let id = sm.append_message(message).expect("append");
            ids.push(id);
        }
        ids
    };
    let mut harness = harness(vec!["/tree\r".to_string()], None);
    let chunks = Arc::clone(&harness.chunks);
    let writes = Arc::clone(&harness.writes);
    std::thread::spawn(move || {
        let wait_for = |needle: &str| {
            for _ in 0..800 {
                if strip_terminal_sequences(&writes.lock().unwrap()).contains(needle) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };
        wait_for("Session Tree");
        // Up twice (the constructor selects the current leaf, the last entry)
        // to the assistant entry, then Enter opens the dialog. An assistant
        // target leaves the editor empty, so `/quit` below is not appended to
        // restored editor text.
        chunks
            .lock()
            .unwrap()
            .push("\u{1b}[A\u{1b}[A\r".to_string());
        wait_for("Summarize branch?");
        chunks.lock().unwrap().push("\r".to_string());
        wait_for("Navigated to selected point");
        std::thread::sleep(Duration::from_millis(50));
        chunks.lock().unwrap().push("/quit\r".to_string());
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    assert_eq!(result, InteractiveOutcome::Exit(0));
    let output = rendered(&harness.writes);
    assert!(output.contains("Session Tree"), "tree rendered: {output:?}");
    assert!(
        output.contains("Summarize branch?"),
        "summary dialog rendered: {output:?}"
    );
    assert!(
        output.contains("Navigated to selected point"),
        "navigation status rendered: {output:?}"
    );
    assert!(
        output.contains("first answer"),
        "kept branch rendered: {output:?}"
    );
    // The rebuilt context holds only the kept branch (the last painted frame
    // after the status shows it; earlier frames still carry the old branch).
    let context_text: String = session
        .state()
        .messages
        .iter()
        .map(|message| match message {
            pillar_agent::types::AgentMessage::Message(Message::User { content, .. }) => {
                match content {
                    pillar_ai::types::UserContent::Text(text) => text.clone(),
                    pillar_ai::types::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            Content::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .collect(),
                }
            }
            pillar_agent::types::AgentMessage::Message(Message::Assistant(assistant)) => {
                pillar_ai::text::content_text(&assistant.content, "")
            }
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(context_text.contains("first answer"), "{context_text:?}");
    assert!(!context_text.contains("second"), "{context_text:?}");
    // The leaf moved to the selected assistant entry.
    assert_eq!(session.get_leaf_id().as_deref(), Some(ids[1].as_str()));
}

/// `/fork` end to end: the selector paints, Enter returns the fork outcome
/// (the caller rebuilds the runtime as a branched session).
#[tokio::test]
async fn fork_command_returns_the_fork_outcome_from_the_run_loop() {
    install_dark();
    let session = session(echo_stream("pong"), "fork");
    let ids: Vec<String> = {
        let mut sm = session.session_manager().lock().unwrap();
        let mut ids = Vec::new();
        for (text, is_user) in [
            ("first question", true),
            ("first answer", false),
            ("second question", true),
            ("second answer", false),
        ] {
            let message = if is_user {
                pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::User {
                    content: pillar_ai::types::UserContent::Text(text.to_string()),
                    timestamp: 1,
                })
            } else {
                pillar_coding_agent::core::messages::CodingAgentMessage::Base(Message::Assistant(
                    Box::new(assistant_message(text)),
                ))
            };
            ids.push(sm.append_message(message).expect("append"));
        }
        ids
    };
    let mut harness = harness(vec!["/fork\r".to_string()], None);
    let chunks = Arc::clone(&harness.chunks);
    let writes = Arc::clone(&harness.writes);
    std::thread::spawn(move || {
        for _ in 0..800 {
            if strip_terminal_sequences(&writes.lock().unwrap()).contains("Fork from Message") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        chunks.lock().unwrap().push("\r".to_string());
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_interactive(
            Arc::clone(&session),
            Box::new(std::mem::replace(
                &mut harness.terminal,
                ProcessTerminal::with_io(Box::new(pillar_tui::process_terminal::NullTerminalIo)),
            )),
            run_options(temp_dir("keybindings")),
        ),
    )
    .await
    .expect("run loop finished")
    .expect("run loop ok");

    // The default selection is the most recent user message.
    assert_eq!(
        result,
        InteractiveOutcome::ForkSession {
            entry_id: ids[2].clone(),
            position: "before".to_string(),
            editor_text: Some("second question".to_string()),
        }
    );
    let output = rendered(&harness.writes);
    assert!(
        output.contains("Fork from Message"),
        "selector rendered: {output:?}"
    );
}
