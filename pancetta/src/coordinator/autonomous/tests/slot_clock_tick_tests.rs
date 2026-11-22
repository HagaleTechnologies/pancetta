//! PAN-358: the autonomous slot tick on the shared `SlotClock`.
//!
//! Each test drives one `ManualClock` through `autonomous_slot_poll` every
//! 250 ms, as the autonomous loop does, and records the instants it fires.

use super::*;
use chrono::{DateTime, TimeZone, Utc};
use pancetta_core::slot_clock::{ManualClock, SlotClock};
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

const FT8_NS: i64 = 15_000_000_000;

fn base_plus(seconds: f64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
        + chrono::Duration::nanoseconds((seconds * 1e9).round() as i64)
}

fn secs(s: f64) -> chrono::Duration {
    chrono::Duration::nanoseconds((s * 1e9).round() as i64)
}

fn start(seconds: f64) -> (ManualClock, SlotClock, SlotTimer) {
    let manual = ManualClock::new(base_plus(seconds));
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(FT8_NS)));
    let timer = SlotTimer::new(&clock.read(), chrono::Duration::zero(), FT8_NS);
    (manual, clock, timer)
}

/// One loop iteration: poll, and on `due` advance as the loop does.
fn tick(clock: &SlotClock, timer: &mut SlotTimer) -> (AutonomousSlotPoll, DateTime<Utc>) {
    let reading = clock.read();
    let poll = autonomous_slot_poll(timer, &reading, FT8_NS);
    if poll.due {
        timer.advance(&reading);
    }
    (poll, reading.now)
}

/// Poll every 250 ms for `polls` polls; return the fire instants and every
/// poll that asked to clear decodes.
fn run(
    manual: &ManualClock,
    clock: &SlotClock,
    timer: &mut SlotTimer,
    polls: usize,
) -> (Vec<DateTime<Utc>>, Vec<AutonomousSlotPoll>) {
    let mut fired = Vec::new();
    let mut clears = Vec::new();
    for _ in 0..polls {
        manual.advance(std::time::Duration::from_millis(250));
        let (poll, now) = tick(clock, timer);
        if poll.due {
            fired.push(now);
        }
        if poll.clear_stale_decodes {
            clears.push(poll);
        }
    }
    (fired, clears)
}

#[test]
fn forward_resync_clears_stale_decodes_and_waits_for_the_next_boundary() {
    // 00:00:05, the deadline is 00:00:15. A +60 s step lands at 00:00:05.25.
    let (manual, clock, mut timer) = start(5.0);
    manual.advance(std::time::Duration::from_millis(250));
    let (before, _) = tick(&clock, &mut timer);
    assert!(!before.due && !before.clear_stale_decodes);
    assert_eq!(timer.deadline(), base_plus(15.0));

    manual.step_wall(secs(60.0));
    let (poll, now) = tick(&clock, &mut timer);
    assert!(!poll.due, "the poll that sees a resync never fires");
    assert!(
        poll.clear_stale_decodes,
        "decodes from before the step are stale"
    );
    match poll.reanchor {
        Some(Reanchor::ClockStep(event)) => {
            assert_eq!(event.epoch, 1);
            assert_eq!(event.skew, secs(60.0));
            assert_eq!(event.slot_start(), base_plus(60.0));
        }
        other => panic!("expected a clock-step re-anchor, got {other:?}"),
    }
    assert_eq!(now, base_plus(65.25));
    assert_eq!(
        timer.deadline(),
        base_plus(75.0),
        "the next fire is the first boundary after the step, never the pre-step 00:00:15"
    );

    // 30 s of polls: fires at 00:01:15 and 00:01:30 only, and nothing clears.
    let (fired, clears) = run(&manual, &clock, &mut timer, 120);
    assert_eq!(fired, vec![base_plus(75.0), base_plus(90.0)]);
    assert!(clears.is_empty());
    assert!(
        !autonomous_reanchor_text(&poll.reanchor.unwrap()).is_empty(),
        "the WARN text is built for every re-anchor"
    );
}

#[test]
fn backward_resync_also_clears_stale_decodes() {
    // 00:01:05, the deadline is 00:01:15. A -60 s step lands at 00:01:05.25.
    let (manual, clock, mut timer) = start(65.0);
    manual.advance(std::time::Duration::from_millis(250));
    tick(&clock, &mut timer);
    assert_eq!(timer.deadline(), base_plus(75.0));

    manual.step_wall(secs(-60.0));
    let (poll, _) = tick(&clock, &mut timer);
    assert!(!poll.due);
    assert!(poll.clear_stale_decodes);
    match poll.reanchor {
        Some(Reanchor::ClockStep(event)) => {
            assert_eq!(event.skew, secs(-60.0));
            assert!(event.resync);
            assert_eq!(event.slot_start(), base_plus(0.0));
        }
        other => panic!("expected a clock-step re-anchor, got {other:?}"),
    }
    assert_eq!(timer.deadline(), base_plus(15.0));
    assert!(autonomous_reanchor_text(&poll.reanchor.unwrap()).contains("-60.0 s clock step"));

    let (fired, _) = run(&manual, &clock, &mut timer, 120);
    assert_eq!(fired, vec![base_plus(15.0), base_plus(30.0)]);
}

#[test]
fn lag_without_a_step_clears_and_reanchors() {
    // The loop stalls for 40 s (both clocks advance, so no step): the
    // 00:00:15 deadline is more than one period late at 00:00:45.
    let (manual, clock, mut timer) = start(5.0);
    manual.advance(std::time::Duration::from_secs(40));
    let (poll, now) = tick(&clock, &mut timer);
    assert_eq!(now, base_plus(45.0));
    assert!(!poll.due, "a lagged deadline never fires late");
    assert!(poll.clear_stale_decodes);
    match poll.reanchor {
        Some(Reanchor::Lagged { behind, slot_start }) => {
            assert_eq!(behind, secs(30.0));
            assert_eq!(slot_start, base_plus(45.0));
        }
        other => panic!("expected a lag re-anchor, got {other:?}"),
    }
    assert_eq!(clock.read().epoch, 0, "a lag is not a clock event");
    assert_eq!(timer.deadline(), base_plus(60.0));
    assert!(autonomous_reanchor_text(&poll.reanchor.unwrap()).contains("no clock step"));

    let (fired, _) = run(&manual, &clock, &mut timer, 80);
    assert_eq!(fired, vec![base_plus(60.0)]);
}

#[test]
fn sub_second_step_keeps_the_deadline() {
    let (manual, clock, mut timer) = start(5.0);
    manual.advance(std::time::Duration::from_millis(250));
    tick(&clock, &mut timer);
    assert_eq!(timer.deadline(), base_plus(15.0));

    manual.step_wall(secs(0.5));
    let (poll, _) = tick(&clock, &mut timer);
    assert_eq!(clock.read().epoch, 1, "a 0.5 s step is a clock event");
    assert_eq!(clock.read().resync_epoch, 0, "but not a resync");
    assert!(!poll.due);
    assert!(!poll.clear_stale_decodes);
    assert_eq!(poll.reanchor, None);
    assert_eq!(timer.deadline(), base_plus(15.0));

    // Polls from 00:00:05.75 to 00:00:35.75: fires at the first poll at or
    // after each boundary, as with no step.
    let (fired, clears) = run(&manual, &clock, &mut timer, 120);
    assert_eq!(fired, vec![base_plus(15.0), base_plus(30.0)]);
    assert!(clears.is_empty());
}
