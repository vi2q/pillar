//! Parity tests for the host-driven interactive run loop (pi v0.84.3
//! `interactive-mode.ts` `init` / `run` plus the terminal pump): mounting the
//! mode into the renderer, the editor key path into `handle_submit`, the
//! `ModeAction` execution, and the shutdown handshake.
//!
//! The scripted terminal goes through the real `ProcessTerminal` over injected
//! `TerminalIo`, so `StdinBuffer` splits the raw bytes like upstream.

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
    InteractiveRunOptions, run_interactive,
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
    std::fs::write(&models_path, "{}").unwrap();
    ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        credentials: Some(credentials),
        ..Default::default()
    })
    .unwrap()
}

fn session(stream_fn: pillar_agent::StreamFn, label: &str) -> Arc<AgentSession> {
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
        serde_json::json!({}),
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
    Arc::new(AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        temp_dir(label).to_string_lossy().to_string(),
        resource_loader,
        Arc::new(runtime_with_anthropic_key(label)),
        Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
    )))
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
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

fn harness(chunks: Vec<String>, release: Option<Arc<dyn Fn() -> bool + Send + Sync>>) -> Harness {
    let writes = Arc::new(Mutex::new(String::new()));
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let io = ScriptedIo {
        writes: Arc::clone(&writes),
        chunks: Arc::new(Mutex::new(chunks)),
        release,
        first_taken: false,
        started: Arc::clone(&started),
        stopped: Arc::clone(&stopped),
    };
    Harness {
        terminal: ProcessTerminal::with_io(Box::new(io)),
        writes,
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
        },
        transcript: TranscriptSettings::default(),
        markdown_transformers: Vec::new(),
        initial_message: None,
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
    // `/quit` is only released once the assistant message exists, so the
    // prompt has been executed before the shutdown request.
    let state = Arc::clone(&session);
    let release: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        state
            .state()
            .messages
            .iter()
            .any(|message| matches!(message, pillar_agent::types::AgentMessage::Message(Message::Assistant(_))))
    });
    let mut harness = harness(vec!["hi\r".to_string(), "/quit\r".to_string()], Some(release));

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

    assert_eq!(result, 0);
    assert!(harness.started.load(Ordering::SeqCst), "terminal started");
    assert!(harness.stopped.load(Ordering::SeqCst), "terminal stopped");

    let output = rendered(&harness.writes);
    assert!(output.contains("hi"), "user message rendered: {output:?}");
    assert!(output.contains("pong"), "assistant message rendered: {output:?}");

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

    assert_eq!(result, 0);
    assert!(session.state().messages.is_empty(), "no prompt was sent");
}

#[tokio::test]
async fn bash_submission_executes_and_records_the_result() {
    install_dark();
    let session = session(echo_stream("pong"), "bash");
    let state = Arc::clone(&session);
    let release: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        state.state().messages.iter().any(|message| {
            matches!(
                message,
                pillar_agent::types::AgentMessage::BashExecution(_)
            )
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

    assert_eq!(result, 0);
    let messages = session.state().messages;
    let bash = messages.iter().find_map(|message| match message {
        pillar_agent::types::AgentMessage::BashExecution(bash) => Some(bash.clone()),
        _ => None,
    });
    let bash = bash.expect("bash execution recorded");
    assert_eq!(bash.command, "printf hello");
    assert_eq!(bash.output, "hello");
    assert_eq!(bash.exit_code, Some(0));
}
