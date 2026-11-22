//! PAN-358: one `SlotClock` reports every wall-clock step once.

use super::*;
use chrono::TimeZone;
use std::sync::atomic::AtomicI64;

const FT8_NS: i64 = 15_000_000_000;
const FT2_NS: i64 = 3_200_000_000;

fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn secs(s: f64) -> Duration {
    Duration::nanoseconds((s * 1e9).round() as i64)
}

fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}

fn period(ns: i64) -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(ns))
}

#[test]
fn step_is_reported_once_with_its_real_size_at_every_slot_position() {
    for p in [0.1, 1.0, 5.0, 14.0, 14.9] {
        for step in [
            0.3, 0.5, 2.0, 20.0, 60.0, 3600.0, -0.3, -0.5, -3.0, -10.0, -600.0,
        ] {
            let manual = ManualClock::new(base() + secs(p));
            let clock = manual.slot_clock(period(FT8_NS));
            assert_eq!(clock.read().epoch, 0);
            manual.advance(ms(250));
            assert_eq!(clock.read().epoch, 0);
            manual.step_wall(secs(step));
            let post_step_wall = manual.now();
            let mut events = Vec::new();
            for i in 0..3 {
                if i > 0 {
                    manual.advance(ms(250));
                }
                let r = clock.read();
                if let Some(e) = r.last_event {
                    if !events.contains(&e) {
                        events.push(e);
                    }
                }
                assert_eq!(r.epoch, 1, "p={p} step={step}");
            }
            assert_eq!(events.len(), 1, "p={p} step={step}");
            let e = events[0];
            assert_eq!(e.epoch, 1);
            assert!(
                (e.skew - secs(step)).num_nanoseconds().unwrap().abs() <= 1,
                "skew {:?} for step {step}",
                e.skew
            );
            assert_eq!(e.observed_at, post_step_wall);
            assert_eq!(e.resync, step.abs() >= 1.0, "p={p} step={step}");
        }
    }
}

#[test]
fn a_reading_after_a_step_always_carries_the_new_epoch() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(FT8_NS));
    assert_eq!(clock.read().epoch, 0);
    manual.step_wall(Duration::seconds(60));
    let r = clock.read();
    assert_eq!(r.epoch, 1);
    assert!(r.last_event.is_some());
    assert_eq!(r.resync_epoch, 1);
    assert_eq!(r.last_resync, r.last_event);
}

#[test]
fn concurrent_readers_see_one_event_per_step() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(FT8_NS));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let step_at = base() + Duration::seconds(60);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let clock = clock.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let r = clock.read();
                    seen.push((r.now, r.epoch));
                }
                for _ in 0..10 {
                    let r = clock.read();
                    seen.push((r.now, r.epoch));
                }
                seen
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(20));
    manual.step_wall(Duration::seconds(60));
    std::thread::sleep(std::time::Duration::from_millis(20));
    stop.store(true, Ordering::Relaxed);
    let mut epochs = std::collections::BTreeSet::new();
    for h in handles {
        for (now, epoch) in h.join().unwrap() {
            epochs.insert(epoch);
            if now >= step_at {
                assert_eq!(epoch, 1, "post-step reading carried a pre-step epoch");
            }
        }
    }
    assert!(epochs.is_subset(&[0u64, 1].into_iter().collect()));
    assert!(epochs.contains(&1));
    assert_eq!(clock.read().epoch, 1);
}

#[test]
fn slew_and_sub_threshold_divergence_are_not_events() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(FT8_NS));
    for _ in 0..100 {
        manual.advance(ms(1_000));
        manual.step_wall(Duration::microseconds(500));
        assert_eq!(clock.read().epoch, 0);
    }
    manual.step_wall(Duration::milliseconds(249));
    assert_eq!(clock.read().epoch, 0);
    manual.step_wall(Duration::milliseconds(-249));
    assert_eq!(clock.read().epoch, 0);
}

#[test]
fn resync_is_set_from_one_second() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(FT8_NS));
    manual.step_wall(Duration::milliseconds(999));
    let r = clock.read();
    assert_eq!((r.epoch, r.resync_epoch), (1, 0));
    assert!(!r.last_event.unwrap().resync);
    assert_eq!(r.last_resync, None);
    manual.step_wall(Duration::milliseconds(1_000));
    let r = clock.read();
    assert_eq!((r.epoch, r.resync_epoch), (2, 1));
    assert_eq!(r.last_resync.unwrap().epoch, 2);
}

#[test]
fn without_step_detection_never_reports_an_event() {
    let manual = ManualClock::new(base());
    let clock = SlotClock::without_step_detection(manual.wall(), period(FT8_NS));
    manual.step_wall(Duration::seconds(3_600));
    assert_eq!(clock.read().epoch, 0);
    manual.step_wall(Duration::seconds(-600));
    let r = clock.read();
    assert_eq!((r.epoch, r.resync_epoch, r.last_event), (0, 0, None));
    assert_eq!(r.now, manual.now());
}

#[test]
fn event_slot_start_is_the_post_step_slot() {
    let manual = ManualClock::new(base() + secs(5.0));
    let clock = manual.slot_clock(period(FT8_NS));
    clock.read();
    manual.step_wall(secs(67.5));
    let e = clock.read().last_resync.unwrap();
    assert_eq!(e.slot_start(), base() + Duration::seconds(60));
    assert_eq!(e.jump(), WallClockJump::Forward(secs(67.5)));

    let manual = ManualClock::new(base() + secs(1.0));
    let clock = manual.slot_clock(period(FT2_NS));
    clock.read();
    manual.step_wall(Duration::seconds(60));
    let e = clock.read().last_resync.unwrap();
    assert_eq!(e.slot_ns, FT2_NS);
    assert_eq!(e.slot_start(), base() + Duration::milliseconds(60_800));
}

#[test]
fn non_positive_period_falls_back_to_ft8() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(0));
    assert_eq!(clock.read().slot_ns, crate::slot::SLOT_NS);
}

#[test]
fn clones_share_one_detector() {
    let manual = ManualClock::new(base());
    let clock = manual.slot_clock(period(FT8_NS));
    let other = clock.clone();
    assert!(clock.same_clock(&other));
    manual.step_wall(Duration::seconds(-10));
    assert_eq!(other.read().epoch, 1);
    assert_eq!(clock.read().epoch, 1);
    let unrelated = manual.slot_clock(period(FT8_NS));
    assert!(!clock.same_clock(&unrelated));
}

#[test]
fn a_read_preempted_between_its_samples_is_not_a_step() {
    // The reader is descheduled for 1.2 s between the wall read and the
    // monotonic read: both clocks really moved 1.2 s, but a single wall-then-
    // monotonic sample would see wall +0 s against monotonic +1.2 s.
    let manual = ManualClock::new(chrono::Utc::now());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mono_manual = manual.clone();
    let mono_calls = calls.clone();
    let inner = manual.mono();
    let mono: MonoClock = Arc::new(move || {
        // Call 1 is construction, call 2 the first read's opening sample;
        // the preemption lands before call 3.
        if mono_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
            mono_manual.advance(std::time::Duration::from_millis(1_200));
        }
        inner()
    });
    let clock = SlotClock::new(
        manual.wall(),
        mono,
        Arc::new(std::sync::atomic::AtomicI64::new(crate::slot::SLOT_NS)),
    );
    let first = clock.read();
    assert_eq!(first.epoch, 0, "a torn sample must not be a step");
    manual.advance(std::time::Duration::from_millis(250));
    let second = clock.read();
    assert_eq!(second.epoch, 0, "nor may it leave a torn baseline behind");
    assert_eq!(second.now, manual.now());
}

#[test]
fn a_large_step_is_reported_even_when_every_bracket_stays_wide() {
    // Every monotonic read is 100 ms after the previous one, so no bracket
    // is ever tight. A +60 s step is far larger than the bracket's
    // uncertainty and must still be reported with the post-step reading.
    let manual = ManualClock::new(chrono::Utc::now());
    let mono_manual = manual.clone();
    let inner = manual.mono();
    let mono: MonoClock = Arc::new(move || {
        let now = inner();
        mono_manual.advance(std::time::Duration::from_millis(100));
        now
    });
    let clock = SlotClock::new(
        manual.wall(),
        mono,
        Arc::new(std::sync::atomic::AtomicI64::new(crate::slot::SLOT_NS)),
    );
    let before = clock.read();
    assert_eq!(before.epoch, 0, "a wide bracket alone is not a step");
    manual.step_wall(chrono::Duration::seconds(60));
    let after = clock.read();
    assert_eq!(after.epoch, 1, "a post-step reading carries the new epoch");
    assert_eq!(after.resync_epoch, 1);
    let event = after.last_event.expect("the step is recorded");
    assert_eq!(event.observed_at, after.now);
    let skew_ms = event.skew.num_milliseconds();
    assert!(
        (59_800..=60_200).contains(&skew_ms),
        "skew within the bracket's uncertainty of +60 s: {skew_ms} ms"
    );
    assert_eq!(clock.read().epoch, 1, "the step is reported once");
}
