//! PAN-483: the TX worker's pre-PTT wait on the shared slot clock. A frame
//! scheduled under an older resync epoch is dropped with the step's real
//! size; a sub-second step keeps it.

use super::*;
use pancetta_core::slot::SLOT_NS;
use pancetta_core::slot_clock::{ManualClock, WallClockJump};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::Arc;

fn at(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap() + SignedDuration::from_secs(seconds)
}

fn manual_at(start: UtcTimestamp) -> (ManualClock, pancetta_core::SlotClock) {
    let manual = ManualClock::new(start);
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(SLOT_NS)));
    (manual, clock)
}

#[test]
fn resync_since_reports_the_event_once_the_epoch_moves() {
    let (manual, clock) = manual_at(at(5));
    let scheduled = clock.read();
    assert_eq!(resync_since(&scheduled, scheduled.resync_epoch), None);

    // A 0.5 s step is an event, not a resync: the epoch does not move.
    manual.step_wall(SignedDuration::from_millis(500));
    let after_small = clock.read();
    assert_eq!(after_small.epoch, 1);
    assert_eq!(resync_since(&after_small, scheduled.resync_epoch), None);

    // A -5 s step is a resync: reported with its real size.
    manual.step_wall(-SignedDuration::from_secs(5));
    let after_resync = clock.read();
    assert_eq!(
        resync_since(&after_resync, scheduled.resync_epoch),
        Some(WallClockJump::Backward(SignedDuration::from_secs(5)))
    );
    // A frame scheduled after the resync carries its epoch: no drop.
    assert_eq!(resync_since(&after_resync, after_resync.resync_epoch), None);
}

#[test]
fn ptt_wait_drops_a_frame_scheduled_before_a_resync() {
    let (manual, clock) = manual_at(at(5));
    let scheduled = clock.read();
    let ptt_target = at(15) - SignedDuration::from_millis(80);
    manual.step_wall(SignedDuration::from_secs(2));
    // The target is still 8 s ahead, but the schedule is on pre-step time.
    let reading = clock.read();
    assert!(ptt_target - reading.now > SignedDuration::from_secs(7));
    assert_eq!(
        ptt_wait_step(&reading, ptt_target, scheduled.resync_epoch),
        PttWaitStep::ClockJumped(WallClockJump::Forward(SignedDuration::from_secs(2)))
    );
}

#[test]
fn ptt_wait_keeps_a_frame_across_a_sub_second_step() {
    let (manual, clock) = manual_at(at(5));
    let scheduled = clock.read();
    let ptt_target = at(15) - SignedDuration::from_millis(80);
    manual.step_wall(SignedDuration::from_millis(500));
    assert_eq!(
        ptt_wait_step(&clock.read(), ptt_target, scheduled.resync_epoch),
        PttWaitStep::Sleep(Duration::from_millis(50))
    );
    manual.advance(Duration::from_millis(9_420));
    assert_eq!(
        ptt_wait_step(&clock.read(), ptt_target, scheduled.resync_epoch),
        PttWaitStep::Reached
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_ptt_instant_sees_a_resync_within_one_chunk() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (manual, clock) = manual_at(at(5));
    let scheduled = clock.read().resync_epoch;
    let ptt_target = at(15) - SignedDuration::from_millis(80);
    let stepper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        manual.step_wall(SignedDuration::from_secs(3600));
    });
    let started = tokio::time::Instant::now();
    let outcome = wait_for_ptt_instant(ptt_target, scheduled, &shutdown, &abort, &clock).await;
    let elapsed = started.elapsed();
    stepper.await.unwrap();
    assert_eq!(
        outcome,
        PttWait::ClockJumped(WallClockJump::Forward(SignedDuration::from_secs(3600)))
    );
    assert!(
        elapsed <= Duration::from_millis(250),
        "the resync must be seen within one 50 ms chunk (elapsed={elapsed:?})"
    );
}

/// PAN-483 (AC4): a resync landing after Step 4d and before the Step 6
/// hold releases PTT through the hold guard before the drop report. The
/// +2 s step leaves the slot boundary behind `now`, so the PAN-114 hold
/// bound this replaces would have kept the frame and keyed its audio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resync_before_the_step_6_hold_releases_ptt_before_the_drop() {
    use super::worker_gate_tests::{request, worker_harness_with_slot_clock};
    use crate::message_bus::{MessageType, RigControlMessage};
    use std::sync::atomic::Ordering;

    // 1.2 s before a slot boundary, so the frame targets it with no wait
    // beyond the clock the ticker below drives.
    let boundary = pancetta_core::slot::next_slot_start_with_period(
        UtcTimestamp::now(),
        SignedDuration::ZERO,
        SLOT_NS,
    );
    let manual = ManualClock::new(boundary - SignedDuration::from_millis(1_200));
    let h =
        worker_harness_with_slot_clock(manual.slot_clock(Arc::new(AtomicI64::new(SLOT_NS)))).await;
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Release);
    let ticker_clock = manual.clone();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ticker_clock.advance(Duration::from_millis(10));
        }
    });
    h.coordinator
        .key_down_probe
        .armed
        .store(true, Ordering::Release);
    request(&h, "CQ N0CALL FN20").await;
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.key_down_probe.reached.notified(),
    )
    .await
    .expect("worker passes every final gate and reaches key-down");
    // Step 4d has read the clock; the step lands before Step 6's read.
    manual.step_wall(SignedDuration::from_secs(2));
    h.coordinator.key_down_probe.release.notify_one();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut ptt = Vec::new();
    let mut diagnostics = Vec::new();
    let mut completed = None;
    while std::time::Instant::now() < deadline && completed.is_none() {
        // Autonomous first: any PTT-off sent before the report is already
        // on Hamlib's channel when the report is seen.
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if message_text == "CQ N0CALL FN20" {
                    completed = Some(success);
                }
            }
        }
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                ptt.push(state);
            }
        }
        if completed.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    while let Ok(msg) = h.tui_rx.try_recv() {
        if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
            diagnostics.push(text);
        }
    }
    ticker.abort();
    assert_eq!(completed, Some(false), "the frame is reported unaired");
    assert_eq!(
        ptt,
        vec![true, false],
        "PTT keyed, then released before the drop report"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("dropping stale TX after a +2.0 s clock jump")),
        "{diagnostics:?}"
    );
}
