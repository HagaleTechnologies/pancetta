//! PAN-477: the `--replay` pre-roll waits on the shared `SlotClock` with
//! the active mode's period, and a wall-clock step during the wait never
//! starts the feed mid-slot.

use super::{
    replay_preroll_poll, replay_preroll_target, replay_preroll_wait, PrerollPoll,
    REPLAY_PREROLL_POLL,
};
use jiff::SignedDuration;
use pancetta_core::slot_clock::{ManualClock, SlotReading};
use pancetta_core::time::UtcTimestamp;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::Duration;

const FT8_NS: i64 = 15_000_000_000;
const FT4_NS: i64 = 7_500_000_000;
const FT2_NS: i64 = 3_200_000_000;

fn at(second: u32) -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, second).unwrap()
}

fn reading(now: UtcTimestamp, slot_ns: i64) -> SlotReading {
    SlotReading {
        now,
        slot_ns,
        epoch: 0,
        resync_epoch: 0,
        last_event: None,
        last_resync: None,
    }
}

/// Where the pre-roll started, and every wall instant `Start` was returned.
struct PrerollRun {
    starts: Vec<UtcTimestamp>,
}

/// Drive `replay_preroll_poll` over one `ManualClock`/`SlotClock` starting at
/// `start` in FT8, advancing both clocks by each `Sleep`. `step` is applied
/// once, as a wall-only step, the first time the wall reaches `step_at`.
/// Stops at the first `Start`. Every `Sleep` must be at most
/// `REPLAY_PREROLL_POLL` and positive.
fn run_preroll(start: UtcTimestamp, step: Option<(UtcTimestamp, SignedDuration)>) -> PrerollRun {
    let manual = ManualClock::new(start);
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(FT8_NS)));
    let mut target = replay_preroll_target(&clock.read());
    let mut stepped = step.is_none();
    let mut starts = Vec::new();
    for _ in 0..10_000 {
        if !stepped {
            if let Some((step_at, by)) = step {
                if manual.now() >= step_at {
                    manual.step_wall(by);
                    stepped = true;
                }
            }
        }
        let reading = clock.read();
        match replay_preroll_poll(&reading, &mut target) {
            PrerollPoll::Start => {
                starts.push(reading.now);
                return PrerollRun { starts };
            }
            PrerollPoll::Sleep(nap) => {
                assert!(
                    nap <= REPLAY_PREROLL_POLL,
                    "every sleep is at most {REPLAY_PREROLL_POLL:?}, got {nap:?}"
                );
                assert!(nap > Duration::ZERO, "a sleep always makes progress");
                manual.advance(nap);
            }
        }
    }
    panic!("the pre-roll never started");
}

/// FT8 keeps today's waits; FT4 and FT2 wait for their own next boundary.
/// The two pre-PAN-477 `replay_preroll_wait_*` tests are the FT8 rows.
#[test]
fn preroll_wait_uses_the_active_period() {
    // replay_preroll_wait_lands_on_next_boundary_mid_slot: 7 s into an FT8
    // slot that started at :00:00, the next boundary is :00:15 -> 8 s.
    assert_eq!(
        replay_preroll_wait(&reading(at(7), FT8_NS)),
        Duration::from_secs(8)
    );
    // replay_preroll_wait_on_exact_boundary_waits_a_full_slot: a read
    // exactly on a boundary advances to the next one -> the full 15 s.
    assert_eq!(
        replay_preroll_wait(&reading(at(0), FT8_NS)),
        Duration::from_secs(15)
    );
    // FT4 at :00:07: the next 7.5 s boundary is :00:07.5 -> 0.5 s.
    assert_eq!(
        replay_preroll_wait(&reading(at(7), FT4_NS)),
        Duration::from_millis(500)
    );
    // FT2 at :00:07: the next 3.2 s boundary is :00:09.6 -> 2.6 s.
    assert_eq!(
        replay_preroll_wait(&reading(at(7), FT2_NS)),
        Duration::from_millis(2_600)
    );
    // The target is that boundary.
    assert_eq!(
        replay_preroll_target(&reading(at(7), FT4_NS)),
        at(7) + SignedDuration::from_millis(500)
    );
}

/// The wall reaches 00:00:14.9 on both clocks, then steps +3 s to 00:00:17.9,
/// past the :15 boundary. Starting there would be 2.9 s into a slot, so the
/// pre-roll retargets and starts at the next boundary, :30, and never before.
#[test]
fn preroll_retargets_after_a_forward_step_past_the_boundary() {
    let run = run_preroll(
        at(7),
        Some((
            at(14) + SignedDuration::from_millis(900),
            SignedDuration::from_secs(3),
        )),
    );
    assert_eq!(run.starts, vec![at(30)], "starts at :30 and never before");
}

/// A −3 s step at 00:00:14 puts the wall back at 00:00:11: the pre-roll keeps
/// its :15 target and starts when the wall gets there.
#[test]
fn preroll_lands_on_the_wall_boundary_after_a_backward_step() {
    let run = run_preroll(at(7), Some((at(14), SignedDuration::from_secs(-3))));
    assert_eq!(run.starts, vec![at(15)], "starts at wall :15");
}

/// A −600 s step at 00:00:14 puts the wall ten minutes before the :15
/// target. The target is more than one slot ahead, so the pre-roll retargets
/// to the next boundary after the stepped wall (23:50:15 the previous day)
/// instead of idling for ten minutes.
#[test]
fn preroll_retargets_after_a_long_backward_step() {
    let run = run_preroll(at(7), Some((at(14), SignedDuration::from_secs(-600))));
    assert_eq!(
        run.starts,
        vec![at(15) - SignedDuration::from_secs(600)],
        "starts at the first boundary after the stepped wall"
    );
}

/// With no step, the pre-roll starting at 00:00:07 starts on the :15
/// boundary exactly.
#[test]
fn preroll_without_a_step_starts_on_the_boundary() {
    let run = run_preroll(at(7), None);
    assert_eq!(run.starts, vec![at(15)]);

    // Lateness inside the step threshold (a scheduler overshoot) still
    // starts; at the threshold it retargets.
    let mut target = at(15);
    assert_eq!(
        replay_preroll_poll(
            &reading(at(15) + SignedDuration::from_millis(249), FT8_NS),
            &mut target
        ),
        PrerollPoll::Start
    );
    assert_eq!(target, at(15));
    assert_eq!(
        replay_preroll_poll(
            &reading(at(15) + SignedDuration::from_millis(250), FT8_NS),
            &mut target
        ),
        PrerollPoll::Sleep(REPLAY_PREROLL_POLL)
    );
    assert_eq!(target, at(30), "retargeted to the next boundary");
}
