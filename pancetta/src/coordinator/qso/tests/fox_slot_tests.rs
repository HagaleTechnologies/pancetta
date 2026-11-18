//! PAN-386: coordinator handling of Fox-role QSOs.

use super::*;
use crate::coordinator::ApplicationCoordinator;
use crate::message_bus::QsoMessage;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use pancetta_qso::states::QsoProgress;
use pancetta_qso::{CallInitiation, QsoManager, QsoManagerConfig, QsoState};

use super::respond_to_caller_admission_tests::{pan76_set_fox_mode, pan76_station};

fn key(id: &pancetta_qso::QsoId) -> String {
    crate::coordinator::active_tx_qso_key(&id.to_string())
}

/// Answer `call`, heard at `freq`, through the real `maybe_answer_caller`, as
/// the PAN-76 tests do.
async fn answer(
    coordinator: &ApplicationCoordinator,
    manager: &QsoManager,
    call: &str,
    freq: f64,
    max_concurrent: usize,
) {
    let side = manager.current_tx_side().await;
    super::maybe_answer_caller(
        &pancetta_qso::states::MessageType::CqResponse {
            calling_station: "K5ARH".to_string(),
            responding_station: call.to_string(),
            grid: Some("FN31".to_string()),
        },
        "K5ARH",
        freq,
        side.map(|s| s.opposite()).or(Some(SlotParity::Odd)),
        -12.0,
        manager,
        &coordinator.tx_policy,
        pancetta_config::UnattendedPolicy::Off,
        true,
        max_concurrent,
        &coordinator.message_bus,
        &coordinator.fox_mode,
        &coordinator.fox_max_streams,
        &coordinator.fox_admitted_qsos,
    )
    .await;
}

async fn qso_for(manager: &QsoManager, call: &str) -> Option<(pancetta_qso::QsoId, QsoProgress)> {
    manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, p)| p.metadata.their_callsign.as_deref() == Some(call))
}

async fn calling_cq(manager: &QsoManager) -> Vec<(pancetta_qso::QsoId, QsoProgress)> {
    manager
        .get_active_qsos()
        .await
        .into_iter()
        .filter(|(_, p)| matches!(p.state, QsoState::CallingCq { .. }))
        .collect()
}

/// AC1: engaging Fox mode in FT8 starts the CQ on the 300 Hz slot, as a
/// Fox-role, Fox-admitted QSO.
#[tokio::test]
async fn pan386_fox_engage_calls_cq_on_the_300_hz_slot() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cqs = calling_cq(&manager).await;
    assert_eq!(cqs.len(), 1, "Fox mode starts exactly one CQ");
    let (cq_id, cq) = &cqs[0];
    assert_eq!(cq.metadata.frequency, 300.0);
    assert!(cq.metadata.fox, "the Fox CQ is a Fox-role QSO");
    assert!(coordinator
        .fox_admitted_qsos
        .read()
        .unwrap()
        .contains(&key(cq_id)));
}

/// AC2: two Hounds calling at 1200 and 1240 Hz are answered on the next Fox
/// slots, 360 and 420 Hz, and heard at their calling frequencies.
#[tokio::test]
async fn pan386_fox_answers_two_hounds_on_the_next_60_hz_slots() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    for (call, freq) in [("W2BBB", 1200.0), ("W3CCC", 1240.0)] {
        answer(&coordinator, &manager, call, freq, 1).await;
    }
    for (call, call_freq, slot) in [("W2BBB", 1200.0, 360.0), ("W3CCC", 1240.0, 420.0)] {
        let (id, p) = qso_for(&manager, call)
            .await
            .unwrap_or_else(|| panic!("{call} must be answered in Fox mode"));
        assert_eq!(p.metadata.frequency, slot, "{call} TX slot");
        assert_eq!(
            p.metadata.partner_freq,
            Some(call_freq),
            "{call} RX frequency"
        );
        assert!(p.metadata.fox, "{call} is a Fox-role QSO");
        assert!(
            coordinator
                .fox_admitted_qsos
                .read()
                .unwrap()
                .contains(&key(&id)),
            "{call} is Fox-admitted"
        );
    }
}

/// AC3: with no Fox slot 75 Hz clear of the ordinary streams, a calling Hound
/// is not answered; freeing one ordinary stream frees a slot for it.
#[tokio::test]
async fn pan386_no_free_fox_slot_answers_nobody() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    // Ordinary (not Fox-marked) streams at 300, 450, 600, 750 and 900 Hz
    // leave every Fox slot within 75 Hz of one of them.
    let mut ordinary = Vec::new();
    for (i, freq) in [300.0, 450.0, 600.0, 750.0, 900.0].into_iter().enumerate() {
        let id = manager
            .respond_to_caller(
                format!("N{i}ORD"),
                freq,
                Some(SlotParity::Odd),
                ResponseStep::Report,
                Some(-10.0),
                None,
                None,
                false,
                None,
            )
            .await
            .expect("ordinary stream");
        ordinary.push(id);
    }
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    coordinator
        .fox_max_streams
        .store(10, std::sync::atomic::Ordering::Relaxed);
    assert!(
        calling_cq(&manager).await.is_empty(),
        "no Fox slot is free, so no Fox CQ starts"
    );

    answer(&coordinator, &manager, "W9XYZ", 1240.0, 10).await;
    assert!(
        qso_for(&manager, "W9XYZ").await.is_none(),
        "no free Fox slot: the Hound must not be answered"
    );

    // Control: free the 600 Hz stream; 540 Hz is then the lowest free slot.
    manager.cancel_qso(ordinary[2]).await.unwrap();
    answer(&coordinator, &manager, "W9XYZ", 1240.0, 10).await;
    let (_, p) = qso_for(&manager, "W9XYZ")
        .await
        .expect("a freed slot lets the Hound be answered");
    assert_eq!(p.metadata.frequency, 540.0);
    assert!(p.metadata.fox);
}

/// AC4: outside Fox mode an auto-answer stays Tx=Rx.
#[tokio::test]
async fn pan386_auto_answer_outside_fox_mode_is_tx_eq_rx() {
    let (coordinator, manager, _tui_rx) = pan76_station().await;
    answer(&coordinator, &manager, "W2BBB", 1200.0, 1).await;
    let (_, p) = qso_for(&manager, "W2BBB").await.expect("answered");
    assert_eq!(p.metadata.frequency, 1200.0);
    assert_eq!(p.metadata.partner_freq, None);
    assert!(!p.metadata.fox);
}

/// AC4: an FT4 Fox keeps the legacy geometry: CQ at 1500 Hz, Tx=Rx answers,
/// no Fox-role QSO.
#[tokio::test]
async fn pan386_ft4_fox_mode_keeps_the_legacy_geometry() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    // The production mode switch path: the Qso task's own manager.
    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::SetOperatingMode { mode: "FT4".into() }),
            Instant::now(),
        ))
        .await
        .unwrap();
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cqs = calling_cq(&manager).await;
    assert_eq!(cqs.len(), 1);
    assert_eq!(cqs[0].1.metadata.frequency, 1500.0);
    assert!(!cqs[0].1.metadata.fox);

    // `maybe_answer_caller` reads the mode from the manager it is given; in
    // production that is the Qso task's own (FT4) manager.
    let mut ft4_manager = manager.clone();
    ft4_manager.set_active_mode("FT4".into());
    answer(&coordinator, &ft4_manager, "W2BBB", 1200.0, 1).await;
    let (_, p) = qso_for(&manager, "W2BBB").await.expect("answered");
    assert_eq!(p.metadata.frequency, 1200.0);
    assert_eq!(p.metadata.partner_freq, None);
    assert!(manager
        .get_active_qsos()
        .await
        .iter()
        .all(|(_, p)| !p.metadata.fox));
}

/// A WSJT-X Hound sends no 73 after the Fox's RR73, so a completed Fox-role
/// QSO is never stashed for the auto-73 resend.
#[tokio::test]
async fn pan386_fox_completion_is_not_stashed_for_auto_73() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "KH7Z".into(),
        our_grid: Some("BK29".into()),
        ..QsoManagerConfig::default()
    });
    let id = manager
        .respond_to_caller(
            "W9XYZ".into(),
            360.0,
            Some(SlotParity::Odd),
            ResponseStep::Report,
            Some(-8.0),
            None,
            Some(1240.0),
            false,
            None,
        )
        .await
        .unwrap();
    let mut metadata = manager.get_qso(id).await.unwrap().metadata;
    metadata.initiated_by = CallInitiation::Manual;
    assert!(
        stashes_auto_73(&metadata),
        "control: an ordinary manual completion is stashed"
    );
    assert!(manager.mark_fox_role(id).await);
    let mut fox_metadata = manager.get_qso(id).await.unwrap().metadata;
    fox_metadata.initiated_by = CallInitiation::Manual;
    assert!(fox_metadata.fox);
    assert!(!stashes_auto_73(&fox_metadata));
}
