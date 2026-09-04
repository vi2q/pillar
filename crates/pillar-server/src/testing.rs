//! Port of packages/server/src/testing/service.ts + server.ts (pi
//! v0.84.3): deterministic test doubles for the PiServer conformance
//! suite — `TestSessionRuntime` (a scripted runtime whose prompt runs
//! until completed or aborted), `TestServerService` (a seeded
//! session catalog with locking), and `create_test_server`.
//!
//! divergences: Deferred promises become synchronous control — the
//! runtime exposes `finish_prompt`/`abort` as direct calls and
//! prompt returns after applying the outcome; the async list delay
//! hook becomes an explicit pending flag; transcript items are built
//! against pillar-protocol shapes.

use std::collections::{HashMap, HashSet};

use pillar_protocol::schemas::{
    AssistantContent, AssistantStatus, AssistantStopReason, AssistantTranscriptItem, InputKind,
    ModelCost, ModelMetadata, ModelRef, ProtocolErrorCode, SessionMetadata, SessionPhase,
    SessionSnapshot, ThinkingLevel, TranscriptItem, TranscriptProgress, UserContent,
};

use crate::sessions::{
    CreateSessionOptions, PiServerService, PiSessionRuntime, RuntimeEvent, ServerError,
};

/// The deterministic test model (upstream `TEST_MODEL`).
pub fn test_model() -> ModelMetadata {
    ModelMetadata {
        provider: "test".to_string(),
        id: "small".to_string(),
        name: "Test Small".to_string(),
        api: "test-api".to_string(),
        reasoning: true,
        input: vec![InputKind::Text, InputKind::Image],
        context_window: 16_000,
        max_tokens: 2_000,
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        supported_thinking_levels: vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
        authenticated: true,
    }
}

/// A pending prompt (upstream the `pendingPrompt` Deferred pair); the
/// port resolves the outcome synchronously via `finish_prompt`/
/// `abort`).
struct PendingPrompt {
    text: String,
    /// Resolved outcome (upstream the Deferred<"complete"|"aborted">).
    outcome: Option<PromptOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    Complete,
    Aborted,
}

/// A scripted runtime (upstream `TestSessionRuntime`).
pub struct TestSessionRuntime {
    #[allow(dead_code)]
    id: String,
    snapshot: SessionSnapshot,
    dispose_count: usize,
    pub steers: Vec<String>,
    pending_prompt: Option<PendingPrompt>,
    listeners: Vec<std::sync::mpsc::Sender<RuntimeEvent>>,
    on_dispose: Box<dyn Fn() + Send>,
}

impl TestSessionRuntime {
    pub fn new(snapshot: SessionSnapshot, on_dispose: Box<dyn Fn() + Send>) -> Self {
        Self {
            id: snapshot.id.clone(),
            snapshot,
            dispose_count: 0,
            steers: Vec::new(),
            pending_prompt: None,
            listeners: Vec::new(),
            on_dispose,
        }
    }

    pub fn dispose_count(&self) -> usize {
        self.dispose_count
    }

    /// Upstream `setPhase`.
    pub fn set_phase(&mut self, phase: SessionPhase) {
        self.snapshot.phase = phase;
    }

    /// Upstream `finishPrompt`: complete the pending prompt and
    /// apply the outcome (upstream the awaited Deferred resolves,
    /// letting prompt() finish).
    pub fn finish_prompt(&mut self) -> Result<(), ServerError> {
        self.resolve_pending(PromptOutcome::Complete)?;
        self.apply_prompt_outcome();
        Ok(())
    }

    /// Upstream `abort` from the client side.
    pub fn request_abort(&mut self) -> Result<(), ServerError> {
        self.resolve_pending(PromptOutcome::Aborted)
    }

    /// Upstream `emitProgress`.
    pub fn emit_progress(&mut self, progress: TranscriptProgress) {
        self.broadcast(RuntimeEvent::Progress(progress));
    }

    /// Upstream `emitError`.
    pub fn emit_error(&mut self, error: ServerError) {
        self.broadcast(RuntimeEvent::Error(error));
    }

    pub fn resolve_pending(&mut self, outcome: PromptOutcome) -> Result<(), ServerError> {
        let Some(pending) = self.pending_prompt.as_mut() else {
            return Err(ServerError::new(
                ProtocolErrorCode::Busy,
                "There is no active prompt to abort",
            ));
        };
        pending.outcome = Some(outcome);
        Ok(())
    }

    fn apply_prompt_outcome(&mut self) {
        let Some(pending) = self.pending_prompt.take() else {
            return;
        };
        let Some(outcome) = pending.outcome else {
            // Upstream would still be awaiting the Deferred; re-arm.
            self.pending_prompt = Some(pending);
            return;
        };
        let revision = self.snapshot.revision + 1;
        let assistant = TranscriptItem::Assistant(AssistantTranscriptItem {
            id: format!("assistant-{revision}"),
            content: vec![AssistantContent::Text {
                text: if outcome == PromptOutcome::Complete {
                    format!("reply:{}", pending.text)
                } else {
                    String::new()
                },
            }],
            model: self.snapshot.model.clone(),
            response_model: None,
            usage: None,
            status: match outcome {
                PromptOutcome::Complete => AssistantStatus::Complete,
                PromptOutcome::Aborted => AssistantStatus::Aborted,
            },
            stop_reason: Some(match outcome {
                PromptOutcome::Complete => AssistantStopReason::Stop,
                PromptOutcome::Aborted => AssistantStopReason::Aborted,
            }),
            error_message: None,
            timestamp: revision,
        });
        self.update(|snapshot| {
            snapshot.phase = SessionPhase::Idle;
            snapshot.transcript.push(assistant);
        });
    }

    fn update(&mut self, apply: impl FnOnce(&mut SessionSnapshot)) {
        apply(&mut self.snapshot);
        self.snapshot.revision += 1;
        self.snapshot.updated_at += 1;
        self.broadcast(RuntimeEvent::Snapshot);
    }

    fn broadcast(&mut self, event: RuntimeEvent) {
        self.listeners
            .retain(|listener| listener.send(event.clone()).is_ok());
    }
}

impl PiSessionRuntime for TestSessionRuntime {
    fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.clone()
    }

    fn phase(&self) -> SessionPhase {
        self.snapshot.phase
    }

    fn prompt(&mut self, text: &str) -> Result<(), ServerError> {
        if self.phase() != SessionPhase::Idle {
            return Err(ServerError::new(
                ProtocolErrorCode::Busy,
                "A prompt is already running",
            ));
        }
        self.pending_prompt = Some(PendingPrompt {
            text: text.to_string(),
            outcome: None,
        });
        let revision = self.snapshot.revision + 1;
        let user_item = TranscriptItem::User {
            id: format!("user-{revision}"),
            content: vec![UserContent::Text {
                text: text.to_string(),
            }],
            timestamp: revision,
        };
        self.update(|snapshot| {
            snapshot.phase = SessionPhase::Turn;
            snapshot.transcript.push(user_item);
        });
        Ok(())
    }

    fn steer(&mut self, text: &str) -> Result<(), ServerError> {
        if self.phase() == SessionPhase::Idle {
            return Err(ServerError::new(
                ProtocolErrorCode::Busy,
                "There is no active prompt to steer",
            ));
        }
        self.steers.push(text.to_string());
        let revision = self.snapshot.revision + 1;
        let steer_item = TranscriptItem::User {
            id: format!("steer-{revision}"),
            content: vec![UserContent::Text {
                text: text.to_string(),
            }],
            timestamp: revision,
        };
        self.update(|snapshot| {
            snapshot.queued_steer_count += 1;
            snapshot.queued_steer.push(steer_item);
        });
        Ok(())
    }

    fn abort(&mut self) -> Result<(), ServerError> {
        if self.pending_prompt.is_none() {
            return Err(ServerError::new(
                ProtocolErrorCode::Busy,
                "There is no active prompt to abort",
            ));
        }
        self.resolve_pending(PromptOutcome::Aborted)?;
        self.apply_prompt_outcome();
        Ok(())
    }

    fn set_model(&mut self, model: &ModelRef) -> Result<(), ServerError> {
        if self.phase() != SessionPhase::Idle {
            return Err(ServerError::new(ProtocolErrorCode::Busy, "Session is busy"));
        }
        let model = model.clone();
        self.update(|snapshot| snapshot.model = model);
        Ok(())
    }

    fn set_thinking(&mut self, level: &ThinkingLevel) -> Result<(), ServerError> {
        if self.phase() != SessionPhase::Idle {
            return Err(ServerError::new(ProtocolErrorCode::Busy, "Session is busy"));
        }
        let level = *level;
        self.update(|snapshot| snapshot.thinking_level = level);
        Ok(())
    }

    fn dispose(&mut self) {
        self.dispose_count += 1;
        (self.on_dispose)();
    }
}

/// A seeded service (upstream `TestServerService`).
pub struct TestServerService {
    pub sessions: HashMap<String, SessionSnapshot>,
    pub locked: HashSet<String>,
    pub runtime_count: HashMap<String, usize>,
    pub last_created_id: Option<String>,
    /// Shared runtime lifecycle log (upstream tests inspect
    /// runtime.disposeCount; the port records through a closure).
    pub event_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Shared lock registry so runtime dispose can release it
    /// (upstream the onDispose closure does `locked.delete(id)`).
    locked_registry: std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
    /// Upstream `delayNextList`: when set, the next list_sessions
    /// reports it as entered and holds.
    pub pending_list_delay: bool,
    pub list_delay_entered: bool,
}

impl Default for TestServerService {
    fn default() -> Self {
        Self::new()
    }
}

impl TestServerService {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            locked: HashSet::new(),
            runtime_count: HashMap::new(),
            last_created_id: None,
            event_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            locked_registry: std::sync::Arc::new(std::sync::Mutex::new(HashSet::new())),
            pending_list_delay: false,
            list_delay_entered: false,
        }
    }

    /// Upstream `seed`.
    #[allow(clippy::too_many_arguments)]
    pub fn seed(
        &mut self,
        id: &str,
        name: &str,
        cwd: &str,
        model: ModelRef,
        thinking_level: ThinkingLevel,
    ) {
        self.sessions.insert(
            id.to_string(),
            SessionSnapshot {
                id: id.to_string(),
                name: Some(name.to_string()),
                cwd: cwd.to_string(),
                created_at: 1,
                updated_at: 1,
                phase: SessionPhase::Idle,
                model,
                thinking_level,
                attached: false,
                locked: false,
                revision: 0,
                transcript: Vec::new(),
                queued_steer: Vec::new(),
                queued_steer_count: 0,
            },
        );
    }

    pub fn seed_default(&mut self, id: &str) {
        let model = test_model();
        self.seed(
            id,
            &format!("Session {id}"),
            "/tmp/pi-server-conformance",
            ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            },
            ThinkingLevel::Off,
        );
    }

    /// Upstream `delayNextList`.
    pub fn delay_next_list(&mut self) {
        self.pending_list_delay = true;
        self.list_delay_entered = false;
    }

    pub fn release_list_delay(&mut self) {
        self.pending_list_delay = false;
    }

    fn is_locked(&self, id: &str) -> bool {
        self.locked.contains(id)
            || self
                .locked_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(id)
    }

    pub fn lock_session(&mut self, id: &str) {
        self.locked_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string());
    }

    fn acquire(&mut self, id: &str) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
        let stored = self.sessions.get(id).cloned();
        let Some(stored) = stored else {
            return Err(ServerError::new(
                ProtocolErrorCode::InternalError,
                format!("Unknown session: {id}"),
            ));
        };
        self.lock_session(id);
        let snapshot = stored;
        let counter = self.runtime_count.entry(id.to_string()).or_default();
        *counter += 1;
        let log = self.event_log.clone();
        let log_id = id.to_string();
        let registry = self.locked_registry.clone();
        Ok(Box::new(TestSessionRuntime::new(
            snapshot,
            Box::new(move || {
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("dispose:{log_id}"));
                registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&log_id);
            }),
        )))
    }

    /// Release the lock for a session after its runtime is dropped
    /// (upstream the onDispose closure; explicit in the port).
    pub fn release_lock(&mut self, id: &str) {
        self.locked.remove(id);
        self.locked_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    /// Concrete-typed variants for tests and drivers that need the
    /// scripted surface (finish_prompt, steers, emit_*).
    pub fn acquire_test_runtime(&mut self, id: &str) -> Result<TestSessionRuntime, ServerError> {
        let stored = self.sessions.get(id).cloned();
        let Some(stored) = stored else {
            return Err(ServerError::new(
                ProtocolErrorCode::NotFound,
                format!("Unknown session: {id}"),
            ));
        };
        if self.is_locked(id) {
            return Err(ServerError::new(
                ProtocolErrorCode::SessionLocked,
                format!("Session is locked: {id}"),
            ));
        }
        self.lock_session(id);
        let snapshot = stored.clone();
        let log = self.event_log.clone();
        let log_id = id.to_string();
        let registry = self.locked_registry.clone();
        Ok(TestSessionRuntime::new(
            snapshot,
            Box::new(move || {
                log.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("dispose:{log_id}"));
                registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&log_id);
            }),
        ))
    }

    /// Recorded runtime lifecycle events ("dispose:<id>"); the port's
    /// stand-in for upstream's per-runtime disposeCount inspection.
    pub fn dispose_count(&self, id: &str) -> usize {
        self.event_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|entry| entry.as_str() == format!("dispose:{id}").as_str())
            .count()
    }

    /// Upstream `PiServerService.listModels` (part of the service
    /// boundary; the port's trait omits it so it lives here).
    pub fn list_models(&self) -> Vec<ModelMetadata> {
        vec![test_model()]
    }
}

impl PiServerService for TestServerService {
    fn list_sessions(&self) -> Vec<SessionMetadata> {
        if self.pending_list_delay {
            return Vec::new();
        }
        self.sessions
            .values()
            .map(|snapshot| SessionMetadata {
                id: snapshot.id.clone(),
                created_at: snapshot.created_at,
                updated_at: Some(snapshot.updated_at),
                session_name: snapshot.name.clone(),
                cwd: Some(snapshot.cwd.clone()),
                parent_session_id: None,
            })
            .collect()
    }

    fn create_session(
        &mut self,
        options: &CreateSessionOptions,
    ) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
        self.last_created_id = Some(options.id.clone());
        if self.sessions.contains_key(&options.id) {
            return Err(ServerError::new(
                ProtocolErrorCode::SessionLocked,
                "Session already exists",
            ));
        }
        self.seed(
            &options.id,
            options.name.as_deref().unwrap_or("Session"),
            options
                .cwd
                .as_deref()
                .unwrap_or("/tmp/pi-server-conformance"),
            options.model.clone().unwrap_or_else(|| {
                let model = test_model();
                ModelRef {
                    provider: model.provider,
                    id: model.id,
                }
            }),
            options.thinking_level.unwrap_or(ThinkingLevel::Off),
        );
        self.acquire(&options.id)
    }

    fn open_session(&mut self, session_id: &str) -> Result<Box<dyn PiSessionRuntime>, ServerError> {
        if !self.sessions.contains_key(session_id) {
            return Err(ServerError::new(
                ProtocolErrorCode::NotFound,
                format!("Unknown session: {session_id}"),
            ));
        }
        if self.is_locked(session_id) {
            return Err(ServerError::new(
                ProtocolErrorCode::SessionLocked,
                format!("Session is locked: {session_id}"),
            ));
        }
        self.acquire(session_id)
    }
}

/// Upstream `createTestServer`: an unstarted server with deterministic
/// defaults. The port returns the service alongside since the server
/// drives it directly.
pub fn create_test_server() -> TestServerService {
    TestServerService::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> TestServerService {
        let mut service = TestServerService::new();
        service.seed_default("session-1");
        service
    }

    fn session_ref() -> ModelRef {
        let model = test_model();
        ModelRef {
            provider: model.provider,
            id: model.id,
        }
    }

    /// Upstream anchor: listSessions projects snapshot fields into
    /// SessionMetadata (cwd → Option, updatedAt present).
    #[test]
    fn list_sessions_projects_snapshots() {
        let service = service();
        let sessions = service.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "session-1");
        assert_eq!(
            sessions[0].session_name.as_deref(),
            Some("Session session-1")
        );
        assert_eq!(
            sessions[0].cwd.as_deref(),
            Some("/tmp/pi-server-conformance")
        );
        assert_eq!(sessions[0].updated_at, Some(1));
    }

    /// Upstream anchor: seed defaults are deterministic (createdAt 1,
    /// revision 0, idle, unlocked).
    #[test]
    fn seed_defaults_are_deterministic() {
        let service = service();
        let snapshot = service.sessions.get("session-1").unwrap();
        assert_eq!(snapshot.created_at, 1);
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.phase, SessionPhase::Idle);
        assert!(!snapshot.attached);
        assert!(!snapshot.locked);
        assert_eq!(snapshot.model, session_ref());
        assert_eq!(snapshot.thinking_level, ThinkingLevel::Off);
        assert_eq!(service.list_models().len(), 1);
        assert_eq!(service.list_models()[0].id, "small");
    }

    /// Upstream anchor: openSession on an unknown session fails
    /// not_found; a locked session fails session_locked.
    #[test]
    fn open_session_gates_on_existence_and_lock() {
        let mut service = service();
        assert_eq!(
            match service.open_session("missing") {
                Ok(_) => panic!("expected error"),
                Err(error) => error.code,
            },
            ProtocolErrorCode::NotFound
        );
        let runtime = service.acquire_test_runtime("session-1").unwrap();
        assert!(service.is_locked("session-1"));
        assert_eq!(
            match service.open_session("session-1") {
                Ok(_) => panic!("expected error"),
                Err(error) => error.code,
            },
            ProtocolErrorCode::SessionLocked
        );
        drop(runtime);
        service.release_lock("session-1");
        assert!(service.open_session("session-1").is_ok());
    }

    /// Upstream anchor: createSession passes the server-assigned id to
    /// the service and rejects duplicates with session_locked.
    #[test]
    fn create_session_records_id_and_rejects_duplicates() {
        let mut service = TestServerService::new();
        let options = CreateSessionOptions {
            id: "abc".to_string(),
            cwd: Some("/work".to_string()),
            name: Some("Named".to_string()),
            model: None,
            thinking_level: None,
        };
        let _first = service.create_session(&options).unwrap();
        assert_eq!(service.last_created_id.as_deref(), Some("abc"));
        assert!(service.is_locked("abc"));
        assert_eq!(
            match service.create_session(&options) {
                Ok(_) => panic!("expected error"),
                Err(error) => error.code,
            },
            ProtocolErrorCode::SessionLocked
        );
        assert_eq!(
            match service.create_session(&options) {
                Ok(_) => panic!("expected error"),
                Err(error) => error.message,
            },
            "Session already exists"
        );
        service.release_lock("abc");
        let runtime = service.acquire_test_runtime("abc").unwrap();
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.name.as_deref(), Some("Named"));
        assert_eq!(snapshot.cwd, "/work");
        assert_eq!(snapshot.model, session_ref());
        assert!(service.is_locked("abc"));
    }

    /// Upstream anchor: prompt appends a user item, moves to turn, and
    /// applying the outcome appends the assistant reply with
    /// reply:<text>; each update bumps revision and updatedAt.
    #[test]
    fn prompt_complete_flow_bumps_revisions() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        runtime.prompt("hello").unwrap();
        let mid = runtime.snapshot();
        assert_eq!(mid.phase, SessionPhase::Turn);
        assert_eq!(mid.revision, 1);
        assert_eq!(mid.updated_at, 2);
        assert!(matches!(&mid.transcript[0], TranscriptItem::User { id, .. } if id == "user-1"));
        runtime.finish_prompt().unwrap();
        let done = runtime.snapshot();
        assert_eq!(done.phase, SessionPhase::Idle);
        assert_eq!(done.revision, 2);
        assert_eq!(done.transcript.len(), 2);
        match &done.transcript[1] {
            TranscriptItem::Assistant(assistant) => {
                assert_eq!(assistant.id, "assistant-2");
                assert_eq!(assistant.status, AssistantStatus::Complete);
                assert_eq!(assistant.stop_reason, Some(AssistantStopReason::Stop));
                match &assistant.content[0] {
                    AssistantContent::Text { text } => assert_eq!(text, "reply:hello"),
                    other => panic!("unexpected content: {other:?}"),
                }
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    /// Upstream anchor: abort produces an empty aborted assistant item
    /// with stopReason aborted.
    #[test]
    fn abort_flow() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        runtime.prompt("hi").unwrap();
        runtime.abort().unwrap();
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.phase, SessionPhase::Idle);
        match &snapshot.transcript[1] {
            TranscriptItem::Assistant(assistant) => {
                assert_eq!(assistant.status, AssistantStatus::Aborted);
                assert_eq!(assistant.stop_reason, Some(AssistantStopReason::Aborted));
                match &assistant.content[0] {
                    AssistantContent::Text { text } => assert!(text.is_empty()),
                    other => panic!("unexpected content: {other:?}"),
                }
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    /// Upstream anchor: prompt while busy fails; steer while idle
    /// fails; steer while turn queues; abort without prompt fails.
    #[test]
    fn busy_gates_and_steer_queue() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        assert_eq!(
            match runtime.steer("x") {
                Ok(_) => panic!("expected error"),
                Err(error) => error.message,
            },
            "There is no active prompt to steer"
        );
        runtime.prompt("first").unwrap();
        assert_eq!(
            match runtime.prompt("second") {
                Ok(_) => panic!("expected error"),
                Err(error) => error.message,
            },
            "A prompt is already running"
        );
        runtime.steer("steer-1").unwrap();
        runtime.steer("steer-2").unwrap();
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.queued_steer_count, 2);
        assert_eq!(snapshot.queued_steer.len(), 2);
        assert!(runtime.steers == vec!["steer-1".to_string(), "steer-2".to_string()]);
        runtime.finish_prompt().unwrap();
        // steers recorded on the runtime; snapshot queue remains until
        // the host drains it (upstream keeps queuedSteer).
        assert_eq!(runtime.snapshot().queued_steer.len(), 2);
        runtime.abort().unwrap_err();
    }

    /// Upstream anchor: setModel/setThinking only while idle.
    #[test]
    fn set_model_and_thinking_gate_on_idle() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        runtime
            .set_model(&ModelRef {
                provider: "other".to_string(),
                id: "big".to_string(),
            })
            .unwrap();
        runtime.set_thinking(&ThinkingLevel::High).unwrap();
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.model.id, "big");
        assert_eq!(snapshot.thinking_level, ThinkingLevel::High);
        runtime.prompt("x").unwrap();
        assert_eq!(
            runtime.set_model(&session_ref()).unwrap_err().message,
            "Session is busy"
        );
        assert_eq!(
            runtime
                .set_thinking(&ThinkingLevel::Off)
                .unwrap_err()
                .message,
            "Session is busy"
        );
    }

    /// Upstream anchor: dispose counts; snapshot events broadcast to
    /// subscribers; progress and error events fan out.
    #[test]
    fn events_and_dispose_counting() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.listeners.push(sender);
        runtime.prompt("x").unwrap();
        runtime.finish_prompt().unwrap();
        runtime.emit_progress(TranscriptProgress::AssistantDelta {
            message_id: "assistant-2".to_string(),
            content_index: 0,
            kind: pillar_protocol::schemas::DeltaKind::Text,
            delta: "chunk".to_string(),
        });
        runtime.emit_error(ServerError::new(ProtocolErrorCode::Busy, "no"));
        runtime.set_phase(SessionPhase::Retry);
        assert_eq!(runtime.snapshot().phase, SessionPhase::Retry);
        assert_eq!(runtime.dispose_count(), 0);
        runtime.dispose();
        assert_eq!(runtime.dispose_count(), 1);
        runtime.dispose();
        assert_eq!(runtime.dispose_count(), 2);
        let events: Vec<RuntimeEvent> = receiver.try_iter().collect();
        // Two snapshot events (prompt + finish) + progress + error.
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], RuntimeEvent::Snapshot));
        assert!(matches!(&events[2], RuntimeEvent::Progress(_)));
        assert!(matches!(events[3], RuntimeEvent::Error(_)));
    }

    /// Upstream anchor: delayNextList holds the next list until
    /// released (the port models it as a flag).
    #[test]
    fn list_delay_flag() {
        let mut service = service();
        service.delay_next_list();
        assert!(service.list_sessions().is_empty());
        assert!(!service.list_delay_entered);
        service.release_list_delay();
        assert_eq!(service.list_sessions().len(), 1);
    }

    /// Upstream anchor: finishPrompt without a pending prompt is an
    /// error.
    #[test]
    fn finish_without_prompt_fails() {
        let mut service = service();
        let mut runtime = service.acquire_test_runtime("session-1").unwrap();
        assert!(runtime.finish_prompt().is_err());
    }
}
