//! Port of packages/telemetry/src/memory.ts (pi v0.84.3).
//!
//! Backend-neutral reference implementation recording spans in process
//! memory. Semantics preserved: settle-once, explicit-status precedence,
//! end-sequence ordering, child spans under a settled parent run unrecorded,
//! and recording is passive (never fails the callback).

use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::{
    ChildFn, ChildFuture, SpanAttributes, SpanError, SpanHandle, SpanHandleInner, SpanOptions,
    SpanStarter, SpanStarterInner, SpanStatus, TelemetryContext,
};

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    pub end_sequence: Option<u64>,
}

struct MutableSpan {
    id: u64,
    parent_id: Option<u64>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<RecordedTelemetryEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

#[derive(Default)]
struct InMemoryState {
    spans: Mutex<Vec<Arc<Mutex<MutableSpan>>>>,
    next_span_id: Mutex<Option<u64>>,
    next_end_sequence: Mutex<Option<u64>>,
}

impl InMemoryState {
    fn allocate_span_id(&self) -> u64 {
        // Upstream ids start at 1 (nextSpanId: 1).
        let mut next = self.next_span_id.lock().expect("span id lock");
        let id = next.unwrap_or(1);
        *next = Some(id + 1);
        id
    }

    fn allocate_end_sequence(&self) -> u64 {
        // Upstream end sequences start at 1 (nextEndSequence: 1).
        let mut next = self.next_end_sequence.lock().expect("end seq lock");
        let seq = next.unwrap_or(1);
        *next = Some(seq + 1);
        seq
    }
}

fn copy_status(status: &SpanStatus) -> SpanStatus {
    match status {
        SpanStatus::Ok => SpanStatus::Ok,
        SpanStatus::Error { error } => SpanStatus::Error {
            error: match error {
                SpanError::None => SpanError::None,
                SpanError::Some { name, message } => SpanError::Some {
                    name: name.clone(),
                    message: message.clone(),
                },
            },
        },
    }
}

fn settle(state: &InMemoryState, span: &Arc<Mutex<MutableSpan>>, failed: bool) {
    let mut span = span.lock().expect("span lock");
    if span.settled {
        return;
    }
    // The Rust callback model carries errors in the caller's value, so an
    // automatic error status has nothing to inspect; upstream's Error
    // inspection maps to the no-details form.
    if failed && !span.explicit_status {
        span.status = SpanStatus::Error {
            error: SpanError::None,
        };
    }
    span.settled = true;
    span.end_sequence = Some(state.allocate_end_sequence());
}

fn snapshot(span: &Arc<Mutex<MutableSpan>>) -> RecordedTelemetrySpan {
    let span = span.lock().expect("span lock");
    RecordedTelemetrySpan {
        id: span.id,
        parent_id: span.parent_id,
        name: span.name.clone(),
        attributes: span.attributes.clone(),
        events: span.events.clone(),
        status: copy_status(&span.status),
        settled: span.settled,
        end_sequence: span.end_sequence,
    }
}

struct RecordedHandle {
    state: Arc<InMemoryState>,
    span: Arc<Mutex<MutableSpan>>,
}

impl SpanHandleInner for RecordedHandle {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        let mut span = self.span.lock().expect("span lock");
        if span.settled {
            return;
        }
        span.events.push(RecordedTelemetryEvent {
            name: name.to_owned(),
            attributes,
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        let mut span = self.span.lock().expect("span lock");
        if span.settled {
            return;
        }
        for (name, value) in attributes {
            span.attributes.insert(name, value);
        }
    }

    fn set_status(&self, status: SpanStatus) {
        let mut span = self.span.lock().expect("span lock");
        if span.settled {
            return;
        }
        span.status = copy_status(&status);
        span.explicit_status = true;
    }

    fn start_child_erased(&self, options: SpanOptions, callback: ChildFn) -> ChildFuture {
        Box::pin(run_child(
            self.state.clone(),
            self.span.clone(),
            options,
            callback,
        ))
    }
}

fn record(
    state: &Arc<InMemoryState>,
    parent: Option<&Arc<Mutex<MutableSpan>>>,
    options: SpanOptions,
) -> Option<Arc<Mutex<MutableSpan>>> {
    // A child under a settled parent runs unrecorded (noop fallback upstream).
    if let Some(parent) = parent
        && parent.lock().expect("span lock").settled
    {
        return None;
    }
    let id = state.allocate_span_id();
    let span = Arc::new(Mutex::new(MutableSpan {
        id,
        parent_id: parent.map(|p| p.lock().expect("span lock").id),
        name: options.name,
        attributes: options.attributes,
        events: Vec::new(),
        status: SpanStatus::Ok,
        explicit_status: false,
        settled: false,
        end_sequence: None,
    }));
    state
        .spans
        .lock()
        .expect("spans lock")
        .push(Arc::clone(&span));
    Some(span)
}

async fn run_child(
    state: Arc<InMemoryState>,
    parent: Arc<Mutex<MutableSpan>>,
    options: SpanOptions,
    callback: ChildFn,
) -> Box<dyn std::any::Any + Send> {
    let recorded = record(&state, Some(&parent), options);
    let handle = match &recorded {
        Some(span) => SpanHandle {
            inner: Arc::new(RecordedHandle {
                state: Arc::clone(&state),
                span: Arc::clone(span),
            }),
        },
        None => crate::noop::unrecorded_handle(),
    };
    let starter = SpanStarter {
        inner: Arc::new(ChildStarterInnerImpl {
            state: Arc::clone(&state),
            parent: recorded.clone().unwrap_or_else(|| Arc::clone(&parent)),
            unrecorded: recorded.is_none(),
        }),
    };
    let result = (callback.0)(handle, starter).await;
    if let Some(span) = recorded {
        settle(&state, &span, false);
    }
    result
}

struct ChildStarterInnerImpl {
    state: Arc<InMemoryState>,
    parent: Arc<Mutex<MutableSpan>>,
    unrecorded: bool,
}

impl SpanStarterInner for ChildStarterInnerImpl {
    fn start_erased(&self, options: SpanOptions, callback: ChildFn) -> ChildFuture {
        if self.unrecorded {
            // Upstream: children of unrecorded runs stay unrecorded.
            Box::pin(async move {
                (callback.0)(crate::noop::unrecorded_handle(), noop_child_starter()).await
            })
        } else {
            Box::pin(run_child(
                Arc::clone(&self.state),
                Arc::clone(&self.parent),
                options,
                callback,
            ))
        }
    }
}

fn noop_child_starter() -> SpanStarter {
    SpanStarter {
        inner: Arc::new(NoopChildStarter),
    }
}

struct NoopChildStarter;

impl SpanStarterInner for NoopChildStarter {
    fn start_erased(&self, _options: SpanOptions, callback: ChildFn) -> ChildFuture {
        Box::pin(async move {
            (callback.0)(crate::noop::unrecorded_handle(), noop_child_starter()).await
        })
    }
}

/// Backend-neutral reference implementation that records spans in process
/// memory. Create a fresh instance to isolate tests or independent scopes.
#[derive(Default)]
pub struct InMemoryTelemetryContext {
    state: Arc<InMemoryState>,
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        let spans = self.state.spans.lock().expect("spans lock");
        spans.iter().map(snapshot).collect()
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn starter(&self) -> SpanStarter {
        SpanStarter {
            inner: Arc::new(ContextStarter {
                state: Arc::clone(&self.state),
            }),
        }
    }
}

struct ContextStarter {
    state: Arc<InMemoryState>,
}

impl SpanStarterInner for ContextStarter {
    fn start_erased(&self, options: SpanOptions, callback: ChildFn) -> ChildFuture {
        Box::pin(run_root(Arc::clone(&self.state), options, callback))
    }
}

async fn run_root(
    state: Arc<InMemoryState>,
    options: SpanOptions,
    callback: ChildFn,
) -> Box<dyn std::any::Any + Send> {
    let recorded = record(&state, None, options);
    let handle = match &recorded {
        Some(span) => SpanHandle {
            inner: Arc::new(RecordedHandle {
                state: Arc::clone(&state),
                span: Arc::clone(span),
            }),
        },
        None => crate::noop::unrecorded_handle(),
    };
    let starter = SpanStarter {
        inner: Arc::new(ChildStarterInnerImpl {
            state: Arc::clone(&state),
            parent: recorded.clone().unwrap_or_else(|| {
                Arc::new(Mutex::new(MutableSpan {
                    id: 0,
                    parent_id: None,
                    name: String::new(),
                    attributes: SpanAttributes::new(),
                    events: Vec::new(),
                    status: SpanStatus::Ok,
                    explicit_status: false,
                    settled: false,
                    end_sequence: None,
                }))
            }),
            unrecorded: recorded.is_none(),
        }),
    };
    let result = (callback.0)(handle, starter).await;
    if let Some(span) = recorded {
        settle(&state, &span, false);
    }
    result
}

#[allow(unused)]
fn _future_bounds() {
    fn check() -> impl Future<Output = ()> + Send {
        let context = InMemoryTelemetryContext::new();
        async move {
            let value: i32 = context
                .start_span(SpanOptions::new("x"), |_span, _child| async { 1 })
                .await;
            fn assert_send<T: Send>(_: T) {}
            assert_send(value);
        }
    }
    let _ = check;
}
