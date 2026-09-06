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
    AssistantMessage, AssistantMessageEvent, Content, StopReason, Usage, UsageCost, UserContent,
};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, AgentSessionConfig, AgentSessionEvent,
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
    let dir =
        std::env::temp_dir().join(format!("pillar-session-loop-{}-{name}", std::process::id()));
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

    let resource_loader = Arc::new(ResourceLoader::new(
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
    ));

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
    let resource_loader = Arc::new(ResourceLoader::new(
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
    ));

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
    let resource_loader = Arc::new(ResourceLoader::new(
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
    ));
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
