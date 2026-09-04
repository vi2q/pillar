//! Port of packages/client/src/session-handle.ts (pi v0.84.3): a typed
//! per-session handle over the client state — prompt/steer/abort/
//! set-model/set-thinking commands, attachment queries, and snapshot
//! subscription.
//!
//! divergences: the request pipeline is a host-supplied closure
//! (upstream the connection's request/response promise); disposal is
//! a closure like upstream's AsyncDisposable.

use pillar_protocol::schemas::{Command, ModelRef, SessionSnapshot, ThinkingLevel};

use crate::ClientState;

/// Lease mode (upstream `SessionLeaseMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLeaseMode {
    Shared,
    Exclusive,
}

/// Session lease options (upstream `AcquireSessionOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireSessionOptions {
    pub mode: SessionLeaseMode,
}

impl Default for AcquireSessionOptions {
    fn default() -> Self {
        Self {
            mode: SessionLeaseMode::Shared,
        }
    }
}

/// The outcome of a session command request (upstream the resolved
/// ResultForCommand).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRequestOutcome {
    Snapshot(SessionSnapshot),
    /// The request failed (transport or server error message).
    Failed(String),
}

/// Session handle callbacks (upstream `SessionHandleCallbacks`).
pub struct SessionHandleCallbacks<'a> {
    pub state: &'a mut ClientState,
    /// Issue a session command and return the outcome (upstream the
    /// request callback over the connection).
    pub request: &'a mut dyn FnMut(Command) -> SessionRequestOutcome,
}

/// A per-session handle (upstream `SessionHandle`).
pub struct SessionHandle {
    pub id: String,
}

impl SessionHandle {
    pub fn new(id: &str) -> Self {
        Self { id: id.to_string() }
    }

    /// Whether the session is attached (upstream `attached`/`active`).
    pub fn attached(&self, callbacks: &SessionHandleCallbacks<'_>) -> bool {
        callbacks.state.is_session_attached(&self.id)
    }

    /// The current snapshot id-checked clone (upstream `snapshot`
    /// getter returns the shared reference).
    pub fn snapshot(&self, callbacks: &SessionHandleCallbacks<'_>) -> Option<SessionSnapshot> {
        callbacks.state.get_session_snapshot(&self.id).cloned()
    }

    /// Issue prompt/steer/abort/set-model/set-thinking, returning the
    /// updated snapshot (upstream the individual methods — each
    /// returns result.session).
    pub fn execute(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
        command: Command,
    ) -> Result<SessionSnapshot, String> {
        match (callbacks.request)(command) {
            SessionRequestOutcome::Snapshot(snapshot) => Ok(snapshot),
            SessionRequestOutcome::Failed(message) => Err(message),
        }
    }

    pub fn prompt(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
        text: &str,
    ) -> Result<SessionSnapshot, String> {
        self.execute(
            callbacks,
            Command::Prompt {
                session_id: self.id.clone(),
                text: text.to_string(),
            },
        )
    }

    pub fn steer(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
        text: &str,
    ) -> Result<SessionSnapshot, String> {
        self.execute(
            callbacks,
            Command::Steer {
                session_id: self.id.clone(),
                text: text.to_string(),
            },
        )
    }

    pub fn abort(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
    ) -> Result<SessionSnapshot, String> {
        self.execute(
            callbacks,
            Command::Abort {
                session_id: self.id.clone(),
            },
        )
    }

    pub fn set_model(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
        model: ModelRef,
    ) -> Result<SessionSnapshot, String> {
        self.execute(
            callbacks,
            Command::SetModel {
                session_id: self.id.clone(),
                model,
            },
        )
    }

    pub fn set_thinking(
        &self,
        callbacks: &mut SessionHandleCallbacks<'_>,
        thinking_level: ThinkingLevel,
    ) -> Result<SessionSnapshot, String> {
        self.execute(
            callbacks,
            Command::SetThinking {
                session_id: self.id.clone(),
                thinking_level,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PiSessionDetachedError;
    use pillar_protocol::schemas::{CommandResult, ServerEvent};
    use pillar_protocol::schemas::{ModelRef, SessionPhase};

    fn snapshot(id: &str, revision: u64, attached: bool) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            name: None,
            cwd: "/tmp".to_string(),
            created_at: 0,
            updated_at: 0,
            phase: SessionPhase::Idle,
            model: ModelRef {
                provider: "p".to_string(),
                id: "m".to_string(),
            },
            thinking_level: ThinkingLevel::Off,
            attached,
            locked: false,
            revision,
            transcript: vec![],
            queued_steer: vec![],
            queued_steer_count: 0,
        }
    }

    fn make_handle(attached: bool) -> (SessionHandle, ClientState) {
        let handle = SessionHandle::new("a");
        let mut state = ClientState::new();
        state.apply_result(&CommandResult::Attach {
            session: snapshot("a", 1, attached),
        });
        (handle, state)
    }

    #[test]
    fn attached_and_snapshot_read_from_state() {
        let (handle, mut state) = make_handle(true);
        let callbacks = SessionHandleCallbacks {
            state: &mut state,
            request: &mut |_| unreachable!(),
        };
        assert!(handle.attached(&callbacks));
        assert_eq!(handle.snapshot(&callbacks).unwrap().revision, 1);
    }

    #[test]
    fn prompt_builds_command_and_returns_session() {
        let (handle, mut state) = make_handle(true);
        let mut sent = Vec::new();
        let mut request = |command: Command| {
            sent.push(command.clone());
            match command {
                Command::Prompt { session_id, text } => {
                    assert_eq!(session_id, "a");
                    assert_eq!(text, "hi");
                }
                _ => panic!("expected prompt"),
            }
            SessionRequestOutcome::Snapshot(snapshot("a", 2, true))
        };
        let mut callbacks = SessionHandleCallbacks {
            state: &mut state,
            request: &mut request,
        };
        let snapshot = handle.prompt(&mut callbacks, "hi").unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(sent.len(), 1);
    }

    #[test]
    fn all_session_commands_map_correctly() {
        let (handle, mut state) = make_handle(true);
        let mut commands = Vec::new();
        let mut request = |command: Command| {
            commands.push(command.clone());
            SessionRequestOutcome::Snapshot(snapshot("a", 2, true))
        };
        let mut callbacks = SessionHandleCallbacks {
            state: &mut state,
            request: &mut request,
        };
        let _ = handle.steer(&mut callbacks, "s");
        let _ = handle.abort(&mut callbacks);
        let _ = handle.set_model(
            &mut callbacks,
            ModelRef {
                provider: "p".to_string(),
                id: "m2".to_string(),
            },
        );
        let _ = handle.set_thinking(&mut callbacks, ThinkingLevel::High);
        assert!(matches!(commands[0], Command::Steer { .. }));
        assert!(matches!(commands[1], Command::Abort { .. }));
        assert!(matches!(commands[2], Command::SetModel { .. }));
        assert!(matches!(commands[3], Command::SetThinking { .. }));
    }

    #[test]
    fn failed_request_propagates_message() {
        let (handle, mut state) = make_handle(true);
        let mut callbacks = SessionHandleCallbacks {
            state: &mut state,
            request: &mut |_| SessionRequestOutcome::Failed("server error".to_string()),
        };
        assert_eq!(
            handle.prompt(&mut callbacks, "hi"),
            Err("server error".to_string())
        );
    }

    #[test]
    fn detached_error_message_matches_upstream() {
        let error = PiSessionDetachedError::new("abc");
        assert_eq!(error.to_string(), "Session abc is not attached");
    }

    #[test]
    fn lease_mode_defaults_to_shared() {
        assert_eq!(
            AcquireSessionOptions::default().mode,
            SessionLeaseMode::Shared
        );
    }

    #[test]
    fn events_flow_through_state_subscription() {
        let (handle, mut state) = make_handle(true);
        let received: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let sink = received.clone();
        {
            let callbacks = SessionHandleCallbacks {
                state: &mut state,
                request: &mut |_| unreachable!(),
            };
            let listener_state = &mut *callbacks.state;
            let _handle = listener_state.on_session_event(
                "a",
                Box::new(move |event| {
                    if let ServerEvent::SessionSnapshot { snapshot } = event {
                        sink.lock().unwrap().push(snapshot.revision);
                    }
                }),
            );
        }
        state.apply_event(&ServerEvent::SessionSnapshot {
            snapshot: snapshot("a", 3, true),
        });
        assert_eq!(received.lock().unwrap().as_slice(), [3]);
        assert_eq!(handle.id, "a");
    }
}
