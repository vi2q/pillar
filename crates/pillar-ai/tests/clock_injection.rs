//! The host timer injection (`pillar_ai::clock`): a host without a tokio timer
//! driver supplies its own sleep, and the port's waiting goes through it.
//!
//! Its own test binary: the timer is process-global, so installing a fake here
//! cannot disturb the other suites.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn the_host_timer_drives_sleep_and_timeout() {
    let calls = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let seen = Arc::clone(&calls);
    pillar_ai::set_default_sleep(Some(Arc::new(move |duration| {
        seen.lock().unwrap().push(duration);
        // Resolve immediately: the host owns the clock.
        Box::pin(async {})
    })));

    // `sleep` asks the host instead of tokio's timer.
    futures::executor::block_on(pillar_ai::sleep(Duration::from_millis(25)));
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [Duration::from_millis(25)]
    );

    // A future that finishes first wins, and the host timer is still requested.
    let value = futures::executor::block_on(pillar_ai::timeout(
        Duration::from_secs(5),
        async { 7u8 },
    ))
    .expect("the inner future wins");
    assert_eq!(value, 7);
    assert_eq!(calls.lock().unwrap().len(), 2);

    // When the host clock is the only one that advances, `timeout` reports the
    // elapsed deadline.
    let elapsed = futures::executor::block_on(pillar_ai::timeout(
        Duration::from_millis(1),
        std::future::pending::<()>(),
    ));
    assert!(elapsed.is_err());

    pillar_ai::set_default_sleep(None);
}

/// Without an installed timer the native default is still `tokio::time`, so the
/// replacement did not accidentally break the normal path.
#[tokio::test]
async fn the_platform_default_still_waits() {
    pillar_ai::set_default_sleep(None);
    assert!(pillar_ai::get_default_sleep().is_none());
    let start = Instant::now();
    pillar_ai::sleep(Duration::from_millis(5)).await;
    assert!(start.elapsed() >= Duration::from_millis(4));

    let counter = Arc::new(AtomicUsize::new(0));
    let bumped = Arc::clone(&counter);
    let result = pillar_ai::timeout(Duration::from_secs(5), async move {
        bumped.fetch_add(1, Ordering::SeqCst);
        "done"
    })
    .await
    .expect("the future wins");
    assert_eq!(result, "done");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
