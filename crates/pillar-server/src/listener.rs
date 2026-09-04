//! Port of packages/server/src/listener.ts + the listener
//! composition of server.ts (pi v0.84.3): the `PiServerListener`
//! trait and `ListenerGroup` — start/close ordering with failure
//! rollback (previously started listeners close when a later startup
//! fails) — plus the Unix listener filesystem lifecycle decision core
//! from the upstream unix listener (bind guarding, stale-socket
//! removal rules) expressed as path decisions.
//!
//! divergences: real socket bind/unlink/permissions are host-side;
//! this module supplies the composition and decision logic and the
//! test fixtures stand in for transports.

/// A server listener (upstream `PiServerListener`).
pub trait ServerListener: std::any::Any {
    /// Downcast support for test assertions.
    fn as_any(&self) -> &dyn std::any::Any;
    /// Human-readable bound address after startup, if any.
    fn address(&self) -> Option<String>;
    /// Start listening; connections go to the host acceptor.
    fn start(&mut self, accept: &mut dyn FnMut()) -> Result<(), String>;
    /// Close the listener.
    fn close(&mut self);
}

/// A scripted listener for conformance tests (upstream
/// `TestListener`).
pub struct TestListener {
    pub address: Option<String>,
    pub start_count: usize,
    pub close_count: usize,
    pub start_error: Option<String>,
}

impl TestListener {
    pub fn new(address: &str) -> Self {
        Self {
            address: Some(address.to_string()),
            start_count: 0,
            close_count: 0,
            start_error: None,
        }
    }

    pub fn failing(address: &str, error: &str) -> Self {
        Self {
            start_error: Some(error.to_string()),
            ..Self::new(address)
        }
    }
}

impl ServerListener for TestListener {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn address(&self) -> Option<String> {
        self.address.clone()
    }

    fn start(&mut self, _accept: &mut dyn FnMut()) -> Result<(), String> {
        self.start_count += 1;
        match self.start_error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn close(&mut self) {
        self.close_count += 1;
        self.address = None;
    }
}

/// Result of a [`start_listeners`] run: bound addresses (upstream
/// `server.addresses`).
#[derive(Debug)]
pub struct StartedListeners {
    pub addresses: Vec<String>,
}

/// Start every configured listener in order (upstream
/// `startInternal`): on a failure, previously started listeners are
/// closed and the error propagates.
pub fn start_listeners(
    listeners: &mut [Box<dyn ServerListener>],
    accept: &mut dyn FnMut(),
) -> Result<StartedListeners, String> {
    let mut addresses = Vec::new();
    let mut started = 0usize;
    for listener in listeners.iter_mut() {
        match listener.start(accept) {
            Ok(()) => {
                if let Some(address) = listener.address() {
                    addresses.push(address);
                }
                started += 1;
            }
            Err(error) => {
                for listener in listeners.iter_mut().take(started) {
                    listener.close();
                }
                return Err(error);
            }
        }
    }
    Ok(StartedListeners { addresses })
}

/// Close every listener (upstream `closeServerState`): each close
/// runs exactly once, in order.
pub fn close_listeners(listeners: &mut [Box<dyn ServerListener>]) {
    for listener in listeners.iter_mut() {
        listener.close();
    }
}

/// The Unix socket path bind decision (upstream the unix listener's
/// bind guard): refuse a live listener (report "already running"),
/// refuse a regular file (report "non-socket"), remove a genuinely
/// stale socket, and create missing parents. The host probe reports
/// what exists at the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    /// No file at the path.
    Missing,
    /// A socket that no live process owns (connect failed).
    StaleSocket,
    /// A socket with a live listener.
    LiveSocket,
    /// A non-socket file.
    RegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindDecision {
    /// Nothing to clean; bind directly.
    Bind,
    /// Unlink the stale socket first, then bind.
    UnlinkThenBind,
}

/// Decide how to prepare the socket path (upstream the unix
/// listener's start guard).
pub fn bind_decision(state: PathState) -> Result<BindDecision, String> {
    match state {
        // Missing paths bind directly (upstream skips the unlink when
        // nothing exists); stale sockets are unlinked first.
        PathState::Missing => Ok(BindDecision::Bind),
        PathState::StaleSocket => Ok(BindDecision::UnlinkThenBind),
        PathState::LiveSocket => Err("Socket path is already running a listener".to_string()),
        PathState::RegularFile => Err("Socket path is a non-socket file".to_string()),
    }
}

/// Whether the socket should be unlinked at shutdown (upstream the
/// unix listener's close guard): only the socket this process created
/// (matching inode identity) is removed; a replacement inode is left
/// alone.
pub fn should_unlink_at_close(created: bool, path_state: PathState) -> bool {
    match path_state {
        PathState::Missing => created,
        PathState::LiveSocket => created,
        PathState::StaleSocket => false,
        PathState::RegularFile => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listeners() -> Vec<Box<dyn ServerListener>> {
        vec![
            Box::new(TestListener::new("first")),
            Box::new(TestListener::new("second")),
        ]
    }

    /// Upstream "starts and closes every configured listener".
    #[test]
    fn starts_and_closes_every_configured_listener() {
        let mut group = listeners();
        {
            let mut accept = || {};
            let started = start_listeners(&mut group, &mut accept).unwrap();
            assert_eq!(
                started.addresses,
                vec!["first".to_string(), "second".to_string()]
            );
        }
        // Each listener received the acceptor (start_count 1).
        let start_counts: Vec<usize> = group
            .iter()
            .map(|listener| {
                listener
                    .as_any()
                    .downcast_ref::<TestListener>()
                    .map(|test| test.start_count)
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(start_counts, vec![1, 1]);
        close_listeners(&mut group);
        let close_counts: Vec<usize> = group
            .iter()
            .map(|listener| {
                listener
                    .as_any()
                    .downcast_ref::<TestListener>()
                    .map(|test| test.close_count)
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(close_counts, vec![1, 1]);
        for listener in &group {
            assert_eq!(listener.address(), None);
        }
    }

    /// Upstream "closes previously started listeners when startup
    /// fails".
    #[test]
    fn closes_previously_started_listeners_on_startup_failure() {
        let mut group: Vec<Box<dyn ServerListener>> = vec![
            Box::new(TestListener::new("first")),
            Box::new(TestListener::failing("second", "listener failed")),
        ];
        let mut accept = || {};
        let error = start_listeners(&mut group, &mut accept).unwrap_err();
        assert_eq!(error, "listener failed");
        // The first listener rolled back; the failing one never closed.
        let close_count = |index: usize| {
            group[index]
                .as_any()
                .downcast_ref::<TestListener>()
                .map(|test| test.close_count)
                .unwrap_or(0)
        };
        assert_eq!(close_count(0), 1);
        assert_eq!(close_count(1), 0);
    }

    /// Upstream "rejects a live listener without unlinking it" and
    /// "never unlinks a regular file at the configured path".
    #[test]
    fn bind_guard_rejects_live_sockets_and_regular_files() {
        assert_eq!(
            bind_decision(PathState::LiveSocket).unwrap_err(),
            "Socket path is already running a listener"
        );
        assert_eq!(
            bind_decision(PathState::RegularFile).unwrap_err(),
            "Socket path is a non-socket file"
        );
        assert_eq!(bind_decision(PathState::Missing), Ok(BindDecision::Bind));
        assert_eq!(
            bind_decision(PathState::StaleSocket),
            Ok(BindDecision::UnlinkThenBind)
        );
    }

    /// Upstream "does not remove a replacement inode during
    /// shutdown" and "removes a genuinely stale socket before
    /// binding".
    #[test]
    fn shutdown_unlink_respects_inode_identity() {
        // A socket this process created is removed at close.
        assert!(should_unlink_at_close(true, PathState::LiveSocket));
        // A missing path this process was bound to: nothing to remove.
        assert!(should_unlink_at_close(true, PathState::Missing));
        // A replacement inode or foreign socket is never removed.
        assert!(!should_unlink_at_close(false, PathState::RegularFile));
        assert!(!should_unlink_at_close(false, PathState::StaleSocket));
        assert!(!should_unlink_at_close(false, PathState::LiveSocket));
    }
}
