//! Slot-clock driver task (PAN-358).
//!
//! Reads the coordinator's [`SlotClock`] every 250 ms, so a wall-clock step
//! is observed within 250 ms even when no slot consumer happens to read the
//! clock, and publishes each new [`ClockEvent`] on a `watch` channel for
//! event-driven consumers (the queued-call TTL task). Consumers never depend
//! on it for correctness: every `read()` observes a step on its own.
//!
//! Unsupervised (like the clock-skew monitor), runs under `--replay`, is not
//! started for `--wav`, and is aborted at shutdown.

use pancetta_core::slot_clock::ClockEvent;
use pancetta_core::SlotClock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the driver reads the clock.
pub(crate) const SLOT_CLOCK_DRIVER_PERIOD: Duration = Duration::from_millis(250);

/// Spawn the driver. It exits on the first tick after `shutdown` is set.
pub(crate) fn spawn_slot_clock_driver(
    clock: SlotClock,
    events: watch::Sender<Option<ClockEvent>>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut published_epoch = events.borrow().map_or(0, |e| e.epoch);
        let mut tick = tokio::time::interval(SLOT_CLOCK_DRIVER_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let reading = clock.read();
            if reading.epoch != published_epoch {
                published_epoch = reading.epoch;
                events.send_replace(reading.last_event);
            }
        }
    })
}

#[cfg(test)]
#[path = "slot_clock_driver/tests/slot_clock_driver_tests.rs"]
mod slot_clock_driver_tests;
