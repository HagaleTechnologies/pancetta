//! `SlotTimer` (PAN-358): the autonomous `SlotTicker` tests ported to the
//! shared clock, plus the resync, lag and no-refire rules.

use super::*;
use crate::slot_clock::{ManualClock, SlotClock};
use chrono::TimeZone;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

const FT8_SLOT_NS: i64 = 15_000_000_000;
const FT4_SLOT_NS: i64 = 7_500_000_000;
const FT2_SLOT_NS: i64 = 3_200_000_000;

fn at(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(0) + Duration::milliseconds(millis)
}

/// A reading with no clock event (as on a virtual clock without detection).
fn r(millis: i64) -> SlotReading {
    SlotReading {
        now: at(millis),
        slot_ns: FT8_SLOT_NS,
        epoch: 0,
        resync_epoch: 0,
        last_event: None,
        last_resync: None,
    }
}

/// A reading that carries resync event `epoch`, observed at this instant.
fn resync_reading(millis: i64, skew: Duration, epoch: u64) -> SlotReading {
    let event = ClockEvent {
        epoch,
        skew,
        observed_at: at(millis),
        slot_ns: FT8_SLOT_NS,
        resync: true,
    };
    SlotReading {
        now: at(millis),
        slot_ns: FT8_SLOT_NS,
        epoch,
        resync_epoch: epoch,
        last_event: Some(event),
        last_resync: Some(event),
    }
}

fn manual_at(seconds: f64) -> (ManualClock, SlotClock) {
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let manual = ManualClock::new(base + Duration::nanoseconds((seconds * 1e9) as i64));
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(FT8_SLOT_NS)));
    (manual, clock)
}

fn base_plus(seconds: f64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
        + Duration::nanoseconds((seconds * 1e9).round() as i64)
}

fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}

/// Poll once; on due, advance and return the boundary that fired.
fn poll_fire(timer: &mut SlotTimer, reading: &SlotReading, slot_ns: i64) -> Option<DateTime<Utc>> {
    let p = timer.poll(reading, slot_ns);
    if p.due {
        let fired = timer.deadline();
        timer.advance(reading);
        Some(fired)
    } else {
        None
    }
}

#[test]
fn slot_timer_recovers_ft2_boundary_crossed_between_polls_once() {
    let mut t = SlotTimer::new(&r(98_750), Duration::zero(), FT8_SLOT_NS);
    assert!(!t.poll(&r(99_000), FT8_SLOT_NS).due);
    assert_eq!(t.deadline(), at(105_000));
    assert_eq!(
        t.poll(&r(99_250), FT2_SLOT_NS),
        SlotPoll {
            due: true,
            reanchor: None
        }
    );
    t.advance(&r(99_250));
    assert_eq!(t.deadline(), at(102_400));
    assert_eq!(t.deadline_slot_ns, FT2_SLOT_NS);
    assert!(!t.poll(&r(99_250), FT2_SLOT_NS).due);
    assert!(!t.poll(&r(99_500), FT2_SLOT_NS).due);
    assert!(t.poll(&r(102_400), FT2_SLOT_NS).due);
    t.advance(&r(102_400));
    assert_eq!(t.deadline(), at(105_600));
}

#[test]
fn slot_timer_recovers_ft4_boundary_at_or_before_current_poll() {
    for observed_at in [97_500, 97_750] {
        let mut t = SlotTimer::new(&r(97_250), Duration::zero(), FT8_SLOT_NS);
        assert_eq!(
            t.poll(&r(observed_at), FT4_SLOT_NS),
            SlotPoll {
                due: true,
                reanchor: None
            }
        );
        t.advance(&r(observed_at));
        assert_eq!(t.deadline(), at(105_000));
        assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
        assert!(!t.poll(&r(observed_at), FT4_SLOT_NS).due);
        assert!(!t.poll(&r(98_000), FT4_SLOT_NS).due);
        assert!(t.poll(&r(105_000), FT4_SLOT_NS).due);
        t.advance(&r(105_000));
        assert_eq!(t.deadline(), at(112_500));
    }
}

#[test]
fn slot_timer_mode_switch_without_crossed_boundary_only_recomputes() {
    let mut t = SlotTimer::new(&r(97_000), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(
        t.poll(&r(97_250), FT4_SLOT_NS),
        SlotPoll {
            due: false,
            reanchor: None
        }
    );
    assert_eq!(t.deadline(), at(97_500));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
    assert!(t.poll(&r(97_500), FT4_SLOT_NS).due);
}

#[test]
fn slot_timer_non_due_poll_advances_boundary_recovery_history() {
    let mut t = SlotTimer::new(&r(97_250), Duration::zero(), FT8_SLOT_NS);
    assert!(!t.poll(&r(97_750), FT8_SLOT_NS).due);
    assert_eq!(
        t.poll(&r(98_000), FT4_SLOT_NS),
        SlotPoll {
            due: false,
            reanchor: None
        }
    );
    assert_eq!(t.deadline(), at(105_000));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
}

#[test]
fn slot_timer_boundary_at_previous_poll_is_excluded() {
    let mut t = SlotTimer::new(&r(97_250), Duration::zero(), FT8_SLOT_NS);
    assert!(!t.poll(&r(97_500), FT8_SLOT_NS).due);
    assert_eq!(
        t.poll(&r(97_750), FT4_SLOT_NS),
        SlotPoll {
            due: false,
            reanchor: None
        }
    );
    assert_eq!(t.deadline(), at(105_000));
}

#[test]
fn slot_timer_backward_step_with_mode_switch_updates_history_without_firing() {
    let mut t = SlotTimer::new(&r(700_000), Duration::zero(), FT8_SLOT_NS);
    let step = resync_reading(99_000, Duration::milliseconds(-601_000), 1);
    let p = t.poll(&step, FT4_SLOT_NS);
    assert!(!p.due);
    assert!(matches!(p.reanchor, Some(Reanchor::ClockStep(e)) if e.epoch == 1));
    assert_eq!(t.deadline(), at(105_000));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
    // History uses the corrected clock: the FT2 boundary at 99.2 s is inside
    // the next poll's interval.
    let mut next = step;
    next.now = at(99_250);
    assert_eq!(
        t.poll(&next, FT2_SLOT_NS),
        SlotPoll {
            due: true,
            reanchor: None
        }
    );
    t.advance(&next);
    assert_eq!(t.deadline(), at(102_400));
    next.now = at(99_500);
    assert!(!t.poll(&next, FT2_SLOT_NS).due);
    next.now = at(102_400);
    assert!(t.poll(&next, FT2_SLOT_NS).due);
}

#[test]
fn slot_timer_delayed_mode_switch_recovers_only_one_tick() {
    let mut t = SlotTimer::new(&r(90_250), Duration::zero(), FT8_SLOT_NS);
    // Cross 92.8 s, 96 s and 99.2 s without reaching the old 105 s deadline.
    assert!(t.poll(&r(99_250), FT2_SLOT_NS).due);
    t.advance(&r(99_250));
    assert_eq!(t.deadline(), at(102_400));
    assert_eq!(t.deadline_slot_ns, FT2_SLOT_NS);
    assert!(!t.poll(&r(99_250), FT2_SLOT_NS).due);
    for millis in (99_500..102_400).step_by(250) {
        assert!(!t.poll(&r(millis), FT2_SLOT_NS).due);
    }
    assert!(t.poll(&r(102_400), FT2_SLOT_NS).due);
    t.advance(&r(102_400));
    assert_eq!(t.deadline(), at(105_600));
}

#[test]
fn no_fire_before_deadline() {
    let mut t = SlotTimer::new(&r(10_000), Duration::zero(), FT8_SLOT_NS);
    assert!(!t.poll(&r(10_000), FT8_SLOT_NS).due);
    assert_eq!(t.deadline(), at(15_000), "unchanged while not yet due");
}

#[test]
fn fires_exactly_at_deadline_and_recomputes_next_ft8_boundary() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(poll_fire(&mut t, &r(15_000), FT8_SLOT_NS), Some(at(15_000)));
    assert_eq!(t.deadline(), at(30_000));
}

#[test]
fn ft8_cadence_unchanged_across_repeated_250ms_polls() {
    let mut now = 3_200;
    let mut t = SlotTimer::new(&r(now), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), at(15_000));
    let mut fired_at = Vec::new();
    for _ in 0..400 {
        now += 250;
        if let Some(b) = poll_fire(&mut t, &r(now), FT8_SLOT_NS) {
            fired_at.push(b);
        }
    }
    assert_eq!(
        fired_at,
        vec![
            at(15_000),
            at(30_000),
            at(45_000),
            at(60_000),
            at(75_000),
            at(90_000)
        ]
    );
}

#[test]
fn mid_run_mode_change_to_ft4_shortens_the_next_deadline() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert!(poll_fire(&mut t, &r(15_000), FT4_SLOT_NS).is_some());
    assert_eq!(t.deadline(), at(22_500));
}

#[test]
fn mid_run_mode_change_to_ft2_shortens_the_next_deadline() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert!(poll_fire(&mut t, &r(15_000), FT2_SLOT_NS).is_some());
    assert_eq!(t.deadline(), at(16_000));
}

#[test]
fn mid_wait_mode_change_shortens_the_deadline_before_it_would_have_fired() {
    let mut t = SlotTimer::new(&r(1_000), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), at(15_000));
    assert!(!t.poll(&r(5_000), FT4_SLOT_NS).due);
    assert_eq!(t.deadline(), at(7_500));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
    assert!(poll_fire(&mut t, &r(7_500), FT4_SLOT_NS).is_some());
    assert_eq!(t.deadline(), at(15_000));
}

#[test]
fn mode_aware_wrapper_is_a_no_op_when_slot_ns_never_changes() {
    let mut now = 3_200;
    let mut t = SlotTimer::new(&r(now), Duration::zero(), FT8_SLOT_NS);
    let mut fired_at = Vec::new();
    for _ in 0..400 {
        now += 250;
        if let Some(b) = poll_fire(&mut t, &r(now), FT8_SLOT_NS) {
            fired_at.push(b);
        }
        assert_eq!(t.deadline_slot_ns, FT8_SLOT_NS);
    }
    assert_eq!(fired_at.len(), 6);
    assert!(fired_at
        .iter()
        .all(|b| b.timestamp_nanos_opt().unwrap() % FT8_SLOT_NS == 0));
}

#[test]
fn mode_mismatch_discovered_exactly_when_the_stale_deadline_fires_still_fires() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert!(t.poll(&r(15_000), FT4_SLOT_NS).due);
    t.advance(&r(15_000));
    assert_eq!(t.deadline(), at(22_500));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
}

#[test]
fn mode_mismatch_discovered_after_the_stale_deadline_is_overdue_still_fires() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert!(t.poll(&r(15_250), FT4_SLOT_NS).due);
    t.advance(&r(15_250));
    assert_eq!(t.deadline(), at(22_500));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
}

#[test]
fn small_backward_step_inside_one_period_is_waited_out_unchanged() {
    let mut t = SlotTimer::new(&r(95_000), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), at(105_000));
    // The clock steps back 3 s with no resync: 13 s ahead is waited out.
    assert_eq!(
        t.poll(&r(92_000), FT8_SLOT_NS),
        SlotPoll {
            due: false,
            reanchor: None
        }
    );
    assert_eq!(t.deadline(), at(105_000));
}

#[test]
fn slot_timer_keeps_firing_on_utc_boundaries_after_a_backward_step() {
    // A backward step the clock did not report (no detection): the timer
    // still returns to the UTC grid instead of stalling.
    let seeded_at = 1_000_003_200;
    let mut t = SlotTimer::new(&r(seeded_at), Duration::zero(), FT8_SLOT_NS);
    let step_at = seeded_at - 600_000;
    let mut now = step_at;
    let mut fires = 0;
    let mut first_fire = None;
    while now - step_at <= 599_000 {
        let deadline_before = t.deadline();
        if poll_fire(&mut t, &r(now), FT8_SLOT_NS).is_some() {
            fires += 1;
            first_fire.get_or_insert(now);
            let ns = deadline_before.timestamp_nanos_opt().unwrap();
            assert_eq!(ns % FT8_SLOT_NS, 0, "fired off the UTC slot grid");
            assert!(at(now) >= deadline_before);
            assert!(at(now) - deadline_before < Duration::milliseconds(250));
        }
        now += 250;
    }
    assert!(
        fires >= 39,
        "only {fires} slot ticks after a 600 s backward step"
    );
    assert!(first_fire.expect("never fired") - step_at <= 15_000);
}

#[test]
fn slot_timer_mode_switch_is_not_reported_as_a_reanchor() {
    let mut t = SlotTimer::new(&r(99_750), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(
        t.poll(&r(100_000), FT4_SLOT_NS),
        SlotPoll {
            due: false,
            reanchor: None
        }
    );
    assert_eq!(t.deadline(), at(105_000));
    assert_eq!(t.deadline_slot_ns, FT4_SLOT_NS);
}

// ---- PAN-358: the shared clock's resync, lag and no-refire rules ----

#[test]
fn timer_without_steps_matches_the_ft8_deadline_sequence() {
    let (manual, clock) = manual_at(5.0);
    let mut t = SlotTimer::new(&clock.read(), Duration::zero(), FT8_SLOT_NS);
    let mut fires = Vec::new();
    for _ in 0..600 {
        manual.advance(ms(250));
        let reading = clock.read();
        assert_eq!(reading.epoch, 0);
        if poll_fire(&mut t, &reading, FT8_SLOT_NS).is_some() {
            fires.push(reading.now);
        }
    }
    let expected: Vec<_> = (1..=10).map(|k| base_plus(15.0 * k as f64)).collect();
    assert_eq!(fires, expected);
}

#[test]
fn forward_resync_reanchors_from_the_observed_instant_without_firing() {
    let (manual, clock) = manual_at(5.0);
    let mut t = SlotTimer::new(&clock.read(), Duration::zero(), FT8_SLOT_NS);
    manual.advance(ms(250));
    assert!(!t.poll(&clock.read(), FT8_SLOT_NS).due);
    let pre_step_deadline = t.deadline();
    assert_eq!(pre_step_deadline, base_plus(15.0));
    manual.step_wall(Duration::milliseconds(3_604_300));
    let reading = clock.read();
    let event = reading.last_resync.expect("a resync");
    assert_eq!(event.skew, Duration::milliseconds(3_604_300));
    let p = t.poll(&reading, FT8_SLOT_NS);
    assert_eq!(
        p,
        SlotPoll {
            due: false,
            reanchor: Some(Reanchor::ClockStep(event))
        }
    );
    let first = next_phase_with_period(event.observed_at, Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), first);
    let mut fires = Vec::new();
    for _ in 0..120 {
        manual.advance(ms(250));
        let reading = clock.read();
        if let Some(b) = poll_fire(&mut t, &reading, FT8_SLOT_NS) {
            fires.push(b);
        }
    }
    assert_eq!(fires[0], first);
    assert!(!fires.contains(&pre_step_deadline));
    let mut unique = fires.clone();
    unique.dedup();
    assert_eq!(unique, fires, "one fire per boundary");
}

#[test]
fn backward_resync_reanchors_on_the_next_poll() {
    let (manual, clock) = manual_at(605.0);
    let mut t = SlotTimer::new(&clock.read(), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), base_plus(615.0));
    manual.advance(ms(250));
    manual.step_wall(Duration::seconds(-600));
    let reading = clock.read();
    let p = t.poll(&reading, FT8_SLOT_NS);
    assert!(!p.due);
    assert!(
        matches!(p.reanchor, Some(Reanchor::ClockStep(e)) if e.skew == Duration::seconds(-600))
    );
    assert_eq!(t.deadline(), base_plus(15.0));
    let mut fired = None;
    while fired.is_none() {
        manual.advance(ms(250));
        fired = poll_fire(&mut t, &clock.read(), FT8_SLOT_NS);
    }
    assert_eq!(fired, Some(base_plus(15.0)));
}

#[test]
fn lag_without_a_step_reanchors_without_firing() {
    let mut t = SlotTimer::new(&r(5_000), Duration::zero(), FT8_SLOT_NS);
    assert_eq!(t.deadline(), at(15_000));
    let p = t.poll(&r(31_000), FT8_SLOT_NS);
    assert_eq!(
        p,
        SlotPoll {
            due: false,
            reanchor: Some(Reanchor::Lagged {
                behind: Duration::seconds(16),
                slot_start: at(30_000)
            })
        }
    );
    assert_eq!(t.deadline(), at(45_000));
}

#[test]
fn small_backward_drift_never_refires_a_boundary() {
    let (manual, clock) = manual_at(14.85);
    let mut t = SlotTimer::new(&clock.read(), Duration::zero(), FT8_SLOT_NS);
    let mut fires = Vec::new();
    manual.advance(ms(250));
    if let Some(b) = poll_fire(&mut t, &clock.read(), FT8_SLOT_NS) {
        fires.push(b);
    }
    assert_eq!(fires, vec![base_plus(15.0)]);
    manual.step_wall(Duration::milliseconds(-500));
    while manual.now() < base_plus(30.2) {
        let reading = clock.read();
        assert_eq!(reading.resync_epoch, 0);
        if let Some(b) = poll_fire(&mut t, &reading, FT8_SLOT_NS) {
            fires.push(b);
        }
        manual.advance(ms(250));
    }
    assert_eq!(fires, vec![base_plus(15.0), base_plus(30.0)]);
}

#[test]
fn due_deadline_waits_for_advance() {
    let mut t = SlotTimer::new(&r(14_750), Duration::zero(), FT8_SLOT_NS);
    assert!(t.poll(&r(15_000), FT8_SLOT_NS).due);
    assert!(t.poll(&r(15_250), FT8_SLOT_NS).due);
    assert_eq!(t.deadline(), at(15_000));
    t.advance(&r(15_250));
    assert_eq!(t.deadline(), at(30_000));
    assert!(!t.poll(&r(15_500), FT8_SLOT_NS).due);
}

#[test]
fn offset_timer_fires_on_the_phase() {
    let mut t = SlotTimer::new(&r(0), Duration::seconds(13), FT8_SLOT_NS);
    let mut fires = Vec::new();
    let mut now = 0;
    while now <= 60_000 {
        if let Some(b) = poll_fire(&mut t, &r(now), FT8_SLOT_NS) {
            fires.push(b);
        }
        now += 250;
    }
    assert_eq!(fires, vec![at(13_000), at(28_000), at(43_000), at(58_000)]);
}

#[test]
fn a_second_resync_reanchors_again() {
    let mut t = SlotTimer::new(&r(5_000), Duration::zero(), FT8_SLOT_NS);
    assert!(t
        .poll(
            &resync_reading(65_000, Duration::seconds(60), 1),
            FT8_SLOT_NS
        )
        .reanchor
        .is_some());
    assert_eq!(t.deadline(), at(75_000));
    // The same epoch does not re-anchor twice.
    let mut same = resync_reading(65_000, Duration::seconds(60), 1);
    same.now = at(66_000);
    assert_eq!(t.poll(&same, FT8_SLOT_NS).reanchor, None);
    let p = t.poll(
        &resync_reading(125_000, Duration::seconds(59), 2),
        FT8_SLOT_NS,
    );
    assert!(matches!(p.reanchor, Some(Reanchor::ClockStep(e)) if e.epoch == 2));
    assert_eq!(t.deadline(), at(135_000));
}

#[test]
fn backward_resync_never_refires_the_boundary_that_just_fired() {
    // Fires 00:00:15 at the 00:00:15.1 poll, then a -1.5 s step lands the
    // wall clock at 00:00:13.6, before the boundary that just fired.
    let (manual, clock) = manual_at(14.85);
    let mut t = SlotTimer::new(&clock.read(), Duration::zero(), FT8_SLOT_NS);
    manual.advance(ms(250));
    assert_eq!(
        poll_fire(&mut t, &clock.read(), FT8_SLOT_NS),
        Some(base_plus(15.0))
    );
    manual.step_wall(Duration::milliseconds(-1500));
    let reading = clock.read();
    let p = t.poll(&reading, FT8_SLOT_NS);
    assert!(!p.due);
    assert!(matches!(p.reanchor, Some(Reanchor::ClockStep(_))));
    assert_eq!(t.deadline(), base_plus(30.0), "00:00:15 already fired");
    let mut fires = Vec::new();
    for _ in 0..80 {
        manual.advance(ms(250));
        if let Some(at) = poll_fire(&mut t, &clock.read(), FT8_SLOT_NS) {
            fires.push(at);
        }
    }
    assert_eq!(fires, vec![base_plus(30.0)]);
}
