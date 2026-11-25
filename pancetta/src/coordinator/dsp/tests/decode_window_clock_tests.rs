//! PAN-358: the DSP decode-window cutter on the shared slot clock.
//!
//! The cutter polls a `SlotTimer` with every audio batch through
//! [`decode_window_clock_step`]. A resync in either direction, or a deadline
//! more than one slot late with no step, flushes the buffer and re-anchors
//! the window to the first decode phase after the step without emitting; a
//! sub-second step keeps buffer and deadline.

use super::{
    decode_window_clock_step, decode_window_reanchor_text, decode_window_slot, overload_slot_index,
    Reanchor, SlotTimer,
};
use crate::coordinator::derive_dsp_timing;
use jiff::SignedDuration;
use pancetta_core::slot_clock::ManualClock;
use pancetta_core::time::UtcTimestamp;
use pancetta_core::SlotClock;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

const FT8_SLOT_NS: i64 = 15_000_000_000;
const FT4_SLOT_NS: i64 = 7_500_000_000;
const SR: usize = 12_000;
/// 0.1 s of 12 kHz audio per batch.
const BATCH: usize = SR / 10;

fn decode_phase() -> SignedDuration {
    SignedDuration::from_secs(13)
}

/// 2026-01-01 00:00:`secs` UTC (00:00:00 is an FT8 boundary).
fn at(secs: i64) -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap() + SignedDuration::from_secs(secs)
}

/// A manual clock `p` seconds into an FT8 slot, its slot clock, and a
/// decode-window timer seeded on it.
fn cutter_at(p: i64) -> (ManualClock, SlotClock, SlotTimer) {
    let manual = ManualClock::new(at(p));
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(FT8_SLOT_NS)));
    let timer = SlotTimer::new(&clock.read(), decode_phase(), FT8_SLOT_NS);
    (manual, clock, timer)
}

#[test]
fn forward_resync_flushes_and_reanchors() {
    let (manual, clock, mut timer) = cutter_at(5);
    assert_eq!(timer.deadline(), at(13));
    manual.advance(std::time::Duration::from_secs(1));
    manual.step_wall(SignedDuration::from_secs(60));
    let reading = clock.read();
    let p = decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS);
    assert!(p.flush, "the buffer spans the step");
    assert!(!p.due, "a resync never emits the pre-step window");
    let Some(Reanchor::ClockStep(event)) = p.reanchor else {
        panic!("expected a clock-step re-anchor, got {:?}", p.reanchor);
    };
    assert_eq!(event.skew, SignedDuration::from_secs(60));
    // First decode phase after the step: 00:01:06 -> 00:01:13.
    assert_eq!(timer.deadline(), at(73));
    assert!(decode_window_reanchor_text(&Reanchor::ClockStep(event))
        .contains("after a +60.0 s clock step"));
}

#[test]
fn resync_seen_first_by_another_reader_never_leaves_a_past_deadline() {
    let (manual, clock, mut timer) = cutter_at(5);
    assert_eq!(timer.deadline(), at(13));
    manual.advance(std::time::Duration::from_millis(7_900));
    manual.step_wall(SignedDuration::from_secs(60));
    // Another consumer observes the step at 00:01:12.9 ...
    let other = clock.read();
    assert_eq!(other.resync_epoch, 1);
    // ... and the DSP's next read lands after the 00:01:13 decode phase.
    manual.advance(std::time::Duration::from_millis(130));
    let reading = clock.read();
    let p = decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS);
    assert!(p.flush && !p.due);
    assert!(matches!(p.reanchor, Some(Reanchor::ClockStep(_))));
    assert_eq!(
        timer.deadline(),
        at(88),
        "a decode phase already past at the DSP's read is skipped"
    );
    manual.advance(std::time::Duration::from_millis(100));
    let next = decode_window_clock_step(&mut timer, &clock.read(), FT8_SLOT_NS);
    assert!(
        !next.due,
        "the flushed buffer is not emitted under the 00:01:00 slot"
    );
}

#[test]
fn backward_resync_flushes_and_reanchors() {
    let (manual, clock, mut timer) = cutter_at(125);
    assert_eq!(timer.deadline(), at(133));
    manual.advance(std::time::Duration::from_secs(1));
    manual.step_wall(SignedDuration::from_secs(-60));
    let reading = clock.read();
    let p = decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS);
    assert!(p.flush, "the buffer spans the step");
    assert!(!p.due);
    let Some(Reanchor::ClockStep(event)) = p.reanchor else {
        panic!("expected a clock-step re-anchor, got {:?}", p.reanchor);
    };
    assert_eq!(event.skew, SignedDuration::from_secs(-60));
    // 00:01:06 after the step: the next decode phase is 00:01:13, not the
    // pre-step deadline a minute later.
    assert_eq!(timer.deadline(), at(73));
    assert!(decode_window_reanchor_text(&Reanchor::ClockStep(event))
        .contains("after a -60.0 s clock step"));
}

#[test]
fn lag_without_a_step_flushes_and_reports_lag_not_a_jump() {
    let (manual, clock, mut timer) = cutter_at(5);
    // Both clocks move 25 s with no poll (buffer starvation): 17 s past the
    // 00:00:13 deadline, more than one slot.
    manual.advance(std::time::Duration::from_secs(25));
    let reading = clock.read();
    assert!(reading.last_event.is_none(), "no clock step happened");
    let p = decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS);
    assert!(p.flush, "a lagged buffer is flushed like a resync");
    assert!(!p.due);
    let reanchor = p.reanchor.expect("a lag re-anchors");
    assert!(matches!(reanchor, Reanchor::Lagged { .. }), "{reanchor:?}");
    assert_eq!(timer.deadline(), at(43));
    assert_eq!(
        decode_window_reanchor_text(&reanchor),
        "DSP decode window more than one slot late; re-anchored to UTC (no clock step)"
    );
}

#[test]
fn sub_second_step_keeps_buffer_and_deadline() {
    let (manual, clock, mut timer) = cutter_at(5);
    manual.advance(std::time::Duration::from_secs(1));
    manual.step_wall(SignedDuration::from_millis(500));
    let reading = clock.read();
    let event = reading.last_event.expect("a 500 ms step is a clock event");
    assert!(!event.resync, "but not a resync");
    let p = decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS);
    assert!(!p.flush, "the buffer is kept");
    assert!(!p.due);
    assert_eq!(p.reanchor, None);
    assert_eq!(timer.deadline(), at(13), "the deadline is kept");
}

#[test]
fn first_window_after_a_resync_is_anchored_in_the_post_step_slot() {
    let timing = derive_dsp_timing(&pancetta_ft8::ProtocolParams::from_protocol(
        pancetta_ft8::Protocol::Ft8,
    ));
    let (manual, clock, mut timer) = cutter_at(5);
    let mut buffered = 0usize;
    let mut first_boundary = None;
    // The DSP loop's per-batch order: append, poll, flush on a re-anchor,
    // emit when the buffer is full and the deadline is due, then advance.
    for batch in 0..400 {
        manual.advance(std::time::Duration::from_millis(100));
        if batch == 10 {
            // +60 s step 6 s into the slot: 00:00:06 -> 00:01:06.
            manual.step_wall(SignedDuration::from_secs(60));
        }
        buffered += BATCH;
        let reading = clock.read();
        let p = decode_window_clock_step(&mut timer, &reading, timing.slot_ns);
        if p.flush {
            buffered = 0;
        }
        if buffered >= timing.window_samples && p.due {
            first_boundary = Some(timer.deadline() - timing.decode_phase);
            break;
        }
    }
    let boundary = first_boundary.expect("a window is emitted after the step");
    let event = clock.read().last_resync.expect("the step was a resync");
    assert_eq!(event.slot_start(), at(60));
    assert!(
        boundary >= at(60),
        "first window anchored at {boundary}, before the post-step slot"
    );
}

#[test]
fn overload_slot_index_uses_the_active_period() {
    let epoch = UtcTimestamp::UNIX_EPOCH;
    // FT4: the 1001st 7.5 s slot, not the 500th 15 s slot.
    let ft4_boundary = epoch + SignedDuration::from_millis(7_500 * 1001);
    assert_eq!(overload_slot_index(ft4_boundary, FT4_SLOT_NS), 1001);
    assert_ne!(
        overload_slot_index(ft4_boundary, FT4_SLOT_NS),
        ft4_boundary.as_second().div_euclid(15)
    );
    // FT8: the same number as the former 15 s index.
    let ft8_boundary = at(75);
    assert_eq!(
        overload_slot_index(ft8_boundary, FT8_SLOT_NS),
        ft8_boundary.as_second().div_euclid(15)
    );
}

/// PAN-477: the slot a due window is cut from is the timer's deadline minus
/// the decode phase, on the period the DSP cut it with. For FT8 that is the
/// 00:00:00 slot at the 00:00:13 deadline; FT4's 6.5 s phase cuts the
/// 00:00:00 slot at 00:00:06.5, and after an advance the next one.
#[test]
fn decode_window_slot_is_the_deadline_minus_the_decode_phase() {
    let (manual, clock, mut timer) = cutter_at(5);
    assert_eq!(timer.deadline(), at(13));
    let slot = decode_window_slot(&timer, decode_phase(), FT8_SLOT_NS);
    assert_eq!(slot.start, timer.deadline() - decode_phase());
    assert_eq!(slot.start, at(0));
    assert_eq!(slot.slot_ns, FT8_SLOT_NS);

    manual.advance(std::time::Duration::from_secs(8));
    let reading = clock.read();
    assert!(decode_window_clock_step(&mut timer, &reading, FT8_SLOT_NS).due);
    timer.advance(&reading);
    let next = decode_window_slot(&timer, decode_phase(), FT8_SLOT_NS);
    assert_eq!(next.start, at(15));

    let ft4_phase = SignedDuration::from_millis(6_500);
    let manual = ManualClock::new(at(1));
    let ft4_clock = manual.slot_clock(Arc::new(AtomicI64::new(FT4_SLOT_NS)));
    let ft4_timer = SlotTimer::new(&ft4_clock.read(), ft4_phase, FT4_SLOT_NS);
    let ft4 = decode_window_slot(&ft4_timer, ft4_phase, FT4_SLOT_NS);
    assert_eq!(ft4_timer.deadline(), at(0) + ft4_phase);
    assert_eq!(ft4.start, at(0));
    assert_eq!(ft4.slot_ns, FT4_SLOT_NS);
}
