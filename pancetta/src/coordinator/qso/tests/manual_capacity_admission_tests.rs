//! PAN-78: `max_concurrent_qsos` is enforced when a MANUAL QSO is
//! admitted (`StartQso`, `RespondToCaller`, `EngageHound`, `StartCq`),
//! not only by the TX worker's coalescer — which only sees the
//! `TransmitRequest`s drained in one cycle, so a QSO that has
//! transmitted and is now idle (waiting for a reply) is invisible to it.
//!
//! Exercises the REAL `start_qso_component` message loop over the real
//! message bus. `test_coordinator` / the drain helpers are local copies of
//! `respond_to_caller_admission_tests`' (no shared helper exists); see
//! that module's doc for why `--replay` is set (never touch the real
//! logbook).
use super::super::ApplicationCoordinator;
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage};
use pancetta_config::Config;
use pancetta_core::slot::SlotParity;
use pancetta_core::{DiagnosticLevel, ResponseStep};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Rx = crossbeam_channel::Receiver<ComponentMessage>;

async fn test_coordinator() -> ApplicationCoordinator {
    let config = Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true,                                                // no_audio
        true,                                                // headless
        None,                                                // no WAV
        Some(std::path::PathBuf::from("/some/capture/dir")), // --replay: never touch the real logbook
        None,                                                // no test-tx
        1500.0,
        shutdown,
        Vec::new(),                                             // no config warnings
        std::env::temp_dir().join("pancetta-test-config.toml"), // test-only config path
        true,                                                   // test-only: assume TOML
        None,                                                   // PAN-270: no session-home override
    )
    .await
    .expect("coordinator creation should succeed");

    coordinator.pancetta_home_override = Some(std::env::temp_dir().join(format!(
        "pancetta-test-home-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));

    coordinator
}

/// A started QSO component with `max_concurrent_qsos = cap`, plus the
/// Tui, Ft8Transmitter and RemoteGateway receivers.
struct Harness {
    coordinator: ApplicationCoordinator,
    manager: pancetta_qso::QsoManager,
    tui: Rx,
    tx: Rx,
    gateway: Rx,
    // Keep the senders alive so the channels stay registered.
    _senders: Vec<crossbeam_channel::Sender<ComponentMessage>>,
}

async fn harness(cap: u32, display_feed: bool) -> Harness {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "K1TEST".to_string();
        cfg.station.grid_square = "FN42".to_string();
        cfg.autonomous.max_concurrent_qsos = cap;
    }
    coordinator
        .display_feed_enabled
        .store(display_feed, Ordering::Relaxed);
    let bus = &coordinator.message_bus;
    let (tui_s, tui) = bus.create_channel(ComponentId::Tui).await.unwrap();
    let (tx_s, tx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (gw_s, gateway) = bus
        .create_channel(ComponentId::RemoteGateway)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    Harness {
        coordinator,
        manager,
        tui,
        tx,
        gateway,
        _senders: vec![tui_s, tx_s, gw_s],
    }
}

impl Harness {
    async fn send(&self, msg: QsoMessage) {
        self.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Qso,
                MessageType::QsoMessage(msg),
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    async fn start_qso(&self, call: &str, frequency: u64) {
        self.send(QsoMessage::StartQso {
            callsign: call.to_string(),
            frequency,
            dx_parity: Some(SlotParity::Even),
            remote_origin: false,
            remote_client_key_id: None,
            dx_offset_unknown: false,
        })
        .await;
    }

    /// Open QSO A and wait until its opening `TransmitRequest` has been
    /// drained — A has transmitted its cycle and is now idle.
    async fn open_idle_qso_a(&self) {
        self.start_qso("K1AAA", 1500).await;
        let tx = collect_until(&self.tx, |m| {
            tx_text(m).is_some_and(|t| t.contains("K1AAA"))
        })
        .await;
        let a_requests: Vec<_> = tx.iter().filter_map(tx_text).collect();
        assert_eq!(
            a_requests.iter().filter(|t| t.contains("K1AAA")).count(),
            1,
            "QSO A must emit exactly one opening TransmitRequest: {a_requests:?}"
        );
        assert_eq!(self.manager.active_qso_count().await, 1);
        // Settle A's own Tui traffic (status, snapshot pushes).
        let _ = collect_quiet(&self.tui, 300).await;
    }
}

/// Collect EVERY message until one satisfies `pred` (or ~5 s pass), then
/// keep collecting until the channel has been quiet for 100 ms.
async fn collect_until(rx: &Rx, pred: impl Fn(&ComponentMessage) -> bool) -> Vec<ComponentMessage> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(m) => {
                let hit = pred(&m);
                out.push(m);
                if hit {
                    out.extend(collect_quiet(rx, 100).await);
                    return out;
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    out
}

/// Collect every message until the channel has been quiet for `quiet_ms`.
async fn collect_quiet(rx: &Rx, quiet_ms: u64) -> Vec<ComponentMessage> {
    let mut out = Vec::new();
    let mut idle_since = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(m) => {
                out.push(m);
                idle_since = Instant::now();
            }
            Err(_) => {
                if idle_since.elapsed() >= Duration::from_millis(quiet_ms) {
                    return out;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

fn tx_text(m: &ComponentMessage) -> Option<&str> {
    match &m.message_type {
        MessageType::TransmitRequest { message_text, .. } => Some(message_text.as_str()),
        _ => None,
    }
}

fn status(m: &ComponentMessage) -> Option<&str> {
    match &m.message_type {
        MessageType::StatusUpdate { text: s, .. } => Some(s.as_str()),
        _ => None,
    }
}

fn statuses(msgs: &[ComponentMessage]) -> Vec<&str> {
    msgs.iter().filter_map(status).collect()
}

fn any_tx_containing(msgs: &[ComponentMessage], needle: &str) -> bool {
    msgs.iter().filter_map(tx_text).any(|t| t.contains(needle))
}

fn has_capacity_diagnostic(msgs: &[ComponentMessage], call: Option<&str>) -> bool {
    msgs.iter().any(|m| {
        matches!(
            &m.message_type,
            MessageType::DiagnosticEvent { target, level, text, callsign, .. }
                if *target == "qso"
                    && *level == DiagnosticLevel::Warn
                    && callsign.as_deref() == call
                    && text.contains("at capacity 1/1")
        )
    })
}

fn refused_status(open: &str) -> String {
    format!("Not {open} — 1/1 QSOs active (max_concurrent_qsos); finish or abort one first")
}

/// The ticket's regression test (PAN-78 acceptance criterion): QSO A has
/// transmitted its cycle and is idle (no pending `TransmitRequest`); with
/// `max_concurrent_qsos = 1`, manually starting QSO B must not transmit
/// while A remains active. On `14ae9f1` B was admitted (2 active) and its
/// `TransmitRequest` reached the TX worker.
#[tokio::test]
async fn staggered_manual_start_is_refused_while_idle_qso_is_active() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    h.start_qso("K2BBB", 2000).await;
    let expected = refused_status("calling K2BBB");
    let tui = collect_until(&h.tui, |m| status(m) == Some(expected.as_str())).await;
    let tx = collect_quiet(&h.tx, 300).await;

    assert!(
        !any_tx_containing(&tx, "K2BBB"),
        "B must never reach the TX worker while A is active: {tx:?}"
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
    assert!(!h.manager.has_active_qso_with("K2BBB").await);
    assert!(
        statuses(&tui).contains(&expected.as_str()),
        "expected the refusal status line; observed: {:?}",
        statuses(&tui)
    );
    assert!(
        has_capacity_diagnostic(&tui, Some("K2BBB")),
        "expected a Warn `qso` capacity diagnostic for K2BBB; observed: {tui:?}"
    );
}

#[tokio::test]
async fn manual_start_admitted_when_below_cap() {
    let h = harness(2, false).await;
    h.open_idle_qso_a().await;

    h.start_qso("K2BBB", 2000).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K2BBB"))).await;
    assert!(
        any_tx_containing(&tx, "K2BBB"),
        "B must transmit below the cap: {tx:?}"
    );
    assert_eq!(h.manager.active_qso_count().await, 2);
    let tui = collect_until(&h.tui, |m| {
        status(m).is_some_and(|s| s.starts_with("Calling K2BBB"))
    })
    .await;
    assert!(
        statuses(&tui).contains(&"Calling K2BBB — TX queued (2000 Hz)"),
        "observed: {:?}",
        statuses(&tui)
    );
    assert!(!statuses(&tui).iter().any(|s| s.starts_with("Not calling")));
}

#[tokio::test]
async fn recall_of_active_station_is_not_refused_at_capacity() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    // FIX 1 idempotent keep-call: resolves to A, adds nothing.
    h.start_qso("K1AAA", 1500).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K1AAA"))).await;
    assert!(
        any_tx_containing(&tx, "K1AAA"),
        "the re-call must re-send A's current frame: {tx:?}"
    );
    let tui = collect_quiet(&h.tui, 300).await;
    assert!(
        !statuses(&tui).iter().any(|s| s.starts_with("Not calling")),
        "a re-call adds no QSO and must not be refused: {:?}",
        statuses(&tui)
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn capacity_frees_when_active_qso_ends() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    let (a_id, _) = h.manager.get_active_qsos().await.remove(0);
    h.send(QsoMessage::AbortQso {
        qso_id: a_id.to_string(),
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.manager.active_qso_count().await > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.manager.active_qso_count().await, 0, "A must be aborted");

    h.start_qso("K2BBB", 2000).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K2BBB"))).await;
    assert!(
        any_tx_containing(&tx, "K2BBB"),
        "with A gone a slot is free and B must start: {tx:?}"
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn respond_to_caller_refused_at_capacity_except_seventy_three() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    h.send(QsoMessage::RespondToCaller {
        callsign: "K3CCC".to_string(),
        frequency: 1800,
        dx_parity: Some(SlotParity::Even),
        step: ResponseStep::Report,
        snr: Some(-10.0),
        remote_origin: false,
        remote_client_key_id: None,
    })
    .await;
    let expected = refused_status("replying to K3CCC");
    let tui = collect_until(&h.tui, |m| status(m) == Some(expected.as_str())).await;
    let tx = collect_quiet(&h.tx, 300).await;
    assert!(
        statuses(&tui).contains(&expected.as_str()),
        "observed: {:?}",
        statuses(&tui)
    );
    assert!(!any_tx_containing(&tx, "K3CCC"), "{tx:?}");
    assert_eq!(h.manager.active_qso_count().await, 1);

    // `SeventyThree` opens directly at Completed — adds no active QSO.
    h.send(QsoMessage::RespondToCaller {
        callsign: "K4DDD".to_string(),
        frequency: 2200,
        dx_parity: Some(SlotParity::Even),
        step: ResponseStep::SeventyThree,
        snr: Some(-10.0),
        remote_origin: false,
        remote_client_key_id: None,
    })
    .await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K4DDD"))).await;
    assert!(
        any_tx_containing(&tx, "K4DDD"),
        "send 73 must not be refused at capacity: {tx:?}"
    );
    let tui = collect_quiet(&h.tui, 300).await;
    assert!(
        !statuses(&tui).iter().any(|s| s.starts_with("Not replying")),
        "{:?}",
        statuses(&tui)
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn start_cq_refused_at_capacity() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    h.send(QsoMessage::StartCq {
        frequency: 1200,
        tx_parity: None,
        remote_origin: false,
        remote_client_key_id: None,
    })
    .await;
    let expected = refused_status("calling CQ");
    let tui = collect_until(&h.tui, |m| status(m) == Some(expected.as_str())).await;
    let tx = collect_quiet(&h.tx, 300).await;
    assert!(
        statuses(&tui).contains(&expected.as_str()),
        "observed: {:?}",
        statuses(&tui)
    );
    assert!(has_capacity_diagnostic(&tui, None), "{tui:?}");
    assert!(
        !tx.iter()
            .filter_map(tx_text)
            .any(|t| t.starts_with("CQ K1TEST")),
        "{tx:?}"
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn engage_hound_refused_at_capacity() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    // Same desired parity as A (DX Even ⇒ we TX Odd): passes the parity
    // gate and reaches the capacity gate.
    h.send(QsoMessage::EngageHound {
        callsign: "K5FOX".to_string(),
        fox_freq: 600,
        dx_parity: Some(SlotParity::Even),
        fox_grid: None,
    })
    .await;
    let expected = refused_status("calling Fox K5FOX");
    let tui = collect_until(&h.tui, |m| status(m) == Some(expected.as_str())).await;
    let tx = collect_quiet(&h.tx, 300).await;
    assert!(
        statuses(&tui).contains(&expected.as_str()),
        "observed: {:?}",
        statuses(&tui)
    );
    assert!(!any_tx_containing(&tx, "K5FOX"), "{tx:?}");
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn pan302_queued_open_rechecks_permission_after_admission_wait() {
    let mut h = harness(2, false).await;
    let mut saved = h.coordinator.config.read().await.clone();
    saved.autonomous.enabled = true;
    h.coordinator
        .handle_config_reload(Ok((saved.clone(), vec![])))
        .await;
    let qso_sender = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Qso)
        .await
        .unwrap()
        .0;
    let gate = h.coordinator.qso_admission_for_test.clone().unwrap();
    let held = gate.lock().await;
    h.send(QsoMessage::StartAutonomousQso {
        callsign: Some("K3CCC".into()),
        frequency: 1800.0,
        parity: Some(SlotParity::Even),
        cq_attempt_id: None,
    })
    .await;
    // On this current-thread runtime, taking the opening runs the task
    // through its permission check to the held admission await.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !qso_sender.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    saved.autonomous.enabled = false;
    h.coordinator
        .handle_config_reload(Ok((saved, vec![])))
        .await;
    drop(held);
    let tx = collect_quiet(&h.tx, 300).await;
    assert!(!any_tx_containing(&tx, "K3CCC"));
    assert_eq!(h.manager.active_qso_count().await, 0);
}

/// Codex P1 on #449: an autonomous open queued before a manual open
/// filled the last slot is re-checked against the cap when handled.
#[tokio::test]
async fn autonomous_open_is_rechecked_against_the_cap() {
    let h = harness(1, false).await;
    h.coordinator
        .autonomous_config_enabled
        .store(true, Ordering::Release);
    h.open_idle_qso_a().await;

    // Same parity as A (DX Even ⇒ we TX Odd), so only capacity can refuse.
    h.send(QsoMessage::StartAutonomousQso {
        callsign: Some("K3CCC".to_string()),
        frequency: 1800.0,
        parity: Some(SlotParity::Even),
        cq_attempt_id: None,
    })
    .await;
    let tx = collect_quiet(&h.tx, 500).await;
    assert!(!any_tx_containing(&tx, "K3CCC"), "{tx:?}");
    assert_eq!(h.manager.active_qso_count().await, 1);
    assert!(!h.manager.has_active_qso_with("K3CCC").await);
}

fn engage_k5fox() -> QsoMessage {
    QsoMessage::EngageHound {
        callsign: "K5FOX".to_string(),
        fox_freq: 600,
        dx_parity: Some(SlotParity::Even),
        fox_grid: None,
    }
}

/// Validation finding on PAN-78: at the cap, pressing Shift+H again on a
/// Fox already being worked was refused ("Not calling Fox K5FOX — 1/1 QSOs
/// active"), because the EngageHound gate assumed `engage_hound` always
/// opens a new QSO. It is a manual `respond_to_cq_with`, so the repeat
/// press continues the existing Hound QSO and adds nothing.
#[tokio::test]
async fn repeat_engage_of_the_fox_being_worked_is_not_refused_at_capacity() {
    let h = harness(1, false).await;
    h.send(engage_k5fox()).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K5FOX"))).await;
    assert!(any_tx_containing(&tx, "K5FOX"), "{tx:?}");
    assert_eq!(h.manager.active_qso_count().await, 1);
    let (first_id, _) = h.manager.get_active_qsos().await.remove(0);
    let _ = collect_quiet(&h.tui, 300).await;

    // At capacity (1/1) with the Hound QSO itself; press Shift+H again.
    assert!(!h.manager.engage_hound_adds_active_qso("K5FOX").await);
    h.send(engage_k5fox()).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K5FOX"))).await;
    let tui = collect_quiet(&h.tui, 300).await;
    assert!(
        !statuses(&tui).iter().any(|s| s.starts_with("Not calling")),
        "a repeat engage adds no QSO and must not be refused: {:?}",
        statuses(&tui)
    );
    assert!(!has_capacity_diagnostic(&tui, Some("K5FOX")), "{tui:?}");
    assert!(
        any_tx_containing(&tx, "K5FOX"),
        "the repeat engage re-sends the Hound call: {tx:?}"
    );
    let active = h.manager.get_active_qsos().await;
    assert_eq!(active.len(), 1, "still exactly one active QSO");
    assert_eq!(active[0].0, first_id, "the existing Hound QSO is continued");

    // A different Fox at the cap is still refused.
    assert!(h.manager.engage_hound_adds_active_qso("K6FOX").await);
}

fn remote_start_k2bbb() -> QsoMessage {
    QsoMessage::StartQso {
        callsign: "K2BBB".to_string(),
        frequency: 2000,
        dx_parity: Some(SlotParity::Even),
        remote_origin: true,
        remote_client_key_id: Some("peer".to_string()),
        dx_offset_unknown: false,
    }
}

fn tx_denied_reasons(msgs: &[ComponentMessage]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match &m.message_type {
            MessageType::TxDenied { reason, qso_id } => {
                assert!(qso_id.is_none(), "no QSO exists to attribute it to");
                Some(reason.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn remote_start_qso_at_capacity_relays_tx_denied() {
    let h = harness(1, true).await;
    h.open_idle_qso_a().await;
    let _ = collect_quiet(&h.gateway, 100).await;

    h.send(remote_start_k2bbb()).await;
    let gw = collect_until(&h.gateway, |m| {
        matches!(m.message_type, MessageType::TxDenied { .. })
    })
    .await;
    let reasons = tx_denied_reasons(&gw);
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("call to K2BBB refused — at capacity 1/1")),
        "expected a relayed TxDenied; observed: {reasons:?}"
    );
    assert_eq!(h.manager.active_qso_count().await, 1);
}

#[tokio::test]
async fn remote_refusal_not_relayed_when_display_feed_disabled() {
    let h = harness(1, false).await;
    h.open_idle_qso_a().await;

    h.send(remote_start_k2bbb()).await;
    let expected = refused_status("calling K2BBB");
    let tui = collect_until(&h.tui, |m| status(m) == Some(expected.as_str())).await;
    let gw = collect_quiet(&h.gateway, 300).await;
    assert!(
        tx_denied_reasons(&gw).is_empty(),
        "additive-only: nothing is relayed while the display feed is off: {gw:?}"
    );
    assert!(
        statuses(&tui).contains(&expected.as_str()),
        "the local status line is unchanged; observed: {:?}",
        statuses(&tui)
    );
    assert!(has_capacity_diagnostic(&tui, Some("K2BBB")), "{tui:?}");
    assert_eq!(h.manager.active_qso_count().await, 1);
}
