//! PAN-358: the driver publishes each slot-clock event once.

use super::*;
use chrono::TimeZone;
use pancetta_core::slot_clock::ManualClock;
use std::sync::atomic::AtomicI64;

fn manual() -> (ManualClock, SlotClock) {
    let m = ManualClock::new(chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 5).unwrap());
    let clock = m.slot_clock(Arc::new(AtomicI64::new(15_000_000_000)));
    (m, clock)
}

async fn tick() {
    tokio::time::advance(SLOT_CLOCK_DRIVER_PERIOD).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn driver_publishes_each_new_event_once() {
    let (m, clock) = manual();
    let (tx, mut rx) = watch::channel(None);
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = spawn_slot_clock_driver(clock, tx, shutdown.clone());
    tick().await;
    assert!(!rx.has_changed().unwrap());

    m.step_wall(chrono::Duration::seconds(60));
    tick().await;
    assert!(rx.has_changed().unwrap());
    let event = rx.borrow_and_update().expect("an event");
    assert_eq!(event.epoch, 1);
    assert_eq!(event.skew, chrono::Duration::seconds(60));

    for _ in 0..4 {
        m.advance(SLOT_CLOCK_DRIVER_PERIOD);
        tick().await;
        assert!(!rx.has_changed().unwrap(), "no step, nothing published");
    }

    shutdown.store(true, Ordering::Release);
    tick().await;
    assert!(handle.is_finished());
}

#[tokio::test(start_paused = true)]
async fn driver_publishes_an_event_another_reader_observed() {
    let (m, clock) = manual();
    let (tx, mut rx) = watch::channel(None);
    let handle = spawn_slot_clock_driver(clock.clone(), tx, Arc::new(AtomicBool::new(false)));
    tick().await;
    m.step_wall(chrono::Duration::seconds(-30));
    // Another consumer observes the step first.
    assert_eq!(clock.read().epoch, 1);
    tick().await;
    assert!(rx.has_changed().unwrap());
    let event = rx.borrow_and_update().expect("an event");
    assert_eq!(event.epoch, 1);
    assert_eq!(event.skew, chrono::Duration::seconds(-30));
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn dropping_the_guard_stops_the_driver() {
    let (_m, clock) = manual();
    let (tx, _rx) = watch::channel(None);
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = spawn_slot_clock_driver(clock, tx, shutdown);
    let guard = AbortDriverOnDrop::new(&handle);
    tick().await;
    assert!(!handle.is_finished());
    drop(guard);
    let joined = handle.await;
    assert!(
        joined.is_err_and(|e| e.is_cancelled()),
        "the guard aborts the driver when run() returns early"
    );
}
