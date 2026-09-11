//! Parity tests for the AgentSession streaming loop (upstream
//! test/agent-session-retry.test.ts scenarios plus streaming queueing):
//! prompt -> post-run loop, auto-retry with backoff, steer/followUp
//! queueing while streaming, queue_update events, session persistence on
//! message_end, auth validation, and clearQueue.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_agent::{Agent, AgentOptions, AgentState, AgentThinkingLevel, FauxModelRef};
use pillar_ai::auth_types::{ApiKeyCredential, Credential, CredentialInfo, CredentialStore};
use pillar_ai::error::AiError;
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Model, ModelCost, ModelCostRates, StopReason,
    Usage, UsageCost, UserContent,
};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, BeforeSessionStartFn, ExtensionBindings,
    ExtensionRunnerFactory, SessionEventMeta, SystemPromptRebuildFn,
};
use pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore;
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

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

fn assistant_message(text: &str, stop_reason: StopReason, error: Option<&str>) -> AssistantMessage {
    AssistantMessage {
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![Content::text(text)]
        },
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        response_model: None,
        usage: create_usage(),
        stop_reason,
        deferred: None,
        error_message: error.map(str::to_string),
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// Mock credential store fixture.
#[derive(Default)]
struct MemCredentials(std::sync::Mutex<BTreeMap<String, Credential>>);

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

fn temp_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-loop-{}-{id}-{name}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// ModelRuntime with a seeded API-key credential for "anthropic".
fn runtime_with_anthropic_key() -> (ModelRuntime, Arc<MemCredentials>) {
    let credentials = Arc::new(MemCredentials::default());
    credentials.0.lock().unwrap().insert(
        "anthropic".to_string(),
        Credential::ApiKey(ApiKeyCredential {
            key: Some("test-key".to_string()),
            env: None,
        }),
    );
    let dir = temp_dir("runtime");
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").unwrap();
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        credentials: Some(credentials.clone()),
        ..Default::default()
    })
    .unwrap();
    (runtime, credentials)
}

/// ModelRuntime with no credentials for "anthropic".
fn runtime_without_auth() -> ModelRuntime {
    let dir = temp_dir("runtime-noauth");
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").unwrap();
    ModelRuntime::new(CreateModelRuntimeOptions {
        models_path: Some(models_path),
        models_store: Some(Arc::new(InMemoryCodingAgentModelsStore::new())),
        ..Default::default()
    })
    .unwrap()
}

/// Stream fn that fails (error message) the first `fail_limit` calls and
/// succeeds afterwards. `call_count` observes the number of LLM calls.
fn fail_then_succeed_stream(fail_limit: u32, call_count: Arc<AtomicU32>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| {
        let call_count = Arc::clone(&call_count);
        async move {
            let call = call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let stream = assistant_message_event_stream();
            if call <= fail_limit {
                let error = assistant_message("", StopReason::Error, Some("overloaded_error"));
                stream.push(AssistantMessageEvent::Start {
                    partial: error.clone(),
                });
                stream.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error,
                });
            } else {
                let message = assistant_message("Success", StopReason::Stop, None);
                stream.push(AssistantMessageEvent::Start {
                    partial: message.clone(),
                });
                stream.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message,
                });
            }
            stream
        }
    })
}

/// Stream fn that signals `started` on its first invocation and then waits
/// on `release` (or the run's abort signal) before producing a success
/// message. Later invocations succeed immediately.
fn gated_success_stream(
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> pillar_agent::StreamFn {
    let first_call = Arc::new(std::sync::atomic::AtomicBool::new(true));
    pillar_agent::StreamFn::new(move |_context, options| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let first_call = Arc::clone(&first_call);
        let signal = options.as_ref().and_then(|o| o.abort.clone());
        async move {
            if first_call.swap(false, Ordering::SeqCst) {
                started.notify_one();
                match signal {
                    Some(signal) => {
                        tokio::select! {
                            _ = release.notified() => {}
                            _ = signal.aborted() => {}
                        }
                    }
                    None => release.notified().await,
                }
            }
            let stream = assistant_message_event_stream();
            let message = assistant_message("Done", StopReason::Stop, None);
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message,
            });
            stream
        }
    })
}

fn make_session(
    stream_fn: pillar_agent::StreamFn,
    settings_extra: serde_json::Value,
) -> (Arc<AgentSession>, Arc<AtomicU32>) {
    make_session_with_model(stream_fn, settings_extra, mock_model())
}

fn make_session_with_model(
    stream_fn: pillar_agent::StreamFn,
    settings_extra: serde_json::Value,
    model: FauxModelRef,
) -> (Arc<AgentSession>, Arc<AtomicU32>) {
    make_session_with_model_and_runner(
        stream_fn,
        settings_extra,
        model,
        Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
    )
}

fn make_session_with_model_and_runner(
    stream_fn: pillar_agent::StreamFn,
    settings_extra: serde_json::Value,
    model: FauxModelRef,
    extension_runner: Arc<Mutex<ExtensionRunner>>,
) -> (Arc<AgentSession>, Arc<AtomicU32>) {
    let call_count = Arc::new(AtomicU32::new(0));
    let mut options = AgentOptions::new(stream_fn);
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model,
        thinking_level: AgentThinkingLevel::Off,
        tools: Vec::new(),
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));

    let (runtime, _credentials) = runtime_with_anthropic_key();
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let mut settings = SettingsManager::in_memory(
        serde_json::json!({
            "retry": { "enabled": true, "maxRetries": 3, "baseDelayMs": 1 },
        }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    );
    settings.apply_overrides(&settings_extra);
    let settings_manager = Arc::new(Mutex::new(settings));

    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("agent-dir").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));

    let session = AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime),
        extension_runner,
    ));
    (Arc::new(session), call_count)
}

// ============================================================================
// Retry (upstream test/agent-session-retry.test.ts)
// ============================================================================

#[tokio::test]
async fn retries_after_transient_error_and_succeeds() {
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session(
        fail_then_succeed_stream(1, Arc::clone(&call_count)),
        serde_json::json!({}),
    );

    let retry_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let retry_events = Arc::clone(&retry_events);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::AutoRetryStart { attempt, .. } = event {
                retry_events
                    .lock()
                    .unwrap()
                    .push(format!("start:{attempt}"));
            }
            if let AgentSessionEvent::AutoRetryEnd { success, .. } = event {
                retry_events
                    .lock()
                    .unwrap()
                    .push(format!("end:success={success}"));
            }
        })
    };
    let _unsub = session.subscribe(listener);

    session.prompt("Test", None).await.expect("prompt succeeds");

    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        *retry_events.lock().unwrap(),
        vec!["start:1".to_string(), "end:success=true".to_string()]
    );
    assert!(!session.is_retrying());
    assert_eq!(session.retry_attempt(), 0);
}

#[tokio::test]
async fn exhausts_max_retries_and_emits_failure() {
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session(
        fail_then_succeed_stream(99, Arc::clone(&call_count)),
        serde_json::json!({ "retry": { "maxRetries": 2 } }),
    );
    let retry_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let retry_events = Arc::clone(&retry_events);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::AutoRetryStart { attempt, .. } = event {
                retry_events
                    .lock()
                    .unwrap()
                    .push(format!("start:{attempt}"));
            }
            if let AgentSessionEvent::AutoRetryEnd {
                success, attempt, ..
            } = event
            {
                retry_events
                    .lock()
                    .unwrap()
                    .push(format!("end:success={success}:attempt={attempt}"));
            }
        })
    };
    let _unsub = session.subscribe(listener);

    session
        .prompt("Test", None)
        .await
        .expect("prompt completes");

    // 1 initial call + 2 retries.
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
    let events = retry_events.lock().unwrap().clone();
    assert!(events.contains(&"start:1".to_string()));
    assert!(events.contains(&"start:2".to_string()));
    assert!(events.iter().any(|e| e.starts_with("end:success=false")));
    assert!(!session.is_retrying());
    assert_eq!(session.retry_attempt(), 0);
}

#[tokio::test]
async fn retry_disabled_does_not_retry() {
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session(
        fail_then_succeed_stream(5, Arc::clone(&call_count)),
        serde_json::json!({ "retry": { "enabled": false } }),
    );
    session
        .prompt("Test", None)
        .await
        .expect("prompt completes");
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(!session.is_retrying());
}

#[tokio::test]
async fn resets_retry_counter_after_successful_response() {
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session(
        fail_then_succeed_stream(1, Arc::clone(&call_count)),
        serde_json::json!({ "retry": { "maxRetries": 3 } }),
    );
    session.prompt("Test", None).await.expect("prompt succeeds");
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    assert_eq!(session.retry_attempt(), 0);
    // A second prompt after success keeps the counter at 0.
    session
        .prompt("Test again", None)
        .await
        .expect("second prompt");
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
    assert_eq!(session.retry_attempt(), 0);
}

// ============================================================================
// Streaming queueing (upstream steer/followUp during streaming)
// ============================================================================

#[tokio::test]
async fn steer_queues_during_streaming_and_is_delivered() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (session, _) = make_session(
        gated_success_stream(Arc::clone(&started), Arc::clone(&release)),
        serde_json::json!({}),
    );

    type QueueSnapshots = Vec<(Vec<String>, Vec<String>)>;
    let queue_updates: Arc<Mutex<QueueSnapshots>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let queue_updates = Arc::clone(&queue_updates);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } = event
            {
                queue_updates
                    .lock()
                    .unwrap()
                    .push((steering.clone(), follow_up.clone()));
            }
        })
    };
    let _unsub = session.subscribe(listener);

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("First", None).await })
    };

    // Wait until the agent is streaming, then steer.
    started.notified().await;
    assert!(session.is_streaming());
    session.steer("Steer me", None).await.expect("steer queues");
    assert_eq!(
        session.get_steering_messages(),
        vec!["Steer me".to_string()]
    );

    // Release the run; the steered message is delivered via continue_run.
    release.notify_one();
    run.await.expect("run task").expect("prompt succeeds");

    assert!(session.get_steering_messages().is_empty());
    assert_eq!(session.pending_message_count(), 0);
    assert!(session.is_idle());

    // queue_update was emitted with the pending message, then cleared.
    let updates = queue_updates.lock().unwrap().clone();
    assert!(
        updates
            .iter()
            .any(|(steering, _)| steering == &vec!["Steer me".to_string()]),
        "{updates:?}"
    );
    assert!(updates.last().unwrap().0.is_empty(), "{updates:?}");

    // The steered message landed in agent state as a user message.
    let messages = session.state().messages;
    assert!(
        messages.iter().any(|m| {
            matches!(
                m,
                pillar_agent::AgentMessage::Message(ai_types::Message::User { content, .. })
                    if user_content_text(content) == "Steer me"
            )
        }),
        "{messages:?}"
    );
}

mod ai_types {
    pub use pillar_ai::types::*;
}

/// Extract the text of a user-content value (Blocks or Text).
fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => pillar_ai::text::content_text(blocks, ""),
    }
}

#[tokio::test]
async fn follow_up_queues_during_streaming_and_is_delivered() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (session, _) = make_session(
        gated_success_stream(Arc::clone(&started), Arc::clone(&release)),
        serde_json::json!({}),
    );

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("First", None).await })
    };

    started.notified().await;
    session
        .follow_up("Follow me", None)
        .await
        .expect("follow_up queues");
    assert_eq!(
        session.get_follow_up_messages(),
        vec!["Follow me".to_string()]
    );

    release.notify_one();
    run.await.expect("run task").expect("prompt succeeds");

    assert!(session.get_follow_up_messages().is_empty());
    assert!(session.is_idle());
}

#[tokio::test]
async fn prompt_while_streaming_without_behavior_errors() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (session, _) = make_session(
        gated_success_stream(Arc::clone(&started), Arc::clone(&release)),
        serde_json::json!({}),
    );

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("First", None).await })
    };

    started.notified().await;
    let error = session.prompt("Second", None).await.unwrap_err();
    assert!(error.contains("Agent is already processing"), "{error}");
    assert!(session.get_steering_messages().is_empty());
    assert!(session.get_follow_up_messages().is_empty());

    release.notify_one();
    run.await.expect("run task").expect("prompt succeeds");
}

#[tokio::test]
async fn clear_queue_returns_and_clears_pending_messages() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (session, _) = make_session(
        gated_success_stream(Arc::clone(&started), Arc::clone(&release)),
        serde_json::json!({}),
    );

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("First", None).await })
    };

    started.notified().await;
    session.steer("Steer me", None).await.unwrap();
    session.follow_up("Follow me", None).await.unwrap();
    assert_eq!(session.pending_message_count(), 2);

    let (steering, follow_up) = session.clear_queue();
    assert_eq!(steering, vec!["Steer me".to_string()]);
    assert_eq!(follow_up, vec!["Follow me".to_string()]);
    assert_eq!(session.pending_message_count(), 0);

    release.notify_one();
    run.await.expect("run task").expect("prompt succeeds");
}

// ============================================================================
// Persistence and auth (upstream message_end persistence / auth checks)
// ============================================================================

#[tokio::test]
async fn message_end_persists_user_and_assistant_to_session() {
    let (session, _) = make_session(
        fail_then_succeed_stream(0, Arc::new(AtomicU32::new(0))),
        serde_json::json!({}),
    );
    session
        .prompt("Persist me", None)
        .await
        .expect("prompt succeeds");

    let sm = session.session_manager().lock().unwrap();
    let branch = sm.get_branch(None);
    // user entry + assistant entry.
    assert_eq!(branch.len(), 2, "{branch:?}");
    assert!(matches!(
        branch[0],
        session_entry::SessionEntry::Message(entry)
            if matches!(entry.message, pillar_coding_agent::core::messages::CodingAgentMessage::Base(pillar_ai::types::Message::User { .. }))
    ));
    assert!(matches!(
        branch[1],
        session_entry::SessionEntry::Message(entry)
            if matches!(entry.message, pillar_coding_agent::core::messages::CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(_)))
    ));
}

mod session_entry {
    pub use pillar_coding_agent::core::session_entries::*;
}

#[tokio::test]
async fn prompt_fails_when_no_api_key_is_configured() {
    let call_count = Arc::new(AtomicU32::new(0));
    let mut options = AgentOptions::new(fail_then_succeed_stream(0, Arc::clone(&call_count)));
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
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let settings = SettingsManager::in_memory(
        serde_json::json!({}),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    );
    let settings_manager = Arc::new(Mutex::new(settings));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("agent-dir").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));

    let session = AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime_without_auth()),
        Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
    ));

    let error = session.prompt("Test", None).await.unwrap_err();
    assert!(
        error.starts_with("No API key found for anthropic."),
        "{error}"
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn abort_waits_for_idle() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (session, _) = make_session(
        gated_success_stream(Arc::clone(&started), Arc::clone(&release)),
        serde_json::json!({}),
    );

    let run = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.prompt("First", None).await })
    };

    started.notified().await;
    assert!(session.is_streaming());
    // Abort: the agent run is interrupted and the session settles idle.
    session.abort().await;
    assert!(session.is_idle());
    release.notify_one();
    let _ = run.await;
}

// ============================================================================
// Automatic threshold compaction (upstream _checkCompaction -> _runAutoCompaction)
// ============================================================================

/// Stream fn that answers normal prompts with "Done" and summarization
/// prompts (starting with `<conversation>`) with a compaction summary.
fn threshold_compaction_stream() -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |context, _options| async move {
        let is_summarization = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.to_lowercase().contains("summariz"));
        let stream = assistant_message_event_stream();
        if is_summarization {
            let message = assistant_message("Compacted summary.", StopReason::Stop, None);
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message,
            });
        } else {
            let message = assistant_message("Done", StopReason::Stop, None);
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message,
            });
        }
        stream
    })
}

#[tokio::test]
async fn threshold_compaction_runs_and_rebuilds_context() {
    let mut model = mock_model();
    model.context_window = 200;
    let (session, _) = make_session_with_model(
        threshold_compaction_stream(),
        serde_json::json!({
            "compaction": { "enabled": true, "reserveTokens": 50, "keepRecentTokens": 10 },
            "retry": { "enabled": false },
        }),
        model,
    );

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let events = Arc::clone(&events);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::CompactionStart { reason } = event {
                events.lock().unwrap().push(format!("start:{reason}"));
            }
            if let AgentSessionEvent::CompactionEnd { reason, .. } = event {
                events.lock().unwrap().push(format!("end:{reason}"));
            }
        })
    };
    let _unsub = session.subscribe(listener);

    // Seed prior history so the cut point lands after it: the summarizer
    // needs a non-empty prefix to compact.
    {
        let mut sm = session.session_manager().lock().unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::User {
            content: UserContent::Text("Earlier user turn".to_string()),
            timestamp: 1,
        }))
        .unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::Assistant(
            Box::new(assistant_message("Earlier reply", StopReason::Stop, None)),
        )))
        .unwrap();
    }

    // A large prompt pushes the estimated context over the (small) window
    // so the post-run threshold check compacts.
    let big_prompt = "word ".repeat(400);
    session
        .prompt(&big_prompt, None)
        .await
        .expect("prompt succeeds");

    let events = events.lock().unwrap().clone();
    assert!(
        events.contains(&"start:threshold".to_string()),
        "{events:?}"
    );
    assert!(events.contains(&"end:threshold".to_string()), "{events:?}");

    // A compaction entry was appended with the summarizer's output.
    let sm = session.session_manager().lock().unwrap();
    let branch = sm.get_branch(None);
    assert!(
        branch.iter().any(|e| {
            matches!(
                e,
                session_entry::SessionEntry::Compaction(c) if c.summary == "Compacted summary."
            )
        }),
        "{branch:?}"
    );

    // The compacted context replaced the summarized seed with the
    // compaction summary; the recent big prompt is kept.
    let messages = session.state().messages;
    let all_text: String = messages
        .iter()
        .map(|m| match m {
            pillar_agent::AgentMessage::Message(ai_types::Message::User { content, .. }) => {
                user_content_text(content)
            }
            pillar_agent::AgentMessage::Message(ai_types::Message::Assistant(a)) => {
                pillar_ai::text::content_text(&a.content, "")
            }
            _ => String::new(),
        })
        .collect();
    assert!(
        !all_text.contains("Earlier user turn"),
        "summarized seed removed from context: {all_text}"
    );
    // The agent state reflects the post-compaction context exactly: the
    // compaction summary message replaces the summarized seed, while the
    // recent big prompt is kept.
    let state_messages = session.state().messages;
    assert!(
        state_messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::CompactionSummary(summary)
                if summary.summary == "Compacted summary."
        )),
        "compaction summary message in agent state: {state_messages:?}"
    );
    assert!(
        state_messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::Message(ai_types::Message::User { content, .. })
                if user_content_text(content).contains("word word")
        )),
        "recent user message kept after compaction"
    );
}

// ============================================================================
// Between-turn refresh (upstream _installAgentNextTurnRefresh)
// ============================================================================

/// Session harness with explicit tools and an optional pre-construction
/// `AgentOptions` mutator (used to install a prior prepare-next-turn hook).
fn make_session_with_tools(
    stream_fn: pillar_agent::StreamFn,
    settings_extra: serde_json::Value,
    model: FauxModelRef,
    tools: Vec<pillar_agent::AgentTool>,
    configure: impl FnOnce(&mut AgentOptions),
) -> (Arc<AgentSession>, Arc<AtomicU32>) {
    let call_count = Arc::new(AtomicU32::new(0));
    let mut options = AgentOptions::new(stream_fn);
    configure(&mut options);
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model,
        thinking_level: AgentThinkingLevel::Off,
        tools,
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));

    let (runtime, _credentials) = runtime_with_anthropic_key();
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let mut settings = SettingsManager::in_memory(
        serde_json::json!({ "retry": { "enabled": false } }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    );
    settings.apply_overrides(&settings_extra);
    let settings_manager = Arc::new(Mutex::new(settings));

    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("agent-dir").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));

    let session = AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime),
        Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
    ));
    (Arc::new(session), call_count)
}

/// Stream fn: call 1 requests `noop`, summarization prompts return a
/// compaction summary, and later calls succeed. `call_count` counts
/// provider turns (summarization calls excluded).
fn tool_then_summarize_stream(call_count: Arc<AtomicU32>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |context, _options| {
        let call_count = Arc::clone(&call_count);
        async move {
            let is_summarization = context
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.to_lowercase().contains("summariz"));
            let message = if is_summarization {
                assistant_message("Compacted summary.", StopReason::Stop, None)
            } else {
                let call = call_count.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    let mut message = assistant_message("", StopReason::ToolUse, None);
                    message.content =
                        vec![Content::tool_call("call-1", "noop", serde_json::json!({}))];
                    message
                } else {
                    assistant_message("Done", StopReason::Stop, None)
                }
            };
            let stream = assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        }
    })
}

#[tokio::test]
async fn between_turn_threshold_compaction_runs_mid_run() {
    let mut model = mock_model();
    model.context_window = 200;
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session_with_tools(
        tool_then_summarize_stream(Arc::clone(&call_count)),
        serde_json::json!({
            "compaction": { "enabled": true, "reserveTokens": 50, "keepRecentTokens": 10 },
        }),
        model,
        vec![noop_tool()],
        |_| {},
    );

    // Seed a prior history so between-turn compaction has a prefix to cut.
    {
        let mut sm = session.session_manager().lock().unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::User {
            content: UserContent::Text("Earlier user turn".to_string()),
            timestamp: 1,
        }))
        .unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::Assistant(
            Box::new(assistant_message("Earlier reply", StopReason::Stop, None)),
        )))
        .unwrap();
    }

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let events = Arc::clone(&events);
        Arc::new(move |event: &AgentSessionEvent| match event {
            AgentSessionEvent::CompactionStart { reason } => {
                events.lock().unwrap().push(format!("start:{reason}"));
            }
            AgentSessionEvent::CompactionEnd { reason, .. } => {
                events.lock().unwrap().push(format!("end:{reason}"));
            }
            _ => {}
        })
    };
    let _unsub = session.subscribe(listener);

    // Turn 1 requests a tool; the large prompt plus tool result push the
    // agent context over the window, so the between-turn hook compacts
    // before turn 2.
    let big_prompt = format!("{}Use a tool", "word ".repeat(400));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.prompt(&big_prompt, None),
    )
    .await
    .expect("prompt completes")
    .expect("prompt succeeds");

    let events = events.lock().unwrap().clone();
    assert!(
        events.contains(&"start:threshold".to_string()),
        "{events:?}"
    );
    assert!(events.contains(&"end:threshold".to_string()), "{events:?}");

    // Two provider turns plus the summarization call.
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    // The summary replaced the summarized prefix in agent state.
    let state_messages = session.state().messages;
    assert!(
        state_messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::CompactionSummary(summary)
                if summary.summary == "Compacted summary."
        )),
        "compaction summary message in agent state: {state_messages:?}"
    );
    assert!(
        !state_messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::Message(ai_types::Message::User { content, .. })
                if user_content_text(content).contains("Earlier user turn")
        )),
        "summarized prefix dropped from agent state"
    );
}

/// Captured `(system prompt, user texts)` per provider request.
type CapturedContexts = Arc<Mutex<Vec<(Option<String>, Vec<String>)>>>;

/// Stream fn: call 1 requests `noop`, later calls succeed, recording the
/// system prompt and user text of every request.
fn capture_tool_use_stream(
    contexts: CapturedContexts,
    call_count: Arc<AtomicU32>,
) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |context, _options| {
        let contexts = Arc::clone(&contexts);
        let call_count = Arc::clone(&call_count);
        async move {
            let users: Vec<String> = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    ai_types::Message::User { content, .. } => Some(user_content_text(content)),
                    _ => None,
                })
                .collect();
            contexts
                .lock()
                .unwrap()
                .push((context.system_prompt.clone(), users));
            let call = call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let message = if call == 1 {
                let mut message = assistant_message("", StopReason::ToolUse, None);
                message.content = vec![Content::tool_call("call-1", "noop", serde_json::json!({}))];
                message
            } else {
                assistant_message("Done", StopReason::Stop, None)
            };
            let stream = assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        }
    })
}

#[tokio::test]
async fn next_turn_refresh_chains_previous_hook_and_refreshes_state() {
    let hook_calls = Arc::new(AtomicU32::new(0));
    let previous_calls = Arc::clone(&hook_calls);
    let contexts: CapturedContexts = Arc::new(Mutex::new(Vec::new()));
    let call_count = Arc::new(AtomicU32::new(0));

    let (session, _) = make_session_with_tools(
        capture_tool_use_stream(Arc::clone(&contexts), Arc::clone(&call_count)),
        serde_json::json!({}),
        mock_model(),
        vec![noop_tool()],
        move |options| {
            options.prepare_next_turn = Some(Arc::new(move |_turn, _signal| {
                let previous_calls = Arc::clone(&previous_calls);
                Box::pin(async move {
                    previous_calls.fetch_add(1, Ordering::SeqCst);
                    Some(pillar_agent::AgentLoopTurnUpdate {
                        context: Some(pillar_agent::AgentContext {
                            system_prompt: "Hook prompt".to_string(),
                            messages: vec![pillar_agent::AgentMessage::Message(
                                ai_types::Message::User {
                                    content: UserContent::Text("hook-injected".to_string()),
                                    timestamp: 9,
                                },
                            )],
                            tools: Vec::new(),
                        }),
                        model: None,
                        thinking_level: None,
                    })
                }) as pillar_agent::types::PrepareNextFuture
            }));
        },
    );

    session
        .prompt("Use a tool", None)
        .await
        .expect("prompt succeeds");

    // The prior hook ran exactly once, chained after the compaction refresh.
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    let contexts = contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 2, "two provider requests: {contexts:?}");
    let (second_prompt, second_users) = &contexts[1];
    // The prior hook's context replacement survives, but the wrapper
    // restores the base system prompt and the agent's live tool set.
    assert_eq!(second_prompt.as_deref(), Some("Test"), "{contexts:?}");
    assert!(
        second_users
            .iter()
            .any(|text| text.contains("hook-injected")),
        "prior hook context preserved: {contexts:?}"
    );

    // The agent's system prompt reflects the refreshed base prompt.
    assert_eq!(session.system_prompt(), "Test");
}

// ============================================================================
// Overflow compaction -> compact-and-retry (upstream _checkCompaction Case 1)
// ============================================================================

/// Stream fn: call 1 is a length-stop overflow message (usage at the context
/// window), summarization prompts return a compaction summary, and later
/// calls succeed.
fn overflow_then_success_stream(call_count: Arc<AtomicU32>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |context, _options| {
        let call_count = Arc::clone(&call_count);
        async move {
            let is_summarization = context
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.to_lowercase().contains("summariz"));
            let call = call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let stream = assistant_message_event_stream();
            if is_summarization {
                let message = assistant_message("Compacted summary.", StopReason::Stop, None);
                stream.push(AssistantMessageEvent::Start {
                    partial: message.clone(),
                });
                stream.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message,
                });
            } else if call == 1 {
                let mut message = assistant_message("", StopReason::Length, None);
                message.usage.input = 200;
                message.usage.total_tokens = 200;
                stream.push(AssistantMessageEvent::Start {
                    partial: message.clone(),
                });
                stream.push(AssistantMessageEvent::Done {
                    reason: StopReason::Length,
                    message,
                });
            } else {
                let message = assistant_message("Success", StopReason::Stop, None);
                stream.push(AssistantMessageEvent::Start {
                    partial: message.clone(),
                });
                stream.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message,
                });
            }
            stream
        }
    })
}

#[tokio::test]
async fn overflow_compaction_retries_once_and_succeeds() {
    let mut model = mock_model();
    model.context_window = 200;
    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session_with_model(
        overflow_then_success_stream(Arc::clone(&call_count)),
        serde_json::json!({
            "compaction": { "enabled": true, "reserveTokens": 50, "keepRecentTokens": 10 },
            "retry": { "enabled": false },
        }),
        model,
    );

    // Seed prior history so the summarizer has a non-empty prefix.
    {
        let mut sm = session.session_manager().lock().unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::User {
            content: UserContent::Text("Earlier user turn".to_string()),
            timestamp: 1,
        }))
        .unwrap();
        sm.append_message(CodingAgentMessage::Base(ai_types::Message::Assistant(
            Box::new(assistant_message("Earlier reply", StopReason::Stop, None)),
        )))
        .unwrap();
    }

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let events = Arc::clone(&events);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::CompactionEnd {
                reason, will_retry, ..
            } = event
            {
                events
                    .lock()
                    .unwrap()
                    .push(format!("end:{reason}:retry={will_retry}"));
            }
        })
    };
    let _unsub = session.subscribe(listener);

    // A large prompt gives the cut point a non-empty prefix to summarize.
    let big_prompt = "word ".repeat(100);
    session
        .prompt(&big_prompt, None)
        .await
        .expect("prompt succeeds");

    // 1 length-stop call + 1 summarization call + 1 retry success.
    assert_eq!(call_count.load(Ordering::SeqCst), 3);

    let events = events.lock().unwrap().clone();
    assert!(
        events.iter().any(|e| e == "end:overflow:retry=true"),
        "{events:?}"
    );

    // The retry delivered a successful assistant response.
    let messages = session.state().messages;
    assert!(
        messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::Message(ai_types::Message::Assistant(a))
                if pillar_ai::text::content_text(&a.content, "") == "Success"
        )),
        "retry success in agent state: {messages:?}"
    );
    // No trailing length-stop message remains.
    assert!(
        !messages.iter().any(|m| matches!(
            m,
            pillar_agent::AgentMessage::Message(ai_types::Message::Assistant(a))
                if a.stop_reason == StopReason::Length
        )),
        "length message dropped before retry"
    );

    // A compaction checkpoint was persisted.
    let sm = session.session_manager().lock().unwrap();
    let branch = sm.get_branch(None);
    assert!(
        branch
            .iter()
            .any(|e| matches!(e, session_entry::SessionEntry::Compaction(_))),
        "compaction entry persisted: {branch:?}"
    );
}

// ============================================================================
// Tool interception hooks (upstream _installAgentToolHooks)
// ============================================================================

/// Stream fn: call 1 requests the `noop` tool, later calls succeed.
fn tool_use_then_done_stream(call_count: Arc<AtomicU32>) -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| {
        let call_count = Arc::clone(&call_count);
        async move {
            let call = call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let stream = assistant_message_event_stream();
            let message = if call == 1 {
                let mut message = assistant_message("", StopReason::ToolUse, None);
                message.content = vec![Content::tool_call("call-1", "noop", serde_json::json!({}))];
                message
            } else {
                assistant_message("Done", StopReason::Stop, None)
            };
            stream.push(AssistantMessageEvent::Start {
                partial: message.clone(),
            });
            stream.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
            stream
        }
    })
}

fn noop_tool() -> pillar_agent::AgentTool {
    pillar_agent::AgentTool {
        tool: pillar_ai::types::Tool {
            name: "noop".into(),
            description: "Noop tool".into(),
            parameters: serde_json::json!({"type": "object"}),
            constrained_sampling: None,
        },
        label: "Noop".into(),
        prepare_arguments: None,
        execute: Arc::new(|_id, _args, _signal, _on_update| {
            Box::pin(async move {
                Ok(pillar_agent::AgentToolResult {
                    content: vec![Content::text("ok")],
                    details: serde_json::json!({}),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}

#[tokio::test]
async fn tool_hooks_dispatch_tool_call_and_tool_result_to_extensions() {
    use pillar_coding_agent::core::extensions_runner::{ExtensionHandler, HostExtension};

    let hook_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tool_call_handler: ExtensionHandler = {
        let hook_calls = Arc::clone(&hook_calls);
        Arc::new(move |event: &serde_json::Value| {
            hook_calls.lock().unwrap().push(format!(
                "tool_call:{}",
                event["toolName"].as_str().unwrap_or("?")
            ));
            Ok(None)
        })
    };
    let tool_result_handler: ExtensionHandler = {
        let hook_calls = Arc::clone(&hook_calls);
        Arc::new(move |event: &serde_json::Value| {
            hook_calls.lock().unwrap().push(format!(
                "tool_result:{}",
                event["toolName"].as_str().unwrap_or("?")
            ));
            Ok(None)
        })
    };
    let mut handlers = std::collections::BTreeMap::new();
    handlers.insert("tool_call".to_string(), vec![tool_call_handler]);
    handlers.insert("tool_result".to_string(), vec![tool_result_handler]);
    let ext = HostExtension {
        path: "<inline>".to_string(),
        handlers,
        commands: Vec::new(),
        tools: std::collections::BTreeMap::new(),
        flags: std::collections::BTreeMap::new(),
        shortcuts: std::collections::BTreeMap::new(),
    };
    let runner = Arc::new(Mutex::new(ExtensionRunner::new(vec![ext])));

    let call_count = Arc::new(AtomicU32::new(0));
    let mut options = AgentOptions::new(tool_use_then_done_stream(Arc::clone(&call_count)));
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model: mock_model(),
        thinking_level: AgentThinkingLevel::Off,
        tools: vec![noop_tool()],
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));

    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let settings = SettingsManager::in_memory(
        serde_json::json!({ "retry": { "enabled": false } }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    );
    let settings_manager = Arc::new(Mutex::new(settings));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("agent-dir").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));
    let (runtime, _credentials) = runtime_with_anthropic_key();

    let session = AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime),
        runner,
    ));
    let session = Arc::new(session);

    session.install_tool_hooks();
    session
        .prompt("Use a tool", None)
        .await
        .expect("prompt succeeds");

    let calls = hook_calls.lock().unwrap().clone();
    assert!(calls.contains(&"tool_call:noop".to_string()), "{calls:?}");
    assert!(calls.contains(&"tool_result:noop".to_string()), "{calls:?}");
    // The tool ran and its result fed a second LLM call.
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

// ============================================================================
// Model management (upstream setModel / setThinkingLevel)
// ============================================================================

/// Full registry model fixture (upstream `Model` objects passed to
/// `setModel`).
fn model_fixture(id: &str, provider: &str, reasoning: bool, context_window: u64) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "anthropic-messages".to_string(),
        provider: provider.to_string(),
        base_url: String::new(),
        reasoning,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: None,
        },
        context_window,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Runner recording every `model_select` / `thinking_level_select` payload.
fn runner_recording_model_events(
    recorded: Arc<Mutex<Vec<serde_json::Value>>>,
) -> Arc<Mutex<ExtensionRunner>> {
    use pillar_coding_agent::core::extensions_runner::{ExtensionHandler, HostExtension};

    let handler_for = |event_type: &'static str| -> ExtensionHandler {
        let recorded = Arc::clone(&recorded);
        Arc::new(move |event: &serde_json::Value| {
            recorded.lock().unwrap().push(serde_json::json!({
                "handler": event_type,
                "event": event.clone(),
            }));
            Ok(None)
        })
    };
    let mut handlers = std::collections::BTreeMap::new();
    handlers.insert(
        "model_select".to_string(),
        vec![handler_for("model_select")],
    );
    handlers.insert(
        "thinking_level_select".to_string(),
        vec![handler_for("thinking_level_select")],
    );
    let extension = HostExtension {
        path: "<inline>".to_string(),
        handlers,
        commands: Vec::new(),
        tools: std::collections::BTreeMap::new(),
        flags: std::collections::BTreeMap::new(),
        shortcuts: std::collections::BTreeMap::new(),
    };
    Arc::new(Mutex::new(ExtensionRunner::new(vec![extension])))
}

#[tokio::test]
async fn set_model_updates_state_transcript_and_emits_model_select() {
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let (session, _) = make_session_with_model_and_runner(
        threshold_compaction_stream(),
        serde_json::json!({}),
        mock_model(),
        runner_recording_model_events(Arc::clone(&recorded)),
    );

    let next = model_fixture("claude-opus-4-1", "anthropic", false, 500_000);
    session
        .set_model(next, false)
        .await
        .expect("set model succeeds");

    // Agent state reflects the switched model.
    let model = session.model().expect("model selected");
    assert_eq!(model.id, "claude-opus-4-1");
    assert_eq!(model.context_window, 500_000);

    // The transcript recorded a model_change entry.
    {
        let session_manager = session.session_manager().lock().unwrap();
        let branch = session_manager.get_branch(None);
        assert!(
            branch.iter().any(|entry| matches!(
                entry,
                session_entry::SessionEntry::ModelChange(change)
                    if change.provider == "anthropic" && change.model_id == "claude-opus-4-1"
            )),
            "model_change appended: {branch:?}"
        );
    }

    let recorded = recorded.lock().unwrap().clone();
    let select = recorded
        .iter()
        .find(|entry| entry["handler"] == "model_select")
        .expect("model_select emitted");
    assert_eq!(select["event"]["model"]["id"], "claude-opus-4-1");
    assert_eq!(select["event"]["previousModel"]["id"], "claude-sonnet-4-5");
    assert_eq!(select["event"]["source"], "set");
}

#[tokio::test]
async fn set_model_without_auth_fails() {
    let (session, _) = make_session(threshold_compaction_stream(), serde_json::json!({}));
    let next = model_fixture("gpt-5", "openai", false, 400_000);
    let error = session
        .set_model(next, false)
        .await
        .expect_err("auth gate rejects");
    assert_eq!(error, "No API key for openai/gpt-5");
}

#[tokio::test]
async fn set_model_persist_updates_global_default() {
    let (session, _) = make_session(threshold_compaction_stream(), serde_json::json!({}));
    let next = model_fixture("claude-opus-4-1", "anthropic", false, 500_000);
    session
        .set_model(next, true)
        .await
        .expect("set model persists");

    let settings = session.settings_manager().lock().unwrap();
    assert_eq!(
        settings.default_model_and_provider(),
        Some(("anthropic".to_string(), "claude-opus-4-1".to_string()))
    );
}

#[tokio::test]
async fn set_thinking_level_updates_state_and_emits_event() {
    let recorded: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let mut model = mock_model();
    model.reasoning = true;
    let (session, _) = make_session_with_model_and_runner(
        threshold_compaction_stream(),
        serde_json::json!({}),
        model,
        runner_recording_model_events(Arc::clone(&recorded)),
    );

    let session_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let session_events = Arc::clone(&session_events);
        Arc::new(move |event: &AgentSessionEvent| {
            if let AgentSessionEvent::ThinkingLevelChanged { level } = event {
                session_events.lock().unwrap().push(level.clone());
            }
        })
    };
    let _unsub = session.subscribe(listener);

    session.set_thinking_level("high", false);
    assert_eq!(session.thinking_level(), "high");

    {
        let session_manager = session.session_manager().lock().unwrap();
        let branch = session_manager.get_branch(None);
        assert!(
            branch.iter().any(|entry| matches!(
                entry,
                session_entry::SessionEntry::ThinkingLevelChange(change)
                    if change.thinking_level == "high"
            )),
            "thinking_level_change appended: {branch:?}"
        );
    }
    assert_eq!(session_events.lock().unwrap().clone(), vec!["high"]);

    let recorded = recorded.lock().unwrap().clone();
    let select = recorded
        .iter()
        .find(|entry| entry["handler"] == "thinking_level_select")
        .expect("thinking_level_select emitted");
    assert_eq!(select["event"]["level"], "high");
    assert_eq!(select["event"]["previousLevel"], "off");
}

// ============================================================================
// Extension binding (upstream bindExtensions / extendResourcesFromExtensions)
// ============================================================================

/// Session harness for binding tests: fixed model, no tools, empty queue.
fn binding_session(
    runner: Arc<Mutex<ExtensionRunner>>,
    session_start_reason: &str,
    rebuild: Option<SystemPromptRebuildFn>,
    runner_factory: Option<ExtensionRunnerFactory>,
) -> Arc<AgentSession> {
    let mut options = AgentOptions::new(threshold_compaction_stream());
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
    let (runtime, _credentials) = runtime_with_anthropic_key();
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({ "retry": { "enabled": false } }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("agent-dir").to_string_lossy().to_string(),
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
        String::new(),
        resource_loader,
        Arc::new(runtime),
        runner,
    );
    config.session_start_event = Some(SessionEventMeta {
        reason: session_start_reason.to_string(),
        previous_session_file: None,
    });
    config.system_prompt_rebuild = rebuild;
    config.extension_runner_rebuild = runner_factory;
    Arc::new(AgentSession::new(config))
}

/// Runner with `session_start` and `resources_discover` handlers recording
/// `(event type, reason)` and returning one skill path.
fn runner_with_resource_discovery(
    calls: Arc<Mutex<Vec<(String, String)>>>,
) -> Arc<Mutex<ExtensionRunner>> {
    use pillar_coding_agent::core::extensions_runner::{ExtensionHandler, HostExtension};

    let session_start: ExtensionHandler = {
        let calls = Arc::clone(&calls);
        Arc::new(move |event: &serde_json::Value| {
            calls.lock().unwrap().push((
                "session_start".to_string(),
                event["reason"].as_str().unwrap_or("?").to_string(),
            ));
            Ok(None)
        })
    };
    let resources_discover: ExtensionHandler = {
        let calls = Arc::clone(&calls);
        Arc::new(move |event: &serde_json::Value| {
            calls.lock().unwrap().push((
                "resources_discover".to_string(),
                event["reason"].as_str().unwrap_or("?").to_string(),
            ));
            Ok(Some(serde_json::json!({
                "skillPaths": ["/tmp/pillar-bind/SKILL.md"],
            })))
        })
    };
    let mut handlers = BTreeMap::new();
    handlers.insert("session_start".to_string(), vec![session_start]);
    handlers.insert("resources_discover".to_string(), vec![resources_discover]);
    let extension = HostExtension {
        path: "<inline>".to_string(),
        handlers,
        commands: Vec::new(),
        tools: BTreeMap::new(),
        flags: BTreeMap::new(),
        shortcuts: BTreeMap::new(),
    };
    Arc::new(Mutex::new(ExtensionRunner::new(vec![extension])))
}

#[tokio::test]
async fn bind_extensions_emits_session_start_and_extends_resources() {
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let runner = runner_with_resource_discovery(Arc::clone(&calls));

    let rebuild_calls = Arc::new(AtomicU32::new(0));
    let rebuild_calls_for = Arc::clone(&rebuild_calls);
    let rebuild: SystemPromptRebuildFn = Arc::new(move |tools: &[String]| {
        rebuild_calls_for.fetch_add(1, Ordering::SeqCst);
        format!("REBUILT tools={}", tools.len())
    });

    let session = binding_session(Arc::clone(&runner), "startup", Some(rebuild), None);
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(true),
            mode: Some("interactive".to_string()),
            on_error: None,
        })
        .await;

    // UI presence reached the runner and both extension events fired.
    assert!(runner.lock().unwrap().has_ui());
    let calls = calls.lock().unwrap().clone();
    assert!(
        calls.contains(&("session_start".to_string(), "startup".to_string())),
        "{calls:?}"
    );
    assert!(
        calls.contains(&("resources_discover".to_string(), "startup".to_string())),
        "{calls:?}"
    );

    // Discovered resources rebuilt the base system prompt.
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session.system_prompt(), "REBUILT tools=0");
}

#[tokio::test]
async fn bind_extensions_uses_reload_reason_and_skips_without_handlers() {
    // A reload session passes "reload" to resources_discover.
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let runner = runner_with_resource_discovery(Arc::clone(&calls));
    let session = binding_session(Arc::clone(&runner), "reload", None, None);
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(false),
            mode: None,
            on_error: None,
        })
        .await;
    assert!(
        calls
            .lock()
            .unwrap()
            .contains(&("resources_discover".to_string(), "reload".to_string())),
        "{calls:?}"
    );
    assert!(!runner.lock().unwrap().has_ui());

    // Without a resources_discover handler the rebuild hook never runs.
    let rebuild_calls = Arc::new(AtomicU32::new(0));
    let rebuild_calls_for = Arc::clone(&rebuild_calls);
    let rebuild: SystemPromptRebuildFn = Arc::new(move |_tools: &[String]| {
        rebuild_calls_for.fetch_add(1, Ordering::SeqCst);
        "unused".to_string()
    });
    let empty_runner = Arc::new(Mutex::new(ExtensionRunner::new(Vec::new())));
    let session = binding_session(empty_runner, "startup", Some(rebuild), None);
    session.bind_extensions(ExtensionBindings::default()).await;
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0);
    assert_eq!(session.system_prompt(), "Test");
}

// ============================================================================
// Reload (upstream reload)
// ============================================================================

#[tokio::test]
async fn reload_rebuilds_runner_and_reemits_session_start() {
    use pillar_coding_agent::core::extensions_runner::{ExtensionHandler, HostExtension};

    // Old runner records session_shutdown and carries a flag value.
    let shutdown_reasons: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let shutdown_handler: ExtensionHandler = {
        let shutdown_reasons = Arc::clone(&shutdown_reasons);
        Arc::new(move |event: &serde_json::Value| {
            shutdown_reasons
                .lock()
                .unwrap()
                .push(event["reason"].as_str().unwrap_or("?").to_string());
            Ok(None)
        })
    };
    let mut old_handlers = BTreeMap::new();
    old_handlers.insert("session_shutdown".to_string(), vec![shutdown_handler]);
    let old_extension = HostExtension {
        path: "<old>".to_string(),
        handlers: old_handlers,
        commands: Vec::new(),
        tools: BTreeMap::new(),
        flags: BTreeMap::new(),
        shortcuts: BTreeMap::new(),
    };
    let mut old_runner = ExtensionRunner::new(vec![old_extension]);
    old_runner.set_flag_value("theme", serde_json::json!("dark"));
    let runner = Arc::new(Mutex::new(old_runner));

    // The factory receives the previous flag values and returns a runner
    // whose session_start handler records the reload reason.
    let session_start_reasons: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let factory_inputs: Arc<Mutex<Vec<BTreeMap<String, serde_json::Value>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let factory: ExtensionRunnerFactory = {
        let session_start_reasons = Arc::clone(&session_start_reasons);
        let factory_inputs = Arc::clone(&factory_inputs);
        Arc::new(move |flags: BTreeMap<String, serde_json::Value>| {
            factory_inputs.lock().unwrap().push(flags);
            let handler: ExtensionHandler = {
                let session_start_reasons = Arc::clone(&session_start_reasons);
                Arc::new(move |event: &serde_json::Value| {
                    session_start_reasons
                        .lock()
                        .unwrap()
                        .push(event["reason"].as_str().unwrap_or("?").to_string());
                    Ok(None)
                })
            };
            let mut handlers = BTreeMap::new();
            handlers.insert("session_start".to_string(), vec![handler]);
            let extension = HostExtension {
                path: "<new>".to_string(),
                handlers,
                commands: Vec::new(),
                tools: BTreeMap::new(),
                flags: BTreeMap::new(),
                shortcuts: BTreeMap::new(),
            };
            ExtensionRunner::new(vec![extension])
        })
    };

    let before_calls = Arc::new(AtomicU32::new(0));
    let before_calls_for = Arc::clone(&before_calls);
    let before: BeforeSessionStartFn = Arc::new(move || {
        let before_calls = Arc::clone(&before_calls_for);
        Box::pin(async move {
            before_calls.fetch_add(1, Ordering::SeqCst);
        })
    });

    let session = binding_session(Arc::clone(&runner), "startup", None, Some(factory));
    // Bindings must be present so reload re-emits session_start.
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(true),
            mode: None,
            on_error: None,
        })
        .await;

    session.reload(Some(before)).await.expect("reload succeeds");

    assert_eq!(shutdown_reasons.lock().unwrap().clone(), vec!["reload"]);
    let inputs = factory_inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1, "factory called once: {inputs:?}");
    assert_eq!(
        inputs[0].get("theme").and_then(|value| value.as_str()),
        Some("dark")
    );
    assert_eq!(
        session_start_reasons.lock().unwrap().clone(),
        vec!["reload"]
    );
    assert_eq!(before_calls.load(Ordering::SeqCst), 1);
}

// ============================================================================
// createAgentSession (upstream core/sdk.ts factory)
// ============================================================================

#[tokio::test]
async fn create_agent_session_wires_agent_tools_prompt_and_persistence() {
    use pillar_coding_agent::core::sdk::{CreateAgentSessionOptions, create_agent_session};

    let (runtime, _credentials) = runtime_with_anthropic_key();
    let dir = temp_dir("sdk");
    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({ "retry": { "enabled": false } }),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let session_manager = SessionManager::in_memory("", None).unwrap();
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: dir.to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: String::new(),
        agent_dir: Some(dir.to_string_lossy().to_string()),
        model_runtime: Arc::new(runtime),
        settings_manager: Some(settings_manager),
        session_manager: Some(session_manager),
        resource_loader: Some(resource_loader),
        model: Some(model_fixture(
            "claude-sonnet-4-5",
            "anthropic",
            false,
            200_000,
        )),
        thinking_level: None,
        scoped_models: Vec::new(),
        tools: None,
        no_tools: None,
        exclude_tools: Vec::new(),
        custom_tools: Vec::new(),
        extension_runner: Arc::new(Mutex::new(ExtensionRunner::new(Vec::new()))),
        session_start_event: None,
        system_prompt_rebuild: None,
        extension_runner_rebuild: None,
        stream_fn: Some(threshold_compaction_stream()),
    })
    .await
    .expect("session created");

    let session = created.session;
    assert_eq!(
        session.model().map(|model| model.id),
        Some("claude-sonnet-4-5".to_string())
    );
    let tool_names: Vec<String> = session
        .state()
        .tools
        .iter()
        .map(|tool| tool.tool.name.clone())
        .collect();
    assert_eq!(tool_names, vec!["read", "bash", "edit", "write"]);
    assert!(!session.system_prompt().is_empty());

    session.prompt("hi", None).await.expect("prompt succeeds");
    let messages = session.state().messages;
    assert!(
        messages.iter().any(|message| matches!(
            message,
            pillar_agent::AgentMessage::Message(ai_types::Message::Assistant(assistant))
                if pillar_ai::text::content_text(&assistant.content, "") == "Done"
        )),
        "assistant response in state: {messages:?}"
    );

    // A new session persisted its initial model and thinking level.
    let session_manager = session.session_manager().lock().unwrap();
    let branch = session_manager.get_branch(None);
    assert!(
        branch
            .iter()
            .any(|entry| matches!(entry, session_entry::SessionEntry::ModelChange(_))),
        "model_change appended: {branch:?}"
    );
    assert!(
        branch
            .iter()
            .any(|entry| matches!(entry, session_entry::SessionEntry::ThinkingLevelChange(_))),
        "thinking_level_change appended: {branch:?}"
    );
}

// ============================================================================
// Print mode (upstream modes/print-mode.ts)
// ============================================================================

/// Stream fn that emits a text delta before finishing, so the session
/// produces `message_update` events (upstream streaming shape).
fn delta_then_done_stream() -> pillar_agent::StreamFn {
    pillar_agent::StreamFn::new(move |_context, _options| async move {
        let stream = assistant_message_event_stream();
        let empty = assistant_message("", StopReason::Stop, None);
        stream.push(AssistantMessageEvent::Start {
            partial: empty.clone(),
        });
        let with_text = assistant_message("Hello", StopReason::Stop, None);
        stream.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: empty,
        });
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "Hello".to_string(),
            partial: with_text.clone(),
        });
        stream.push(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: with_text,
        });
        stream
    })
}

#[tokio::test]
async fn print_mode_prints_the_final_assistant_text() {
    use pillar_coding_agent::modes::print_mode::{PrintModeMode, PrintModeOptions, run_print_mode};

    let (session, _) = make_session(threshold_compaction_stream(), serde_json::json!({}));
    let mut out = Vec::new();
    let code = run_print_mode(
        &session,
        PrintModeOptions {
            mode: PrintModeMode::Text,
            messages: Vec::new(),
            initial_message: Some("hi".to_string()),
            initial_images: None,
        },
        &mut out,
    )
    .await
    .expect("print mode succeeds");

    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap().trim(), "Done");
}

#[tokio::test]
async fn print_mode_reports_assistant_errors() {
    use pillar_coding_agent::modes::print_mode::{PrintModeMode, PrintModeOptions, run_print_mode};

    let call_count = Arc::new(AtomicU32::new(0));
    let (session, _) = make_session(
        fail_then_succeed_stream(1, Arc::clone(&call_count)),
        serde_json::json!({ "retry": { "enabled": false } }),
    );
    let mut out = Vec::new();
    let error = run_print_mode(
        &session,
        PrintModeOptions {
            mode: PrintModeMode::Text,
            messages: Vec::new(),
            initial_message: Some("hi".to_string()),
            initial_images: None,
        },
        &mut out,
    )
    .await
    .expect_err("assistant error surfaces");

    assert!(error.contains("overloaded_error"), "{error}");
}

#[tokio::test]
async fn print_mode_json_emits_header_and_event_lines() {
    use pillar_coding_agent::modes::print_mode::{PrintModeMode, PrintModeOptions, run_print_mode};

    let (session, _) = make_session(delta_then_done_stream(), serde_json::json!({}));
    let mut out = Vec::new();
    let code = run_print_mode(
        &session,
        PrintModeOptions {
            mode: PrintModeMode::Json,
            messages: Vec::new(),
            initial_message: Some("hi".to_string()),
            initial_images: None,
        },
        &mut out,
    )
    .await
    .expect("json mode succeeds");

    assert_eq!(code, 0);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 1, "{text}");
    let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header line is JSON");
    assert!(header.get("id").is_some(), "{header}");
    assert!(text.contains("\"type\":\"message_update\""), "{text}");
    assert!(text.contains("assistantMessageEvent"), "{text}");
    // Cumulative assistant snapshots are stripped from the wire.
    assert!(!text.contains("\"partial\""), "{text}");
}

// ============================================================================
// RPC mode (upstream modes/rpc/rpc-mode.ts)
// ============================================================================

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn rpc_mode_get_state_only() {
    use pillar_coding_agent::modes::rpc::rpc_mode::RpcMode;

    let (session, _) = make_session(
        threshold_compaction_stream(),
        serde_json::json!({ "retry": { "enabled": false } }),
    );
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let out: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
        Arc::new(Mutex::new(Box::new(SharedBuf(Arc::clone(&buffer)))));
    let mode = RpcMode::new(Arc::clone(&session), out);
    let response = mode
        .handle_command(command_envelope(
            serde_json::json!({ "id": "1", "type": "get_state" }),
        ))
        .await;
    assert!(response.success, "{response:?}");
}

#[tokio::test]
async fn rpc_mode_dispatches_core_commands() {
    use pillar_coding_agent::modes::rpc::rpc_mode::RpcMode;
    use serde_json::json;

    let (session, _) = make_session(
        threshold_compaction_stream(),
        serde_json::json!({ "retry": { "enabled": false } }),
    );
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let out: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
        Arc::new(Mutex::new(Box::new(SharedBuf(Arc::clone(&buffer)))));
    let mode = RpcMode::new(Arc::clone(&session), out);

    // get_state
    let response = mode
        .handle_command(command_envelope(json!({ "id": "1", "type": "get_state" })))
        .await;
    assert!(response.success, "{response:?}");
    let data = response.data.expect("state data");
    assert_eq!(data["sessionId"], json!(session.session_id()));
    assert_eq!(data["thinkingLevel"], json!("off"));
    assert_eq!(data["autoCompactionEnabled"], json!(true));
    assert_eq!(data["isStreaming"], json!(false));

    // prompt writes events and records the assistant reply
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "2", "type": "prompt", "message": "hi" }),
        ))
        .await;
    assert!(response.success, "{response:?}");

    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "3", "type": "get_last_assistant_text" }),
        ))
        .await;
    assert_eq!(response.data.expect("text data")["text"], json!("Done"));

    // get_messages
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "4", "type": "get_messages" }),
        ))
        .await;
    let messages = response.data.expect("messages data");
    assert!(
        messages["messages"]
            .as_array()
            .is_some_and(|m| !m.is_empty())
    );

    // clear_queue
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "5", "type": "clear_queue" }),
        ))
        .await;
    assert_eq!(
        response.data.expect("queue data"),
        json!({ "steering": [], "followUp": [] })
    );

    // settings toggles
    for command in [
        json!({ "id": "6", "type": "set_auto_retry", "enabled": false }),
        json!({ "id": "7", "type": "set_auto_compaction", "enabled": false }),
        json!({ "id": "8", "type": "set_steering_mode", "mode": "all" }),
        json!({ "id": "9", "type": "set_thinking_level", "level": "high" }),
        json!({ "id": "10", "type": "set_session_name", "name": "rpc" }),
    ] {
        let response = mode.handle_command(command_envelope(command)).await;
        assert!(response.success, "{response:?}");
    }
    assert!(!session.auto_compaction_enabled());

    // available models / thinking levels
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "11", "type": "get_available_models" }),
        ))
        .await;
    assert!(response.success);
    assert!(response.data.expect("models")["models"].is_array());

    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "12", "type": "get_available_thinking_levels" }),
        ))
        .await;
    assert_eq!(response.data.expect("levels")["levels"], json!(["off"]));

    // session tree / entries (full canonical entry serialization)
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "13", "type": "get_entries" }),
        ))
        .await;
    assert!(response.success, "{response:?}");
    let data = response.data.expect("entries data");
    let entries = data["entries"].as_array().cloned().expect("entries array");
    assert!(!entries.is_empty());
    assert!(
        entries
            .iter()
            .any(|entry| { entry["type"] == json!("message") && entry.get("message").is_some() })
    );
    assert!(data["leafId"].is_string());

    let last_id = entries
        .last()
        .and_then(|entry| entry["id"].as_str())
        .expect("entry id")
        .to_string();
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "14", "type": "get_entries", "since": last_id }),
        ))
        .await;
    assert_eq!(
        response.data.expect("entries data")["entries"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "15", "type": "get_entries", "since": "missing" }),
        ))
        .await;
    assert!(!response.success);

    let response = mode
        .handle_command(command_envelope(json!({ "id": "16", "type": "get_tree" })))
        .await;
    let tree = response.data.expect("tree data")["tree"]
        .as_array()
        .cloned()
        .expect("tree array");
    assert!(!tree.is_empty());
    assert!(tree[0]["entry"]["type"].is_string());

    // session stats (entry counts + usage totals)
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "17", "type": "get_session_stats" }),
        ))
        .await;
    let stats = response.data.expect("stats data");
    assert!(stats["totalMessages"].as_u64().unwrap_or(0) >= 1);
    assert!(stats["tokens"]["total"].is_u64());
    assert_eq!(stats["sessionId"], json!(session.session_id()));

    // cycle commands answer successfully even when there is nothing to cycle
    for command in [
        json!({ "id": "18", "type": "cycle_model" }),
        json!({ "id": "19", "type": "cycle_thinking_level" }),
    ] {
        let response = mode.handle_command(command_envelope(command)).await;
        assert!(response.success, "{response:?}");
    }

    // fork candidates + slash commands + empty session-name rejection
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "20", "type": "get_fork_messages" }),
        ))
        .await;
    let messages = response.data.expect("fork messages")["messages"]
        .as_array()
        .cloned()
        .expect("messages array");
    assert!(
        messages
            .iter()
            .any(|message| message["text"] == json!("hi")),
        "{messages:?}"
    );

    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "21", "type": "get_commands" }),
        ))
        .await;
    let commands = response.data.clone().expect("commands data");
    assert!(commands["commands"].is_array(), "{response:?}");

    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "22", "type": "set_session_name", "name": "   " }),
        ))
        .await;
    assert!(!response.success);

    // export_html rejects an in-memory session (upstream
    // "Cannot export in-memory session to HTML"); the real binary with a
    // session file is covered by the CLI smoke test.
    let response = mode
        .handle_command(command_envelope(json!({
            "id": "23",
            "type": "export_html",
            "outputPath": temp_dir("rpc-export").join("session.html").to_string_lossy(),
        })))
        .await;
    assert!(!response.success);
    assert!(
        response.error.unwrap_or_default().contains("in-memory"),
        "export_html should reject in-memory sessions"
    );

    // unsupported commands fail explicitly
    let response = mode
        .handle_command(command_envelope(
            json!({ "id": "24", "type": "fork", "entryId": "missing" }),
        ))
        .await;
    assert!(!response.success);
    assert!(
        response
            .error
            .unwrap_or_default()
            .contains("not supported yet")
    );

    // events were written as JSON lines
    let written = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert!(written.contains("\"type\":\"agent_start\""), "{written}");
    assert!(written.contains("\"type\":\"message_end\""), "{written}");
}

fn command_envelope(
    value: serde_json::Value,
) -> pillar_coding_agent::modes::rpc::rpc_types::RpcCommandEnvelope {
    serde_json::from_value(value).expect("command envelope")
}
