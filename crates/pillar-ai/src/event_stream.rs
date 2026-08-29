//! Port of packages/ai/src/utils/event-stream.ts (pi v0.84.3).
//!
//! Generic event stream: the push side feeds events; the pull side iterates
//! asynchronously; a final result resolves when a terminal event arrives
//! (`done`/`error` for assistant messages). Once ended, pushes are dropped.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::types::{AssistantMessage, AssistantMessageEvent};

struct EventStreamState<T, R> {
    queue: VecDeque<T>,
    done: bool,
    final_result: Option<Option<R>>,
    waker: Option<Waker>,
}

/// Generic event stream for async iteration. `is_complete` decides which
/// event resolves the final result; `extract_result` derives the result.
pub struct EventStream<T, R> {
    state: Arc<Mutex<EventStreamState<T, R>>>,
    is_complete: fn(&T) -> bool,
    extract_result: fn(&T) -> R,
}

impl<T, R> EventStream<T, R>
where
    T: Clone + Send + 'static,
    R: Clone + Send + 'static,
{
    pub fn new(is_complete: fn(&T) -> bool, extract_result: fn(&T) -> R) -> Self {
        Self {
            state: Arc::new(Mutex::new(EventStreamState {
                queue: VecDeque::new(),
                done: false,
                final_result: None,
                waker: None,
            })),
            is_complete,
            extract_result,
        }
    }

    /// Pushes an event; resolves the final result when the event is
    /// terminal. Pushes after `end` are ignored, mirroring upstream.
    pub fn push(&self, event: T) {
        let mut state = self.state.lock().expect("event stream lock");
        if state.done {
            return;
        }
        if (self.is_complete)(&event) {
            state.done = true;
            state.final_result = Some(Some((self.extract_result)(&event)));
        }
        let wake = state.waker.take();
        state.queue.push_back(event);
        if let Some(waker) = wake {
            waker.wake();
        }
    }

    /// Closes the stream. Events pushed afterwards are dropped.
    pub fn end(&self, result: Option<R>) {
        let mut state = self.state.lock().expect("event stream lock");
        state.done = true;
        if let Some(result) = result {
            state.final_result = Some(Some(result));
        }
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    /// Waits for and returns the final result.
    pub async fn result(&self) -> R {
        ResultFuture {
            state: Arc::clone(&self.state),
        }
        .await
    }

    /// Returns a pull-side iterator. The push half stays usable; multiple
    /// consumers split the queue, matching the single-consumer contract.
    pub fn iter(&self) -> EventIter<T, R> {
        EventIter {
            state: Arc::clone(&self.state),
        }
    }

    /// Creates another handle sharing the same stream state.
    pub fn clone_stream(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            is_complete: self.is_complete,
            extract_result: self.extract_result,
        }
    }
}

/// Pull-side stream. Drains queued events, then waits for push or end.
pub struct EventIter<T, R> {
    state: Arc<Mutex<EventStreamState<T, R>>>,
}

impl<T, R> futures::Stream for EventIter<T, R>
where
    T: Clone,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock().expect("event stream lock");
        if let Some(event) = state.queue.pop_front() {
            return Poll::Ready(Some(event));
        }
        if state.done {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

struct ResultFuture<T, R> {
    state: Arc<Mutex<EventStreamState<T, R>>>,
}

impl<T, R> Future for ResultFuture<T, R>
where
    R: Clone,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().expect("event stream lock");
        // Clone, not take: result() must be callable repeatedly (pi's
        // Promise semantics — awaiting the same promise twice resolves).
        if let Some(ready) = state.final_result.as_ref().and_then(Option::as_ref) {
            return Poll::Ready(ready.clone());
        }
        if state.done {
            // Upstream hangs when end() is called without a result and no
            // terminal event resolved one; the Rust port returns the
            // default-shaped pending wait instead. Registered waker keeps
            // this future alive for a later push that resolves the result.
            state.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Port of `AssistantMessageEventStream`: terminal events are `done` and
/// `error`; the result is the final assistant message either way.
pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;

/// Factory mirroring upstream `createAssistantMessageEventStream`.
pub fn assistant_message_event_stream() -> AssistantMessageEventStream {
    EventStream::new(
        |event| {
            matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )
        },
        |event| match event {
            AssistantMessageEvent::Done { message, .. } => message.clone(),
            AssistantMessageEvent::Error { error, .. } => error.clone(),
            _ => panic!("Unexpected event type for final result"),
        },
    )
}

/// Collects all events until the stream ends (test helper parity with the
/// upstream `collectEvents` helper).
pub async fn collect_events(stream: &AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    use futures::StreamExt;
    let mut events = Vec::new();
    let mut iter = stream.iter();
    while let Some(event) = iter.next().await {
        events.push(event);
    }
    events
}
