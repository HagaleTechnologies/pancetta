// PAN-484: the TX worker reads the coordinator's shared slot clock. A frame
// scheduled under an older resync epoch (a wall-clock step of 1 s or more
// since its scheduling read) is never keyed: the pre-PTT wait, the Step 4d
// final gate and the Step 6 hold drop it, releasing PTT first if keyed.
use super::worker_gate_tests::{await_completion, build_worker_harness, request, Harness};
use super::*;
use crate::message_bus::{RigControlMessage, TransmitRequestItem, TxOrigin};
use pancetta_core::slot::SLOT_NS;
use pancetta_core::slot_clock::{ManualClock, WallClockJump};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use std::sync::Arc;

const TEXT: &str = "CQ N0CALL FN20";
const BUNDLE_TEXTS: [&str; 2] = ["CQ N0CALL FN20", "CQ N0CALL EN27"];

fn ft8_slot_ns() -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(SLOT_NS))
}

/// A worker whose coordinator reads a stepping `ManualClock` frozen 500 ms
/// into an even slot. With `tx_self_parity = Even` the queue-time decision
/// picks that current slot, so Step 4 returns at once, and the frozen clock
/// stays inside the key-time late cap: only a resync can drop the frame.
async fn stepping_harness() -> (Harness, ManualClock) {
    let origin = pancetta_core::time::slot_grid_origin();
    let manual = ManualClock::new(origin + SignedDuration::from_millis(500));
    let installed = manual.clone();
    let mut cfg = pancetta_config::Config::default();
    cfg.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let h = build_worker_harness(
        cfg,
        move |coordinator| {
            coordinator.slot_clock = installed.slot_clock(coordinator.active_slot_ns());
        },
        Arc::new(pancetta_agent::arm::ArmTime::now),
        pancetta_core::time::system_utc_clock(),
        true,
    )
    .await;
    (h, manual)
}

async fn send_bundle(h: &Harness) {
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

fn ptt_state(msg: &ComponentMessage) -> Option<bool> {
    match msg.message_type {
        MessageType::RigControl(RigControlMessage::SetPtt { state }) => Some(state),
        _ => None,
    }
}

/// What a bundle's drop produced: the PTT states Hamlib saw (in order, as of
/// the moment the last completion arrived), the failed completions per
/// text, and every TUI diagnostic.
struct Outcome {
    ptt: Vec<bool>,
    failed: Vec<String>,
    diagnostics: Vec<String>,
}

/// Collect until every text in `texts` has a `TransmitComplete`. Hamlib is
/// drained right after the last completion is seen, so a PTT-off queued
/// before that report is in `ptt`.
async fn await_bundle_outcome(h: &Harness, texts: &[&str]) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut out = Outcome {
        ptt: Vec::new(),
        failed: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut completed = 0;
    while Instant::now() < deadline && completed < texts.len() {
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
        out.ptt
            .extend(h.hamlib_rx.try_iter().filter_map(|msg| ptt_state(&msg)));
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                out.diagnostics.push(text);
            }
        }
        if completed < texts.len() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert_eq!(completed, texts.len(), "every frame must report");
    out
}

async fn await_before_ptt_gate(h: &Harness) {
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("worker must reach its PTT gate after building the waveform");
}

async fn await_key_down_probe(h: &Harness) {
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.key_down_probe.reached.notified(),
    )
    .await
    .expect("worker must pass every final gate and reach key-down");
}

fn clock_jump_diagnostic(diagnostics: &[String], prefix: &str) -> usize {
    diagnostics
        .iter()
        .filter(|text| text.starts_with(prefix))
        .count()
}

#[test]
fn resync_since_reports_the_step_once_the_resync_epoch_moved() {
    let manual = ManualClock::new(pancetta_core::time::slot_grid_origin());
    let clock = manual.slot_clock(ft8_slot_ns());
    let scheduled = clock.read();
    assert_eq!(resync_since(&clock.read(), scheduled.resync_epoch), None);
    manual.step_wall(SignedDuration::from_secs(2));
    let after = clock.read();
    assert_eq!(after.resync_epoch, scheduled.resync_epoch + 1);
    assert_eq!(
        resync_since(&after, scheduled.resync_epoch),
        Some(WallClockJump::Forward(SignedDuration::from_secs(2)))
    );
    // A frame scheduled from a post-step read is not dropped for that step.
    assert_eq!(resync_since(&clock.read(), after.resync_epoch), None);
}

#[test]
fn resync_since_ignores_a_sub_second_step() {
    let manual = ManualClock::new(pancetta_core::time::slot_grid_origin());
    let clock = manual.slot_clock(ft8_slot_ns());
    let scheduled = clock.read();
    manual.step_wall(SignedDuration::from_millis(500));
    let after = clock.read();
    assert_eq!(after.epoch, 1, "a 0.5 s step is an event");
    assert_eq!(after.resync_epoch, 0, "but not a resync");
    assert_eq!(resync_since(&after, scheduled.resync_epoch), None);
}

#[test]
fn ptt_wait_drops_a_frame_whose_slot_is_still_ahead_after_a_resync() {
    // Scheduled at 00:00:05 for the 00:00:15 slot; a +2 s step leaves the
    // slot 8 s ahead, and the frame is dropped all the same.
    let origin = pancetta_core::time::slot_grid_origin();
    let manual = ManualClock::new(origin + SignedDuration::from_secs(5));
    let clock = manual.slot_clock(ft8_slot_ns());
    let scheduled = clock.read();
    let ptt_target = origin + SignedDuration::from_secs(15) - SignedDuration::from_millis(80);
    assert_eq!(
        ptt_wait_step(&clock.read(), ptt_target, scheduled.resync_epoch),
        PttWaitStep::Sleep(Duration::from_millis(50))
    );
    manual.step_wall(SignedDuration::from_secs(2));
    let after = clock.read();
    assert!(ptt_target > after.now, "the slot is still ahead");
    assert_eq!(
        ptt_wait_step(&after, ptt_target, scheduled.resync_epoch),
        PttWaitStep::ClockJumped(WallClockJump::Forward(SignedDuration::from_secs(2)))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn final_gate_drops_a_frame_after_a_resync_without_keying() {
    let (h, manual) = stepping_harness().await;
    let held = h.coordinator.ptt_sync_gate.lock().await;
    request(&h, TEXT).await;
    await_before_ptt_gate(&h).await;
    // The worker is parked on the held gate, past Step 4 and before the
    // Step 4d final gate: the clock steps under it.
    manual.step_wall(SignedDuration::from_secs(2));
    drop(held);
    let (success, diagnostics, ptt_on) = await_completion(&h, TEXT, Duration::from_secs(20)).await;
    assert_eq!(success, Some(false), "the frame is reported unaired");
    assert!(!ptt_on, "a frame scheduled before a resync is never keyed");
    assert_eq!(
        clock_jump_diagnostic(&diagnostics, "dropping stale TX after a +2.0 s clock jump"),
        1,
        "one tx.policy drop line with the real step size: {diagnostics:?}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn bundle_final_gate_drops_every_item_after_a_resync_without_keying() {
    let (h, manual) = stepping_harness().await;
    let held = h.coordinator.ptt_sync_gate.lock().await;
    send_bundle(&h).await;
    await_before_ptt_gate(&h).await;
    manual.step_wall(SignedDuration::from_secs(2));
    drop(held);
    let out = await_bundle_outcome(&h, &BUNDLE_TEXTS).await;
    assert_eq!(out.failed.len(), 2, "one unaired report per item");
    assert!(
        !out.ptt.contains(&true),
        "a bundle scheduled before a resync is never keyed: {:?}",
        out.ptt
    );
    assert_eq!(
        clock_jump_diagnostic(
            &out.diagnostics,
            "dropping stale TX after a +2.0 s clock jump"
        ),
        2,
        "one tx.policy drop line per item: {:?}",
        out.diagnostics
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn resync_before_the_step_6_hold_releases_ptt_before_the_drop() {
    let (h, manual) = stepping_harness().await;
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
    request(&h, TEXT).await;
    // Parked after the Step 4d final gate, before the PTT-on enqueue.
    await_key_down_probe(&h).await;
    manual.step_wall(-SignedDuration::from_secs(5));
    h.coordinator.key_down_probe.release.notify_one();
    let out = await_bundle_outcome(&h, &[TEXT]).await;
    assert_eq!(out.failed, vec![TEXT.to_string()]);
    assert_eq!(
        out.ptt,
        vec![true, false],
        "PTT keys, then the Clock-step PTT hold guard releases it before the drop report"
    );
    assert_eq!(
        clock_jump_diagnostic(
            &out.diagnostics,
            "dropping stale TX after a -5.0 s clock jump"
        ),
        1,
        "{:?}",
        out.diagnostics
    );
    assert_eq!(audio_rx.try_iter().count(), 0, "no audio is routed");
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn bundle_resync_before_the_step_6_hold_releases_ptt_before_the_drop() {
    let (h, manual) = stepping_harness().await;
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
    await_key_down_probe(&h).await;
    manual.step_wall(-SignedDuration::from_secs(5));
    h.coordinator.key_down_probe.release.notify_one();
    let out = await_bundle_outcome(&h, &BUNDLE_TEXTS).await;
    assert_eq!(out.failed.len(), 2, "one unaired report per item");
    assert_eq!(
        out.ptt,
        vec![true, false],
        "PTT keys once, then the Clock-step PTT hold guard releases it before the drop report"
    );
    assert_eq!(
        clock_jump_diagnostic(
            &out.diagnostics,
            "dropping stale TX after a -5.0 s clock jump"
        ),
        2,
        "{:?}",
        out.diagnostics
    );
    assert_eq!(audio_rx.try_iter().count(), 0, "no audio is routed");
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_rekey_takes_the_resync_epoch_of_its_own_clock_read() {
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let ptt_active = Arc::new(AtomicBool::new(false));
    let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
    let last_ptt_off_ms = Arc::new(AtomicU64::new(0));
    let mut guard = super::supersede_rekey_tests::keyed_guard(
        &bus,
        &ptt_active,
        &last_ptt_on_ms,
        &last_ptt_off_ms,
    )
    .await;

    // The in-flight frame was scheduled under resync epoch 0; then the
    // clock stepped and some reader observed it (epoch 1).
    let origin = pancetta_core::time::slot_grid_origin();
    let manual = ManualClock::new(origin + SignedDuration::from_millis(500));
    let clock = manual.slot_clock(ft8_slot_ns());
    let scheduled = clock.read();
    manual.step_wall(SignedDuration::from_secs(2));
    assert_eq!(clock.read().resync_epoch, 1);

    let mut timing = super::SlotTimingContext::ft8_for_test(
        20_000,
        12_000,
        SLOT_NS,
        pancetta_config::station::TxSelfParity::Auto,
        scheduled.now,
    );
    timing.slot_clock = clock.clone();
    let mut message_text = "OLD TEXT".to_string();
    let mut frequency_offset = 1000.0;
    let mut schedule = super::schedule_tx(
        scheduled.now,
        pancetta_core::slot::SlotParity::Even,
        20_000,
        12_000,
        SLOT_NS,
    );
    let mut resync_epoch = scheduled.resync_epoch;
    let outcome = super::supersede_and_rekey_or_bundle(
        MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "NEW TEXT".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: Some(pancetta_core::slot::SlotParity::Even),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        },
        super::WorkingFrame {
            message_text: &mut message_text,
            frequency_offset: &mut frequency_offset,
            schedule: &mut schedule,
            resync_epoch: &mut resync_epoch,
        },
        &mut guard,
        &timing,
        1,
        &super::SupersedeInFlight {
            items: &[],
            origin: TxOrigin::Local,
            remote_client_key_id: None,
            tx_parity: None,
        },
    )
    .await;

    assert!(matches!(outcome, super::SupersedeOutcome::Replace { .. }));
    assert_eq!(message_text, "NEW TEXT");
    assert_eq!(
        resync_epoch, 1,
        "the re-key carries the epoch of the read that decided its schedule"
    );
    // That read, not a pre-step one, decided the schedule...
    assert_eq!(schedule.target_slot, origin);
    // ...so the re-keyed frame is not dropped for the step it already saw.
    assert_eq!(resync_since(&clock.read(), resync_epoch), None);
}
