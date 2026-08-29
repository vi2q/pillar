//! Port of packages/ai/src/utils/abort.ts (pi v0.84.3) — abort-signal
//! plumbing for cancellable operations, in tokio terms.
//!
//! divergence: upstream uses the DOM AbortSignal/AbortController; the Rust
//! port uses a lightweight cloneable signal backed by a tokio watch channel
//! with the same observable semantics (aborted flag, reason, racing).

use std::sync::Arc;

/// Reason attached to an abort. Upstream uses the signal's `reason` (any
/// thrown value); the Rust port models the common cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    /// Upstream default: `Error("The operation was aborted")` named AbortError.
    Aborted,
    /// Explicit reason string (e.g. timeout, superseded refresh).
    Custom(String),
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortReason::Aborted => write!(f, "The operation was aborted"),
            AbortReason::Custom(msg) => write!(f, "{msg}"),
        }
    }
}

/// (kept for future typed-state refactor; currently the watch channel carries
/// abort state directly)
#[allow(dead_code)]
struct AbortSignalState {
    aborted: bool,
    reason: Option<AbortReason>,
}

/// A cloneable abort signal. Clones share abort state, mirroring how one
/// DOM signal fans out to many listeners.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    state: Arc<tokio::sync::watch::Sender<Option<AbortReason>>>,
}

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortSignal {
    /// Fresh, never-aborted signal.
    pub fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(None);
        Self {
            state: Arc::new(tx),
        }
    }

    /// Signal already in the aborted state.
    pub fn aborted(reason: Option<AbortReason>) -> Self {
        let signal = Self::new();
        signal.abort(reason);
        signal
    }

    /// Combines several signals: the composite aborts when any input aborts
    /// (mirrors upstream `AbortSignal.any`).
    pub fn any(signals: &[AbortSignal]) -> Self {
        let composite = AbortSignal::new();
        for signal in signals {
            if signal.is_aborted() {
                composite.abort(signal.reason());
                return composite;
            }
        }
        let mut rx = composite.state.subscribe();
        tokio::spawn(async move {
            let _ = rx.changed().await;
        });
        // Subscribe each input; first abort propagates.
        for signal in signals {
            let mut input_rx = signal.state.subscribe();
            let composite_tx = Arc::clone(&composite.state);
            let _ = input_rx;
            let composite_rx = composite.state.subscribe();
            let _ = composite_rx;
            tokio::spawn(async move {
                if input_rx.changed().await.is_ok() {
                    let reason = input_rx.borrow().clone();
                    let _ = composite_tx.send(reason);
                }
            });
        }
        composite
    }

    /// Timeout signal (mirrors upstream `AbortSignal.timeout`).
    pub fn timeout(duration: std::time::Duration) -> Self {
        let signal = AbortSignal::new();
        let tx = Arc::clone(&signal.state);
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            let _ = tx.send(Some(AbortReason::Custom("TimeoutError".to_string())));
        });
        signal
    }

    /// Aborts the signal with an optional reason.
    pub fn abort(&self, reason: Option<AbortReason>) {
        let _ = self
            .state
            .send(Some(reason.unwrap_or(AbortReason::Aborted)));
    }

    pub fn is_aborted(&self) -> bool {
        self.state.borrow().is_some()
    }

    /// Returns a clone of the abort reason, if any (avoids borrowing the
    /// temporary watch guard across the return boundary).
    pub fn reason(&self) -> Option<AbortReason> {
        self.state.borrow().clone()
    }

    /// Waits until the signal aborts.
    pub async fn aborted_or_pending(&self) -> AbortReason {
        let mut rx = self.state.subscribe();
        loop {
            if let Some(reason) = rx.borrow().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return AbortReason::Aborted;
            }
        }
    }

    /// Upstream `signal.throwIfAborted()`: raises the abort as an error.
    pub fn throw_if_aborted(&self) -> Result<(), crate::error::AiError> {
        match self.reason() {
            Some(reason) => Err(crate::error::AiError::Aborted(reason.to_string())),
            None => Ok(()),
        }
    }

    /// Waits for the operation, racing it against this signal. If the signal
    /// aborts first, returns `Err(Aborted)` (upstream rejects with the abort
    /// reason); the abandoned future keeps running but its result is ignored.
    pub async fn race<T, E, F>(&self, operation: F) -> Result<T, crate::error::AiError>
    where
        F: std::future::Future<Output = Result<T, E>>,
        crate::error::AiError: From<E>,
    {
        tokio::select! {
            biased;
            reason = self.aborted_or_pending() => Err(crate::error::AiError::Aborted(reason.to_string())),
            result = operation => result.map_err(crate::error::AiError::from),
        }
    }
}

/// Creates an operation-local signal for public APIs whose signal is
/// optional (upstream `operationSignal`).
pub fn operation_signal(signal: Option<&AbortSignal>) -> AbortSignal {
    signal.cloned().unwrap_or_default()
}
