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
    /// Every waiter currently parked on this stream. The push and result
    /// halves register separately and can be awaited at the same time, so one
    /// slot would drop a wakeup (docs/ARCHITECTURE-REVIEW-s05c0.md D).
    wakers: Vec<Waker>,
}

impl<T, R> EventStreamState<T, R> {
    /// Park `waker` until the next push or end. The same task polling twice
    /// must not accumulate duplicates.
    fn register(&mut self, waker: &Waker) {
        if self.wakers.iter().any(|existing| existing.will_wake(waker)) {
            return;
        }
        self.wakers.push(waker.clone());
    }

    /// Wake every parked waiter; each will re-check the state.
    fn wake_all(&mut self) {
        for waker in self.wakers.drain(..) {
            waker.wake();
        }
    }
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
                wakers: Vec::new(),
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
        state.queue.push_back(event);
        state.wake_all();
    }

    /// Closes the stream. Events pushed afterwards are dropped.
    pub fn end(&self, result: Option<R>) {
        let mut state = self.state.lock().expect("event stream lock");
        state.done = true;
        if let Some(result) = result {
            state.final_result = Some(Some(result));
        }
        state.wake_all();
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
        state.register(cx.waker());
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
            state.register(cx.waker());
            return Poll::Pending;
        }
        state.register(cx.waker());
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// A waker that counts wakeups instead of scheduling a task, so the
    /// stream's parked waiters are exercised without an executor.
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counter() -> (Arc<Counter>, Waker) {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    /// A terminal event is `0`; the result is the terminal value times ten.
    fn stream() -> EventStream<u32, u32> {
        EventStream::new(|value| *value == 0, |value| value * 10)
    }

    fn wakes(counter: &Arc<Counter>) -> usize {
        counter.0.load(Ordering::SeqCst)
    }

    /// The push half and the result half park at the same time: a single waker
    /// slot silently dropped one of them.
    #[test]
    fn a_push_wakes_every_parked_waiter() {
        let stream = stream();
        let (iter_count, iter_waker) = counter();
        let (result_count, result_waker) = counter();
        let mut iter = pin!(stream.iter());
        let mut result = pin!(stream.result());
        let mut iter_cx = Context::from_waker(&iter_waker);
        let mut result_cx = Context::from_waker(&result_waker);

        assert!(iter.as_mut().poll_next(&mut iter_cx).is_pending());
        assert!(result.as_mut().poll(&mut result_cx).is_pending());

        stream.push(7);
        assert_eq!(wakes(&iter_count), 1, "the event half must be woken");
        assert_eq!(wakes(&result_count), 1, "the result half must be woken");
    }

    /// `end` wakes every parked waiter too.
    #[test]
    fn end_wakes_every_parked_waiter() {
        let stream = stream();
        let (iter_count, iter_waker) = counter();
        let (result_count, result_waker) = counter();
        let mut iter = pin!(stream.iter());
        let mut result = pin!(stream.result());
        let mut iter_cx = Context::from_waker(&iter_waker);
        let mut result_cx = Context::from_waker(&result_waker);

        assert!(iter.as_mut().poll_next(&mut iter_cx).is_pending());
        assert!(result.as_mut().poll(&mut result_cx).is_pending());

        stream.end(Some(3));
        assert_eq!(wakes(&iter_count), 1);
        assert_eq!(wakes(&result_count), 1);
        assert_eq!(result.as_mut().poll(&mut result_cx), Poll::Ready(3));
        assert_eq!(iter.as_mut().poll_next(&mut iter_cx), Poll::Ready(None));
    }

    /// Polling the same task repeatedly parks one waiter, not one per poll.
    #[test]
    fn repeated_polls_do_not_accumulate_waiters() {
        let stream = stream();
        let (count, waker) = counter();
        let mut iter = pin!(stream.iter());
        let mut cx = Context::from_waker(&waker);
        for _ in 0..3 {
            assert!(iter.as_mut().poll_next(&mut cx).is_pending());
        }
        stream.push(7);
        assert_eq!(wakes(&count), 1, "one wakeup per push");
    }

    /// The terminal event resolves the result, drains to the queue end and
    /// closes the stream: later pushes wake nobody.
    #[test]
    fn a_terminal_event_resolves_and_closes_the_stream() {
        let stream = stream();
        let (iter_count, iter_waker) = counter();
        let (result_count, result_waker) = counter();
        let mut iter = pin!(stream.iter());
        let mut result = pin!(stream.result());
        let mut iter_cx = Context::from_waker(&iter_waker);
        let mut result_cx = Context::from_waker(&result_waker);
        assert!(iter.as_mut().poll_next(&mut iter_cx).is_pending());
        assert!(result.as_mut().poll(&mut result_cx).is_pending());

        stream.push(7);
        assert_eq!(
            iter.as_mut().poll_next(&mut iter_cx),
            Poll::Ready(Some(7))
        );
        stream.push(0);
        assert_eq!(wakes(&result_count), 1);
        assert_eq!(result.as_mut().poll(&mut result_cx), Poll::Ready(0));
        assert_eq!(
            iter.as_mut().poll_next(&mut iter_cx),
            Poll::Ready(Some(0))
        );
        assert_eq!(iter.as_mut().poll_next(&mut iter_cx), Poll::Ready(None));

        stream.push(5);
        assert_eq!(wakes(&iter_count), 1, "a push after the end wakes nobody");
        // The resolved result stays awaitable (pi's promise semantics).
        let mut again = pin!(stream.result());
        assert_eq!(again.as_mut().poll(&mut result_cx), Poll::Ready(0));
    }
}
