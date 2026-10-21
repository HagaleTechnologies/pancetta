//! PAN-75: exercise real QSO accounting against the production TX coalescer.
use chrono::{DateTime, Duration, Utc};
use pancetta_core::{slot::SlotParity, TxFreqMode};
use pancetta_lib::coordinator::{coalesce_transmit_requests, CoalesceEntry};
use pancetta_lib::message_bus::TxOrigin;
use pancetta_qso::{MessageDirection, QsoEvent, QsoId, QsoManager, QsoManagerConfig};
use std::sync::{atomic::AtomicU8, Arc};
use tokio::sync::broadcast;

fn manager(max_calls: u32) -> QsoManager {
    let mut config = QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        our_grid: Some("FN42".into()),
        ..Default::default()
    };
    config.timeouts.manual_call_max_calls = max_calls;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    QsoManager::new(config)
}

async fn open(manager: &QsoManager, call: &str, freq: f64) -> QsoId {
    manager
        .respond_to_cq_manual(call.into(), freq, Some(SlotParity::Even))
        .await
        .unwrap()
}

async fn cycle(
    manager: &QsoManager,
    rx: &mut broadcast::Receiver<QsoEvent>,
    excluded: QsoId,
    cap: u32,
    now: DateTime<Utc>,
) -> (Vec<QsoId>, usize, Vec<QsoId>) {
    let mut entries = Vec::new();
    let mut actions = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                ..
            } => entries.push(CoalesceEntry {
                message_text: pancetta_qso::utils::generate_ft8_message(&message, "W1ABC").unwrap(),
                frequency_offset: frequency,
                qso_id: Some(qso_id.to_string()),
                tx_parity,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            }),
            QsoEvent::TxOffsetActionNeeded { qso_id, .. } => actions.push(qso_id),
            _ => {}
        }
    }
    let excluded_id = excluded.to_string();
    let emissions = entries
        .iter()
        .filter(|e| e.qso_id.as_deref() == Some(&excluded_id))
        .count();
    entries.sort_by_key(|e| e.qso_id.as_deref() == Some(&excluded_id));
    let outcome = coalesce_transmit_requests(entries, cap, |_| true);
    for entry in &outcome.truncated_entries {
        manager
            .roll_back_unsent_attempt_at(
                entry.qso_id.as_ref().unwrap().parse().unwrap(),
                &entry.message_text,
                now,
            )
            .await;
    }
    (
        outcome
            .retained
            .iter()
            .map(|e| e.qso_id.as_ref().unwrap().parse().unwrap())
            .collect(),
        emissions,
        actions,
    )
}

#[tokio::test]
async fn cap_excluded_manual_qso_keeps_its_call_budget() {
    let manager = manager(4);
    let mut rx = manager.subscribe();
    let a = open(&manager, "K1AAA", 1000.0).await;
    let b = open(&manager, "K2BBB", 1600.0).await;
    let c = open(&manager, "K3CCC", 2200.0).await;
    let opened = manager
        .get_qso(c)
        .await
        .unwrap()
        .metadata
        .first_call_at
        .unwrap();
    for slot in 0..4 {
        let t = opened + Duration::seconds(15 * slot);
        let (aired, emissions, _) = cycle(&manager, &mut rx, c, 2, t).await;
        assert_eq!(emissions, 1);
        assert!(aired.contains(&a) && aired.contains(&b));
        assert!(!aired.contains(&c));
        let qso = manager.get_qso(c).await.unwrap();
        assert!(qso.state.is_active());
        assert_eq!(qso.metadata.call_count, 1);
        assert_eq!(qso.metadata.last_call_at, qso.metadata.first_call_at);
        assert_eq!(
            qso.messages
                .iter()
                .filter(|m| m.direction == MessageDirection::Sent)
                .count(),
            1
        );
        manager.check_timeouts_at(t).await;
        manager
            .rearm_manual_calls_at(t + Duration::seconds(15))
            .await;
    }
    // A and B reached the cap at slot 3; the cap retires them once that
    // frame is past the rollback window. Rearm stops at the cap, so they
    // emit nothing meanwhile.
    manager
        .check_timeouts_at(opened + Duration::seconds(15 * 3 + 75))
        .await;
    assert!(manager.get_qso(a).await.is_err());
    assert!(manager.get_qso(b).await.is_err());
    for aired_rearms in 1..=3 {
        let t = opened + Duration::seconds(15 * (3 + aired_rearms));
        let (aired, emissions, _) = cycle(&manager, &mut rx, c, 2, t).await;
        assert_eq!(aired, vec![c]);
        assert_eq!(emissions, 1);
        assert_eq!(
            manager.get_qso(c).await.unwrap().metadata.call_count,
            1 + aired_rearms as u32
        );
        manager.check_timeouts_at(t).await;
        manager
            .rearm_manual_calls_at(t + Duration::seconds(15))
            .await;
    }
    manager
        .check_timeouts_at(opened + Duration::seconds(15 * 6 + 75))
        .await;
    assert!(manager.get_qso(c).await.is_err());
}

#[tokio::test]
async fn never_aired_qso_does_not_trip_the_stall_offset_switch() {
    let mut manager = manager(25);
    manager.set_tx_freq_mode_source(Arc::new(AtomicU8::new(TxFreqMode::Auto.as_u8())));
    let mut rx = manager.subscribe();
    let a = open(&manager, "K1AAA", 1000.0).await;
    let b = open(&manager, "K2BBB", 1600.0).await;
    let opened = manager
        .get_qso(b)
        .await
        .unwrap()
        .metadata
        .first_call_at
        .unwrap();
    let mut a_switched = false;
    // PAN-128: A's aired rearms step two slots (30 s) apart so each feeds a
    // distinct own-parity TX slot — the stall counter counts own TX slots,
    // not protocol slots — and the threshold trip is phase-independent.
    for slot in 0..=6 {
        let t = opened + Duration::seconds(slot * 30);
        let (aired, _, actions) = cycle(&manager, &mut rx, b, 1, t).await;
        assert!(!aired.contains(&b));
        assert!(!actions.contains(&b));
        a_switched |= actions.contains(&a);
        assert_eq!(manager.get_qso(b).await.unwrap().metadata.stall_cycles, 0);
        if slot < 6 {
            manager
                .rearm_manual_calls_at(t + Duration::seconds(30))
                .await;
        }
    }
    assert!(a_switched);
}

#[tokio::test]
async fn truncated_auto_resend_is_not_spent() {
    let manager = manager(25);
    let mut rx = manager.subscribe();
    open(&manager, "K1AAA", 1000.0).await;
    let c = manager
        .respond_to_cq("K3CCC".into(), 2200.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let opened = manager
        .get_qso(c)
        .await
        .unwrap()
        .metadata
        .first_call_at
        .unwrap();
    assert!(cycle(&manager, &mut rx, c, 2, opened).await.0.contains(&c));
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(15))
        .await;
    assert!(
        !cycle(&manager, &mut rx, c, 1, opened + Duration::seconds(15))
            .await
            .0
            .contains(&c)
    );
    assert_eq!(manager.get_qso(c).await.unwrap().metadata.call_count, 1);
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    let (aired, emissions, _) =
        cycle(&manager, &mut rx, c, 2, opened + Duration::seconds(30)).await;
    assert_eq!(emissions, 1);
    assert!(aired.contains(&c));
}
