//! Port of packages/telemetry/src/noop.ts (pi v0.84.3).
//!
//! Shared telemetry context used when an application does not provide one.
//! Admits callbacks synchronously, reuses one inert span, records nothing.

use std::future::Future;
use std::sync::Arc;

use crate::{
    ChildFn, ChildFuture, SpanAttributes, SpanHandle, SpanHandleInner, SpanOptions, SpanStarter,
    SpanStarterInner, SpanStatus, TelemetryContext,
};

/// One inert span shared by every noop start; starting children returns the
/// same handle, mirroring upstream's single frozen noop span.
#[derive(Debug, Default)]
pub struct NoopTelemetrySpan;

impl SpanHandleInner for NoopTelemetrySpan {
    fn add_event(&self, _: &str, _: SpanAttributes) {}
    fn set_attributes(&self, _: SpanAttributes) {}
    fn set_status(&self, _: SpanStatus) {}
    fn start_child_erased(&self, _options: SpanOptions, callback: ChildFn) -> ChildFuture {
        // The child of the noop span is the noop span itself.
        Box::pin(async move { (callback.0)(noop_handle(), noop_starter()).await })
    }
}

/// Shared telemetry context used when an application does not provide one.
#[derive(Debug, Default)]
pub struct NoopTelemetryContext;

impl TelemetryContext for NoopTelemetryContext {
    fn starter(&self) -> SpanStarter {
        noop_starter()
    }
}

#[derive(Debug, Default)]
struct NoopStarter;

impl SpanStarterInner for NoopStarter {
    fn start_erased(&self, _options: SpanOptions, callback: ChildFn) -> ChildFuture {
        Box::pin(async move { (callback.0)(noop_handle(), noop_starter()).await })
    }
}

thread_local! {
    static NOOP_SPAN: Arc<NoopTelemetrySpan> = Arc::new(NoopTelemetrySpan);
    static NOOP_STARTER: Arc<NoopStarter> = Arc::new(NoopStarter);
}

fn noop_handle() -> SpanHandle {
    SpanHandle {
        inner: NOOP_SPAN.with(Arc::clone) as Arc<dyn SpanHandleInner>,
    }
}

/// Inert handle used when recording is unavailable or a parent is settled.
/// Internal to the crate.
pub(crate) fn unrecorded_handle() -> SpanHandle {
    noop_handle()
}

fn noop_starter() -> SpanStarter {
    SpanStarter {
        inner: NOOP_STARTER.with(Arc::clone) as Arc<dyn SpanStarterInner>,
    }
}

#[allow(unused)]
fn _future_bounds() {
    fn assert_send<T: Send>(_: T) {}
    fn check() -> impl Future<Output = ()> + Send {
        let starter = noop_starter();
        async move {
            let value: i32 = starter
                .start_span(SpanOptions::new("x"), |_span, _child| async { 1 })
                .await;
            assert_send(value);
        }
    }
    let _ = check;
}
