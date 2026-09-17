//! Where the port waits.
//!
//! Retry backoff, provider timeouts, and the faux provider all need a delay.
//! Natively that is `tokio::time`, which needs a tokio timer driver — something
//! a Wasm embedding host does not have (docs/DEVELOPMENT-STRATEGY.md §5-2). A
//! host with its own clock (a game frame loop, a `setTimeout`-style queue, a
//! test harness) installs a [`SleepFn`] here and the provider code never
//! touches tokio's timer.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A host-supplied delay: resolves once `duration` elapsed on the host's clock.
pub type SleepFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;
/// Starts one delay (see [`set_default_sleep`]).
pub type SleepFn = Arc<dyn Fn(Duration) -> SleepFuture + Send + Sync>;

/// Returned by [`timeout`] when the host's clock ran out first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// A host-supplied wall clock: milliseconds since the Unix epoch.
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

static SLEEP: RwLock<Option<SleepFn>> = RwLock::new(None);
static NOW: RwLock<Option<NowFn>> = RwLock::new(None);

/// Install the host's wall clock (`None` restores the platform default).
///
/// A Wasm host has no system clock: `std::time::SystemTime::now` traps there,
/// so an embedding host either installs its own (a frame clock) or the port
/// reports time `0` — deterministic, never a trap.
pub fn set_default_now(now: Option<NowFn>) {
    *NOW.write().expect("now lock") = now;
}

/// The installed host wall clock, if any.
pub fn get_default_now() -> Option<NowFn> {
    NOW.read().expect("now lock").clone()
}

/// Milliseconds since the Unix epoch, from the host's clock when it has one.
pub fn now_millis() -> i64 {
    if let Some(now) = get_default_now() {
        return now();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        // No system clock on this target; a host that needs real timestamps
        // installs one with `set_default_now`.
        0
    }
}

/// Install the host's timer (`None` restores the platform default).
pub fn set_default_sleep(sleep: Option<SleepFn>) {
    *SLEEP.write().expect("sleep lock") = sleep;
}

/// The installed host timer, if any.
pub fn get_default_sleep() -> Option<SleepFn> {
    SLEEP.read().expect("sleep lock").clone()
}

/// Wait for `duration` on the host's timer.
///
/// Without one the platform default applies: `tokio::time::sleep` on native.
/// On `wasm32` there is no timer driver to wait on, so this panics with the
/// actionable message instead of hanging.
pub fn sleep(duration: Duration) -> SleepFuture {
    if let Some(sleep) = get_default_sleep() {
        return sleep(duration);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Box::pin(tokio::time::sleep(duration))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = duration;
        panic!(
            "no timer installed: a Wasm host must call `pillar_ai::set_default_sleep` because \
             this target has no tokio timer driver"
        );
    }
}

/// Run `future`, giving up when the host's clock has advanced by `duration`.
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    let sleep = sleep(duration);
    futures::pin_mut!(sleep);
    futures::pin_mut!(future);
    match futures::future::select(future, sleep).await {
        futures::future::Either::Left((output, _)) => Ok(output),
        futures::future::Either::Right(((), _)) => Err(Elapsed),
    }
}
