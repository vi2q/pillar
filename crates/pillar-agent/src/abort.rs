//! Abort signal primitive for the agent runtime.
//!
//! divergence: Rust has no AbortSignal; this is a lightweight shared flag
//! with async wait support. Every place pi threads an `AbortSignal` uses
//! this type. Cloning shares state; `abort()` is idempotent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Default, Clone)]
pub struct AbortSignal {
    inner: Arc<AbortInner>,
}

#[derive(Debug, Default)]
struct AbortInner {
    aborted: AtomicBool,
    wakers: std::sync::Mutex<Vec<std::task::Waker>>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        let was = self.inner.aborted.swap(true, Ordering::SeqCst);
        if !was {
            let mut wakers = self.inner.wakers.lock().expect("abort wakers lock");
            for waker in wakers.drain(..) {
                waker.wake();
            }
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    /// Resolves when the signal aborts. Returns immediately if already aborted.
    pub async fn aborted(&self) {
        AbortedFuture {
            signal: self.clone(),
        }
        .await
    }
}

struct AbortedFuture {
    signal: AbortSignal,
}

impl Future for AbortedFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.signal.is_aborted() {
            return std::task::Poll::Ready(());
        }
        let mut wakers = self.signal.inner.wakers.lock().expect("abort wakers lock");
        // Re-check after taking the lock: abort() may have run.
        if self.signal.is_aborted() {
            return std::task::Poll::Ready(());
        }
        wakers.push(cx.waker().clone());
        std::task::Poll::Pending
    }
}

impl Future for AbortSignal {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.is_aborted() {
            return std::task::Poll::Ready(());
        }
        let mut wakers = self.inner.wakers.lock().expect("abort wakers lock");
        if self.is_aborted() {
            return std::task::Poll::Ready(());
        }
        wakers.push(cx.waker().clone());
        std::task::Poll::Pending
    }
}

use std::future::Future;
