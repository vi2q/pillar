//! Where the port starts the background bodies of the loop and proxy streams.
//!
//! Upstream runs `runAgentLoop` in an async IIFE and pushes its events into a
//! stream the caller consumes; the Rust port drives the same body on a
//! background task and returns the stream. Natively that task is
//! `tokio::spawn`, which needs a tokio reactor — something the Wasm
//! embedding profiles do not have (docs/DEVELOPMENT-STRATEGY.md §5-2). A host
//! with its own executor (a `spawn_local`-style microtask queue, a game frame
//! loop, a test harness) therefore supplies a [`SpawnFn`] and the loop never
//! touches tokio.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A spawned body: the loop's run future, boxed and detached.
pub type Spawned = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// How a host starts a background body. The body is `Send` because the native
/// default runs it on a multi-threaded runtime; a single-threaded host may
/// still poll it on one thread (it never migrates by itself).
pub type SpawnFn = Arc<dyn Fn(Spawned) + Send + Sync>;

/// Start `body`, using the caller's spawner when it has one.
///
/// Without a spawner the platform default applies: `tokio::spawn` on native
/// targets. On `wasm32` there is no reactor to spawn onto, so this panics with
/// the actionable message instead of hanging the returned stream forever.
pub fn spawn_background(
    spawner: Option<&SpawnFn>,
    body: impl Future<Output = ()> + Send + 'static,
) {
    let body: Spawned = Box::pin(body);
    if let Some(spawner) = spawner {
        spawner(body);
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::spawn(body);
    }
    #[cfg(target_arch = "wasm32")]
    {
        drop(body);
        panic!(
            "no spawner installed: a Wasm host must pass `AgentLoopConfig::spawn` (or set it \
             through `AgentOptions::spawn`) because this target has no tokio reactor"
        );
    }
}
