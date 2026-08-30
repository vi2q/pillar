//! Port of packages/agent/src/harness/events.ts (pi v0.84.3).
//!
//! Harness run lifecycle events and the event bus. The bus delivers
//! type-filtered direct listeners plus buffering watchers.

use std::sync::{Arc, Mutex};

/// Event published when a run starts on a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStartEvent {
    /// Lane the run executes on.
    pub lane: String,
    /// Unique run identifier.
    pub run_id: String,
}

/// Event published when a run finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEndEvent {
    /// Lane the run executed on.
    pub lane: String,
    /// Unique run identifier.
    pub run_id: String,
    /// How the run ended.
    pub outcome: RunOutcome,
    /// Session entry id the run ended on.
    pub leaf_id: String,
}

/// Terminal outcome of a harness run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

/// Harness lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessEvent {
    RunStart(RunStartEvent),
    RunEnd(RunEndEvent),
}

impl HarnessEvent {
    /// Upstream discriminant (`event.type`).
    pub fn kind(&self) -> &'static str {
        match self {
            HarnessEvent::RunStart(_) => "run_start",
            HarnessEvent::RunEnd(_) => "run_end",
        }
    }
}

/// Listener invoked for events. Async listeners are spawned by the bus.
pub type HarnessListener = Arc<dyn Fn(&HarnessEvent) + Send + Sync>;

/// Handle returned by [`HarnessEventBus::watch`].
pub struct WatchHandle {
    bus: Arc<Mutex<HarnessEventBusInner>>,
    receive_key: usize,
    snapshot: Arc<dyn std::any::Any + Send + Sync>,
}

impl WatchHandle {
    /// The snapshot captured when the watch was created.
    pub fn snapshot(&self) -> &Arc<dyn std::any::Any + Send + Sync> {
        &self.snapshot
    }

    /// Start delivering events to `listener`, first flushing everything
    /// buffered since creation (upstream `watch().start()`).
    pub fn start(self, listener: HarnessListener) {
        let mut inner = self.bus.lock().expect("event bus lock");
        let watch = inner
            .watchers
            .iter_mut()
            .find(|w| w.key == self.receive_key)
            .expect("watch handle registered");
        // Stay in buffering mode while flushing so reentrant emissions
        // preserve order.
        let mut buffered = std::mem::take(&mut watch.buffered);
        for event in &buffered {
            listener(event);
        }
        buffered.clear();
        watch.buffered = buffered;
        watch.listener = Some(listener);
    }

    /// Remove the watch from the bus; buffered events are dropped.
    pub fn unsubscribe(self) {
        let mut inner = self.bus.lock().expect("event bus lock");
        inner.watchers.retain(|w| w.key != self.receive_key);
    }
}

struct Watcher {
    key: usize,
    buffered: Vec<HarnessEvent>,
    listener: Option<HarnessListener>,
}

/// Listener registration key type-filtered subscriptions are removed by.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DirectKey {
    event_kind: &'static str,
    id: usize,
}

pub(crate) struct HarnessEventBusInner {
    next_id: usize,
    direct: Vec<(DirectKey, HarnessListener)>,
    watchers: Vec<Watcher>,
}

/// Event bus for harness run events. Register passive listeners with
/// [`on`](Self::on) or buffering watches with [`watch`](Self::watch).
#[derive(Clone)]
pub struct HarnessEventBus {
    pub(crate) inner: Arc<Mutex<HarnessEventBusInner>>,
}

impl Default for HarnessEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessEventBus {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HarnessEventBusInner {
                next_id: 0,
                direct: Vec::new(),
                watchers: Vec::new(),
            })),
        }
    }

    /// Register a listener for future events of one type and return its
    /// registration handle. Earlier events are not replayed, and no
    /// snapshot or event buffer is provided.
    pub fn on(&self, event_kind: &'static str, listener: HarnessListener) -> DirectListener {
        let mut inner = self.inner.lock().expect("event bus lock");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.direct.push((DirectKey { event_kind, id }, listener));
        DirectListener {
            bus: Arc::clone(&self.inner),
            key: DirectKey { event_kind, id },
        }
    }

    /// Publish an event to current direct listeners and watchers.
    pub fn emit(&self, event: HarnessEvent) {
        let mut inner = self.inner.lock().expect("event bus lock");
        let kind = event.kind();
        // Deliver only to direct listeners registered for this event type.
        for (key, listener) in &inner.direct {
            if key.event_kind == kind {
                listener(&event);
            }
        }
        // Deliver every event to each watcher; unstarted watchers buffer
        // until `start` flushes them.
        for watcher in &mut inner.watchers {
            match &watcher.listener {
                Some(listener) => listener(&event),
                None => watcher.buffered.push(event.clone()),
            }
        }
    }

    /// Create a watch that buffers events from now on and captures a
    /// snapshot atomically (no event gap).
    ///
    /// docs/INSTRUCTIONS.md #1: the bus lock must NOT be held while the snapshot
    /// callback runs — upstream captures can emit re-entrantly, and the
    /// port's `emit` re-locks. Registration happens under the lock, then
    /// the lock is dropped before capture; concurrent emissions between
    /// unlock and capture land in the buffer (which is the upstream
    /// semantics: buffering starts at watch() creation, snapshot is taken
    /// immediately after). A concurrent-emit window before capture is
    /// accepted divergence (upstream JS is single-threaded).
    pub fn watch<F, T>(&self, capture_snapshot: F) -> WatchHandle
    where
        F: FnOnce() -> T,
        T: Send + Sync + 'static,
    {
        let key = {
            let mut inner = self.inner.lock().expect("event bus lock");
            let key = inner.next_id;
            inner.next_id += 1;
            inner.watchers.push(Watcher {
                key,
                buffered: Vec::new(),
                listener: None,
            });
            key
        };
        // Lock dropped: re-entrant emit() inside the snapshot callback
        // buffers into the watcher above instead of deadlocking.
        let snapshot = Arc::new(capture_snapshot());
        WatchHandle {
            bus: Arc::clone(&self.inner),
            receive_key: key,
            snapshot,
        }
    }
}

/// Unsubscribe handle returned by [`HarnessEventBus::on`].
pub struct DirectListener {
    bus: Arc<Mutex<HarnessEventBusInner>>,
    key: DirectKey,
}

impl DirectListener {
    /// Remove the listener (upstream returned unsubscribe closure).
    pub fn unsubscribe(self) {
        let mut inner = self.bus.lock().expect("event bus lock");
        inner.direct.retain(|(key, _)| *key != self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run_start(lane: &str, run_id: &str) -> HarnessEvent {
        HarnessEvent::RunStart(RunStartEvent {
            lane: lane.to_owned(),
            run_id: run_id.to_owned(),
        })
    }

    fn run_end(lane: &str, run_id: &str) -> HarnessEvent {
        HarnessEvent::RunEnd(RunEndEvent {
            lane: lane.to_owned(),
            run_id: run_id.to_owned(),
            outcome: RunOutcome::Completed,
            leaf_id: "entry-1".to_owned(),
        })
    }

    /// upstream test: "delivers matching events to direct listeners and watchers"
    #[test]
    fn delivers_matching_events_to_direct_listeners_and_watchers() {
        let events = HarnessEventBus::new();
        let direct = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
        let watch_events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));

        let direct_for_listener = Arc::clone(&direct);
        let listener = events.on(
            "run_start",
            Arc::new(move |event| {
                direct_for_listener.lock().unwrap().push(event.clone());
            }),
        );
        let watch = events.watch(|| ());
        let watch_for_listener = Arc::clone(&watch_events);
        watch.start(Arc::new(move |event| {
            watch_for_listener.lock().unwrap().push(event.clone());
        }));

        events.emit(run_start("main", "run-1"));
        events.emit(run_end("main", "run-1"));
        listener.unsubscribe();
        events.emit(run_start("main", "run-1"));

        let direct = direct.lock().unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0], run_start("main", "run-1"));
        let watch_events = watch_events.lock().unwrap();
        assert_eq!(
            *watch_events,
            vec![
                run_start("main", "run-1"),
                run_end("main", "run-1"),
                run_start("main", "run-1"),
            ]
        );
    }

    /// upstream test: "captures a snapshot without an event gap, then flushes and delivers live events"
    #[test]
    fn captures_snapshot_without_event_gap_then_flushes_and_delivers_live_events() {
        let events = HarnessEventBus::new();
        let expected_snapshot = Arc::new(AtomicUsize::new(7));
        let snapshot_for_capture = Arc::clone(&expected_snapshot);
        let bus_for_capture = events.clone();

        // The snapshot callback emits re-entrantly; the port registers the
        // buffering watcher first, then releases the lock before capture,
        // so the event lands in the buffer instead of deadlocking.
        let watch = {
            let bus = bus_for_capture.clone();
            bus_for_capture.watch(move || {
                bus.emit(run_start("main", "run-1"));
                Arc::clone(&snapshot_for_capture)
            })
        };
        let received = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));

        assert_eq!(
            watch
                .snapshot()
                .downcast_ref::<Arc<AtomicUsize>>()
                .expect("snapshot type")
                .load(Ordering::SeqCst),
            7
        );
        assert!(received.lock().unwrap().is_empty());

        let received_for_listener = Arc::clone(&received);
        watch.start(Arc::new(move |event| {
            received_for_listener.lock().unwrap().push(event.clone());
        }));
        assert_eq!(*received.lock().unwrap(), vec![run_start("main", "run-1")]);

        events.emit(run_end("main", "run-1"));
        assert_eq!(
            *received.lock().unwrap(),
            vec![run_start("main", "run-1"), run_end("main", "run-1")]
        );
    }
}
