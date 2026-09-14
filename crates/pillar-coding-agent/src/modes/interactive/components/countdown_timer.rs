//! Port of components/countdown-timer.ts: a one-second countdown for dialog
//! components.
//!
//! divergence: upstream owns a `setInterval` and calls `tui.requestRender()`
//! itself. The port is host-driven: [`CountdownTimer::tick`] is called once per
//! second by the host and answers whether a render is needed.

use std::sync::Arc;

/// A countdown that reports each remaining second (upstream `CountdownTimer`).
pub struct CountdownTimer {
    remaining_seconds: i64,
    on_tick: Box<dyn FnMut(i64) + Send>,
    on_expire: Box<dyn FnMut() + Send>,
    disposed: bool,
}

impl CountdownTimer {
    /// Start a countdown of `timeout_ms`; the first tick fires immediately
    /// (upstream's constructor).
    pub fn new(
        timeout_ms: u64,
        mut on_tick: Box<dyn FnMut(i64) + Send>,
        on_expire: Box<dyn FnMut() + Send>,
    ) -> Self {
        let remaining_seconds = timeout_ms.div_ceil(1000) as i64;
        on_tick(remaining_seconds);
        Self {
            remaining_seconds,
            on_tick,
            on_expire,
            disposed: false,
        }
    }

    /// The seconds left before expiry.
    pub fn remaining_seconds(&self) -> i64 {
        self.remaining_seconds
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    /// Advance one second (upstream's interval body). Returns whether the host
    /// should re-render; expiry disposes the timer and fires `on_expire`.
    pub fn tick(&mut self) -> bool {
        if self.disposed {
            return false;
        }
        self.remaining_seconds -= 1;
        (self.on_tick)(self.remaining_seconds);
        if self.remaining_seconds <= 0 {
            self.dispose();
            (self.on_expire)();
        }
        true
    }

    /// Stop the countdown (upstream `dispose`).
    pub fn dispose(&mut self) {
        self.disposed = true;
    }
}

/// Convenience: build a timer whose ticks are recorded in a shared counter and
/// whose expiry flips a flag (used by tests and simple hosts).
pub fn simple_countdown(
    timeout_ms: u64,
) -> (CountdownTimer, Arc<std::sync::Mutex<Vec<i64>>>) {
    let ticks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&ticks);
    let timer = CountdownTimer::new(
        timeout_ms,
        Box::new(move |seconds| {
            if let Ok(mut ticks) = sink.lock() {
                ticks.push(seconds);
            }
        }),
        Box::new(|| {}),
    );
    (timer, ticks)
}
