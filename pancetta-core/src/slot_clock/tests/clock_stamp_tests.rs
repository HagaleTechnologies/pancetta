//! PAN-289 age rule on `ClockStamp`, moved from the queued-call `QueueStamp`.

use super::*;
use chrono::TimeZone;
use std::sync::atomic::AtomicI64;

fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn stamp(wall_s: i64, mono_s: u64) -> ClockStamp {
    ClockStamp {
        wall: base() + Duration::seconds(wall_s),
        mono: std::time::Duration::from_secs(mono_s),
    }
}

#[test]
fn age_counts_a_suspend_on_the_wall_clock() {
    let then = stamp(0, 0);
    assert_eq!(
        then.age_at(stamp(3_600, 1)),
        std::time::Duration::from_secs(3_600)
    );
}

#[test]
fn backward_wall_step_does_not_extend_age() {
    let then = stamp(1_000, 0);
    assert_eq!(
        then.age_at(stamp(400, 30)),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn forward_wall_step_ages_a_stamp() {
    let then = stamp(0, 0);
    assert_eq!(
        then.age_at(stamp(60, 1)),
        std::time::Duration::from_secs(60)
    );
}

#[test]
fn stamp_takes_wall_and_mono_from_one_sample() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(15_000_000_000)));
    manual.advance(std::time::Duration::from_secs(7));
    let s = clock.stamp();
    assert_eq!(s.wall, manual.now());
    assert_eq!(s.mono, (manual.mono())());
    // `stamp` observes: a pending step is recorded by it.
    manual.step_wall(Duration::seconds(60));
    let s2 = clock.stamp();
    assert_eq!(s2.wall, manual.now());
    assert_eq!(clock.read().epoch, 1);
    assert_eq!(clock.age_of(&s), std::time::Duration::from_secs(60));
}
