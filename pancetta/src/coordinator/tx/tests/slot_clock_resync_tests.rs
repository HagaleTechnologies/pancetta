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

/// PAN-484: a worker over a `ManualClock` 1.2 s before a slot boundary,
/// with a ticker driving that clock at real speed, as
/// [`resync_before_the_step_6_hold_releases_ptt_before_the_drop`] sets up.
/// The returned ticker must be aborted by the caller.
async fn ticking_harness() -> (
    super::worker_gate_tests::Harness,
    ManualClock,
    tokio::task::JoinHandle<()>,
) {
    let boundary = pancetta_core::slot::next_slot_start_with_period(
        UtcTimestamp::now(),
        SignedDuration::ZERO,
        SLOT_NS,
    );
    ticking_harness_at(boundary - SignedDuration::from_millis(1_200)).await
}

/// [`ticking_harness`] with the `ManualClock` starting at `start` (PAN-489).
pub(super) async fn ticking_harness_at(
    start: UtcTimestamp,
) -> (
    super::worker_gate_tests::Harness,
    ManualClock,
    tokio::task::JoinHandle<()>,
) {
    use super::worker_gate_tests::worker_harness_with_slot_clock;
    use std::sync::atomic::Ordering;

    let manual = ManualClock::new(start);
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
    (h, manual, ticker)
}

const BUNDLE_TEXTS: [&str; 2] = ["CQ N0CALL FN20", "CQ N0CALL EN27"];

async fn send_bundle(h: &super::worker_gate_tests::Harness) {
    use crate::message_bus::{MessageType, TransmitRequestItem, TxOrigin};
    let items = BUNDLE_TEXTS
        .iter()
        .zip([1000.0, 1600.0])
        .map(|(text, frequency_offset)| TransmitRequestItem {
            attempt_id: None,
            message_text: (*text).to_string(),
            frequency_offset,
            qso_id: None,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        })
        .collect();
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items,
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

/// What a drop produced: the PTT states Hamlib saw in order, the failed
/// completions, every TUI diagnostic and how many audio frames were routed.
#[derive(Debug)]
pub(super) struct DropOutcome {
    pub(super) ptt: Vec<bool>,
    pub(super) failed: Vec<String>,
    pub(super) diagnostics: Vec<String>,
}

/// Collect until every text in `texts` has a `TransmitComplete`. Autonomous
/// is drained before Hamlib each pass, so a PTT-off queued before the last
/// report is in `ptt`.
pub(super) async fn await_drop_outcome(
    h: &super::worker_gate_tests::Harness,
    texts: &[&str],
) -> DropOutcome {
    use crate::message_bus::{MessageType, RigControlMessage};
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut out = DropOutcome {
        ptt: Vec::new(),
        failed: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut completed = 0;
    while std::time::Instant::now() < deadline && completed < texts.len() {
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if texts.contains(&message_text.as_str()) {
                    completed += 1;
                    if !success {
                        out.failed.push(message_text);
                    }
                }
            }
        }
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                out.ptt.push(state);
            }
        }
        if completed < texts.len() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    while let Ok(msg) = h.tui_rx.try_recv() {
        if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
            out.diagnostics.push(text);
        }
    }
    assert_eq!(completed, texts.len(), "every frame must report: {out:?}");
    out
}

pub(super) fn count_prefixed(diagnostics: &[String], prefix: &str) -> usize {
    diagnostics.iter().filter(|d| d.starts_with(prefix)).count()
}

/// Park the worker on a held `ptt_sync_gate` (past Step 4, before the Step
/// 4d final gate), step the clock under it, then let it run.
async fn step_before_the_final_gate(
    h: &super::worker_gate_tests::Harness,
    manual: &ManualClock,
    step: SignedDuration,
    submit: impl std::future::Future<Output = ()>,
) {
    let held = h.coordinator.ptt_sync_gate.lock().await;
    submit.await;
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("worker reaches its PTT gate after building the waveform");
    manual.step_wall(step);
    drop(held);
}

/// PAN-484 (AC2): a resync landing between Step 4 and the Step 4d final
/// gate drops the single frame before PTT-on, with the real step size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_gate_drops_a_frame_after_a_resync_without_keying() {
    use super::worker_gate_tests::request;
    use std::sync::atomic::Ordering;
    let (h, manual, ticker) = ticking_harness().await;
    step_before_the_final_gate(
        &h,
        &manual,
        SignedDuration::from_secs(2),
        request(&h, "CQ N0CALL FN20"),
    )
    .await;
    let out = await_drop_outcome(&h, &["CQ N0CALL FN20"]).await;
    ticker.abort();
    assert_eq!(out.failed, vec!["CQ N0CALL FN20".to_string()], "{out:?}");
    assert!(!out.ptt.contains(&true), "never keyed: {out:?}");
    assert_eq!(
        count_prefixed(
            &out.diagnostics,
            "dropping stale TX after a +2.0 s clock jump"
        ),
        1,
        "{out:?}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// PAN-484 (AC2): the bundle arm's Step 4d gate drops every item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_final_gate_drops_every_item_after_a_resync_without_keying() {
    use std::sync::atomic::Ordering;
    let (h, manual, ticker) = ticking_harness().await;
    step_before_the_final_gate(&h, &manual, SignedDuration::from_secs(2), send_bundle(&h)).await;
    let out = await_drop_outcome(&h, &BUNDLE_TEXTS).await;
    ticker.abort();
    assert_eq!(out.failed.len(), 2, "one unaired report per item: {out:?}");
    assert!(!out.ptt.contains(&true), "never keyed: {out:?}");
    assert_eq!(
        count_prefixed(
            &out.diagnostics,
            "dropping stale TX after a +2.0 s clock jump"
        ),
        2,
        "one drop line per item: {out:?}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// PAN-484 (AC3): the bundle twin of
/// [`resync_before_the_step_6_hold_releases_ptt_before_the_drop`], with a
/// backward step. PTT keys once, the hold guard releases it before the
/// drop reports, and no audio is routed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_resync_before_the_step_6_hold_releases_ptt_before_the_drop() {
    use std::sync::atomic::Ordering;
    let (h, manual, ticker) = ticking_harness().await;
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    h.coordinator
        .key_down_probe
        .armed
        .store(true, Ordering::Release);
    send_bundle(&h).await;
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.key_down_probe.reached.notified(),
    )
    .await
    .expect("worker passes every final gate and reaches key-down");
    manual.step_wall(-SignedDuration::from_secs(5));
    h.coordinator.key_down_probe.release.notify_one();
    let out = await_drop_outcome(&h, &BUNDLE_TEXTS).await;
    ticker.abort();
    assert_eq!(out.failed.len(), 2, "one unaired report per item: {out:?}");
    assert_eq!(out.ptt, vec![true, false], "{out:?}");
    assert_eq!(
        count_prefixed(
            &out.diagnostics,
            "dropping stale TX after a -5.0 s clock jump"
        ),
        2,
        "{out:?}"
    );
    assert_eq!(audio_rx.try_iter().count(), 0, "no audio is routed");
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// PAN-484 (AC4): a re-key decided after a resync takes the resync epoch of
/// the read that decided its schedule, so the step it already saw does not
/// drop it. The frame is superseded mid-playback after a +1.1 s step; the
/// replacement keys and airs, and no clock-jump drop is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supersede_rekey_takes_the_resync_epoch_of_its_own_clock_read() {
    use crate::message_bus::{MessageType, TxOrigin};

    const FIRST: &str = "K1ABC N0CALL FN20";
    const SECOND: &str = "K1ABC N0CALL EN27";
    let (h, manual, ticker) = ticking_harness().await;
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-rk"));
    // Both frames want the slot the first one keys in.
    let parity =
        pancetta_core::slot::SlotParity::of(pancetta_core::slot::next_slot_start_with_period(
            manual.now(),
            SignedDuration::ZERO,
            SLOT_NS,
        ));
    let send = |text: &'static str, attempt: u64| {
        ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: Some(pancetta_qso::TxAttemptId(attempt)),
                message_text: text.into(),
                frequency_offset: 1500.0,
                qso_id: Some("qso-rk".into()),
                tx_parity: Some(parity),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        )
    };
    let mut diagnostics = Vec::new();
    let mut logged = Vec::new();
    let drain_tui = |diagnostics: &mut Vec<String>, logged: &mut Vec<String>| {
        while let Ok(msg) = h.tui_rx.try_recv() {
            match msg.message_type {
                MessageType::DiagnosticEvent { text, .. } => diagnostics.push(text),
                MessageType::TxFrameLogged { text, .. } => logged.push(text),
                _ => {}
            }
        }
    };
    h.coordinator
        .message_bus
        .send_message(send(FIRST, 1))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while !logged.iter().any(|t| t == FIRST) {
        assert!(
            std::time::Instant::now() < deadline,
            "the first frame never reached playback: {diagnostics:?}"
        );
        drain_tui(&mut diagnostics, &mut logged);
        while h.hamlib_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Mid-playback (about 0.1 s into the slot): the clock steps +1.1 s,
    // then the re-key request arrives, still inside the late cap, so the
    // supersede re-keys this slot rather than requeueing.
    manual.step_wall(SignedDuration::from_millis(1_100));
    h.coordinator
        .message_bus
        .send_message(send(SECOND, 2))
        .await
        .unwrap();
    let resent_at = std::time::Instant::now();
    let deadline = resent_at + Duration::from_secs(40);
    while !logged.iter().any(|t| t == SECOND) {
        assert!(
            std::time::Instant::now() < deadline,
            "the re-keyed frame never aired: logged {logged:?}, diagnostics {diagnostics:?}"
        );
        drain_tui(&mut diagnostics, &mut logged);
        while h.hamlib_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ticker.abort();
    // A requeued superseder waits for the next even slot, 30 s on the
    // ticking clock; a re-key airs in this one.
    assert!(
        resent_at.elapsed() < Duration::from_secs(10),
        "the replacement re-keys this slot, not a requeued later one"
    );
    assert!(
        !diagnostics.iter().any(|d| d.contains("clock jump")),
        "a re-key decided after the step is not dropped for it: {diagnostics:?}"
    );
}
