//! PAN-447: a band change must not leave Fox mode on with no Fox CQ on the
//! air. The autonomous hop waits while Fox mode is on; a `BandChanged` that
//! still arrives (band key, rig dial, or a race) ends Fox mode, clears the
//! FOX chip and says why. Driven through the real Qso component.

use super::respond_to_caller_admission_tests::{pan76_set_fox_mode, pan76_station};
use crate::coordinator::band_hop::{execute_band_hop, BandHopCtx, BandHopOutcome};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage};
use pancetta_core::{Band, DiagnosticLevel};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FROM_HZ: u64 = 14_074_000;
const TO_HZ: u64 = 18_100_000;
const FOX_ENDED: &str = "Fox mode OFF — band changed";

/// Drain the Tui channel into a `Vec` for up to 2 s, stopping early once
/// `done` matches a message. Keeps every message it read, so the caller can
/// assert on all of them.
async fn collect_until(
    rx: &crossbeam_channel::Receiver<ComponentMessage>,
    done: impl Fn(&MessageType) -> bool,
) -> Vec<MessageType> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    loop {
        while let Ok(msg) = rx.try_recv() {
            let stop = done(&msg.message_type);
            seen.push(msg.message_type);
            if stop {
                return seen;
            }
        }
        if Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn send_band_changed(coordinator: &crate::coordinator::ApplicationCoordinator) {
    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::BandChanged {
                previous_hz: FROM_HZ,
                new_hz: TO_HZ,
            }),
            Instant::now(),
        ))
        .await
        .unwrap();
}

fn is_status(m: &MessageType, text: &str) -> bool {
    matches!(m, MessageType::StatusUpdate { text: t, .. } if t == text)
}

fn is_fox_status(m: &MessageType) -> bool {
    matches!(m, MessageType::FoxModeStatus { .. })
}

fn mentions_fox_off(m: &MessageType) -> bool {
    match m {
        MessageType::StatusUpdate { text, .. } => text.contains("Fox mode OFF"),
        MessageType::DiagnosticEvent { text, .. } => text.contains("Fox mode OFF"),
        _ => false,
    }
}

async fn fox_cq_id(manager: &pancetta_qso::QsoManager) -> Option<pancetta_qso::QsoId> {
    manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, p)| matches!(p.state, pancetta_qso::QsoState::CallingCq { .. }))
        .map(|(id, _)| id)
}

/// AC5: a `BandChanged` in Fox mode ends Fox mode, drops the cancelled CQ
/// from the Fox set, echoes `FoxModeStatus { on: false }` and says why on
/// the status line and in Diagnostics; the admission cap falls back to
/// `max_concurrent_qsos`.
#[tokio::test]
async fn band_change_in_fox_mode_ends_fox_mode_and_says_why() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cq_id = fox_cq_id(&manager).await.expect("Fox mode must start a CQ");
    let cq_key = crate::coordinator::active_tx_qso_key(&cq_id.to_string());
    assert!(coordinator
        .fox_admitted_qsos
        .read()
        .unwrap()
        .contains(&cq_key));
    while tui_rx.try_recv().is_ok() {}

    send_band_changed(&coordinator).await;
    let seen = collect_until(&tui_rx, is_fox_status).await;

    assert!(
        seen.iter()
            .any(|m| matches!(m, MessageType::FoxModeStatus { on: false })),
        "the FOX chip must clear: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|m| matches!(m, MessageType::FoxModeStatus { on: true })),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(
            m,
            MessageType::StatusUpdate { text, level: DiagnosticLevel::Warn } if text == FOX_ENDED
        )),
        "a Warn status line must say why: {seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(
            m,
            MessageType::DiagnosticEvent { text, level: DiagnosticLevel::Warn, .. }
                if text == FOX_ENDED
        )),
        "a Warn Diagnostics line must say why: {seen:?}"
    );
    assert!(
        !coordinator.fox_mode().load(Ordering::Relaxed),
        "fox_mode must be cleared"
    );
    assert!(
        manager.get_active_qsos().await.is_empty(),
        "the band change cancels every active QSO"
    );
    assert!(
        !coordinator
            .fox_admitted_qsos
            .read()
            .unwrap()
            .contains(&cq_key),
        "the cancelled Fox CQ must leave fox_admitted_qsos"
    );
    let snap = crate::coordinator::qso::capacity_snapshot(
        &manager,
        1,
        &coordinator.fox_mode,
        &coordinator.fox_max_streams,
        &coordinator.fox_admitted_qsos,
    )
    .await;
    assert!(!snap.fox_mode);
    assert_eq!(snap.cap, 1, "the cap falls back to max_concurrent_qsos");

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// AC6: without Fox mode a band change is reported as before, with no Fox
/// line and no `FoxModeStatus`.
#[tokio::test]
async fn band_change_without_fox_mode_sends_no_fox_status() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, None)
        .await
        .expect("open one QSO");
    assert_eq!(manager.get_active_qsos().await.len(), 1);
    while tui_rx.try_recv().is_ok() {}

    send_band_changed(&coordinator).await;
    let ended = "Band change — 1 active QSO(s) ended";
    let mut seen = collect_until(&tui_rx, |m| is_status(m, ended)).await;
    assert!(
        seen.iter().any(|m| is_status(m, ended)),
        "the existing band-change line must still be sent: {seen:?}"
    );
    // Give any later message from the same handler time to arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    seen.extend(tui_rx.try_iter().map(|m| m.message_type));
    assert!(!seen.iter().any(is_fox_status), "{seen:?}");
    assert!(!seen.iter().any(mentions_fox_off), "{seen:?}");
    assert!(!coordinator.fox_mode().load(Ordering::Relaxed));

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// AC1, the ticket scenario: with Fox mode on and the Fox CQ calling
/// unanswered, the autonomous hop waits and the Fox CQ keeps calling.
#[tokio::test]
async fn autonomous_hop_during_fox_cq_waits_and_keeps_the_cq() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cq_id = fox_cq_id(&manager).await.expect("Fox mode must start a CQ");
    while tui_rx.try_recv().is_ok() {}

    let ctx = BandHopCtx {
        bus: coordinator.message_bus.clone(),
        operating_frequency_hz: Arc::new(AtomicU64::new(FROM_HZ)),
        last_freq_command: Arc::new(std::sync::Mutex::new(None)),
        split_tx_hz: Arc::new(AtomicU64::new(0)),
        display_feed_enabled: Arc::new(AtomicBool::new(false)),
        ptt_active: Arc::new(AtomicBool::new(false)),
        ptt_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
        active_is_ft4: false,
        fox_mode: coordinator.fox_mode(),
    };
    let reason = pancetta_qso::BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band17m,
        to_needed: 2,
        to_atno: 0,
        from_needed: 0,
        idle_mins: 10,
        grey_line: None,
    };
    // A Fox CQ in CallingCq is not an exchange: only Fox mode holds the hop.
    let exchanges =
        crate::coordinator::spot_call::exchange_in_progress(Some(manager.clone())).await;
    assert!(exchanges.is_empty(), "{exchanges:?}");

    let outcome = execute_band_hop(&ctx, TO_HZ, &reason, &exchanges).await;
    assert_eq!(
        outcome,
        BandHopOutcome::Waiting("Band hop 20m -> 17m waiting: Fox mode is on".into())
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen: Vec<MessageType> = tui_rx.try_iter().map(|m| m.message_type).collect();
    assert!(
        !seen.iter().any(|m| matches!(
            m,
            MessageType::StatusUpdate { text, .. } if text.starts_with("Band change")
        )),
        "no BandChanged reached the QSO component: {seen:?}"
    );
    assert!(!seen.iter().any(is_fox_status), "{seen:?}");
    assert_eq!(ctx.operating_frequency_hz.load(Ordering::Relaxed), FROM_HZ);
    assert!(ctx.last_freq_command.lock().unwrap().is_none());
    assert!(
        coordinator.fox_mode().load(Ordering::Relaxed),
        "Fox mode stays on"
    );
    let active = manager.get_active_qsos().await;
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(active[0].0, cq_id, "the same Fox CQ is still active");
    assert!(
        matches!(active[0].1.state, pancetta_qso::QsoState::CallingCq { .. }),
        "the Fox CQ keeps calling: {:?}",
        active[0].1.state
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}
