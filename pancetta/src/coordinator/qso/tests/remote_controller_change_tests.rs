//! PAN-137: exercise the real QSO component and its deferred-call queue.
use super::super::ApplicationCoordinator;
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage};
use pancetta_config::Config;
use pancetta_core::slot::SlotParity;
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

async fn remote_call(h: &Harness, call: &str, freq: u64, dx: SlotParity, client: &str) {
    h.send(QsoMessage::StartQso {
        callsign: call.to_string(),
        frequency: freq,
        dx_parity: Some(dx),
        remote_origin: true,
        remote_client_key_id: Some(client.to_string()),
        dx_offset_unknown: false,
    })
    .await;
}

async fn change_controller(h: &Harness, client: &str) {
    h.send(QsoMessage::RemoteControllerChanged {
        controller_key_id: client.to_string(),
    })
    .await;
}

async fn open_a_and_queue_b(h: &Harness) {
    remote_call(h, "K1AAA", 1500, SlotParity::Even, "client-a").await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K1AAA"))).await;
    assert!(any_tx_containing(&tx, "K1AAA"));
    remote_call(h, "K2BBB", 2000, SlotParity::Odd, "client-b").await;
    let tui = collect_until(&h.tui, |m| {
        status(m).is_some_and(|s| s.starts_with("Queued K2BBB"))
    })
    .await;
    assert!(statuses(&tui).iter().any(|s| s.starts_with("Queued K2BBB")));
    assert!(!h.manager.has_active_qso_with("K2BBB").await);
}

#[tokio::test]
async fn controller_change_starts_the_new_controllers_queued_opposite_parity_call() {
    let h = harness(4, false).await;
    open_a_and_queue_b(&h).await;
    change_controller(&h, "client-b").await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K2BBB"))).await;
    let tui = collect_quiet(&h.tui, 300).await;
    assert!(any_tx_containing(&tx, "K2BBB"));
    assert!(!h.manager.has_active_qso_with("K1AAA").await);
    assert_eq!(h.manager.current_tx_side().await, Some(SlotParity::Even));
    assert!(
        statuses(&tui).contains(&"QSO with K1AAA failed: remote control passed to another client")
    );
    assert!(statuses(&tui).contains(&"Now calling K2BBB (was queued)"));
}

#[tokio::test]
async fn controller_change_drops_the_previous_controllers_queued_calls() {
    let h = harness(4, true).await;
    remote_call(&h, "K1AAA", 1500, SlotParity::Even, "client-a").await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K1AAA"))).await;
    assert!(any_tx_containing(&tx, "K1AAA"));
    for (call, client, freq) in [("K3CCC", "client-a", 1800), ("K2BBB", "client-b", 2000)] {
        remote_call(&h, call, freq, SlotParity::Odd, client).await;
        let queued = format!("Queued {call}");
        let tui = collect_until(&h.tui, |m| {
            status(m).is_some_and(|s| s.starts_with(&queued))
        })
        .await;
        assert!(statuses(&tui).iter().any(|s| s.starts_with(&queued)));
    }
    let _ = collect_quiet(&h.gateway, 100).await;
    change_controller(&h, "client-b").await;
    let mut tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K2BBB"))).await;
    tx.extend(collect_quiet(&h.tx, 500).await);
    assert!(any_tx_containing(&tx, "K2BBB"));
    assert!(!any_tx_containing(&tx, "K3CCC"));
    assert!(!h.manager.has_active_qso_with("K1AAA").await);
    assert!(!h.manager.has_active_qso_with("K3CCC").await);
    let gateway = collect_quiet(&h.gateway, 100).await;
    assert!(gateway.iter().any(|m| matches!(&m.message_type,
        MessageType::ActiveQsosSnapshot { pending, .. } if pending.iter().all(|p| p.callsign != "K3CCC")
    )), "queue purge must reach the display feed: {gateway:?}");
}

#[tokio::test]
async fn controller_change_leaves_local_and_same_controller_qsos_running() {
    let h = harness(4, false).await;
    h.start_qso("K4LOC", 1300).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K4LOC"))).await;
    assert!(any_tx_containing(&tx, "K4LOC"));
    remote_call(&h, "K1AAA", 1500, SlotParity::Even, "client-a").await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K1AAA"))).await;
    assert!(any_tx_containing(&tx, "K1AAA"));
    let _ = collect_quiet(&h.tui, 100).await;
    change_controller(&h, "client-a").await;
    let tui = collect_quiet(&h.tui, 300).await;
    assert!(!statuses(&tui).iter().any(|s| s.contains("failed:")));
    assert!(h.manager.has_active_qso_with("K4LOC").await);
    assert!(h.manager.has_active_qso_with("K1AAA").await);
    change_controller(&h, "client-b").await;
    let tui = collect_until(&h.tui, |m| {
        status(m).is_some_and(|s| s.starts_with("QSO with K1AAA failed:"))
    })
    .await;
    assert!(statuses(&tui)
        .iter()
        .any(|s| s.starts_with("QSO with K1AAA failed:")));
    assert!(h.manager.has_active_qso_with("K4LOC").await);
    assert!(!h.manager.has_active_qso_with("K1AAA").await);
}

#[tokio::test]
async fn without_a_controller_change_the_opposite_parity_call_stays_queued() {
    let h = harness(4, false).await;
    open_a_and_queue_b(&h).await;
    let tx = collect_quiet(&h.tx, 1000).await;
    assert!(!any_tx_containing(&tx, "K2BBB"));
    assert!(h.manager.has_active_qso_with("K1AAA").await);
    assert!(!h.manager.has_active_qso_with("K2BBB").await);
}

#[tokio::test]
async fn controller_change_refreshes_snapshot_when_only_queued_calls_are_dropped() {
    let h = harness(4, true).await;
    h.start_qso("K4LOC", 1300).await;
    let tx = collect_until(&h.tx, |m| tx_text(m).is_some_and(|t| t.contains("K4LOC"))).await;
    assert!(any_tx_containing(&tx, "K4LOC"));
    remote_call(&h, "K3CCC", 1800, SlotParity::Odd, "client-a").await;
    let tui = collect_until(&h.tui, |m| {
        status(m).is_some_and(|s| s.starts_with("Queued K3CCC"))
    })
    .await;
    assert!(statuses(&tui).iter().any(|s| s.starts_with("Queued K3CCC")));
    let _ = collect_quiet(&h.gateway, 100).await;
    let _ = collect_quiet(&h.tui, 100).await;
    change_controller(&h, "client-b").await;
    for rx in [&h.tui, &h.gateway] {
        let messages = collect_until(rx, |m| {
            matches!(&m.message_type,
                MessageType::ActiveQsosSnapshot { pending, .. } if pending.is_empty()
            )
        })
        .await;
        assert!(messages.iter().any(|m| matches!(&m.message_type,
            MessageType::ActiveQsosSnapshot { qsos, pending } if pending.is_empty() && qsos.len() == 1
        )), "queue-only purge must push a snapshot: {messages:?}");
    }
    assert!(h.manager.has_active_qso_with("K4LOC").await);
    assert!(!h.manager.has_active_qso_with("K3CCC").await);
}

/// PAN-250: the station agent forwards a remote `stopCq` as
/// `QsoMessage::StopCq`. By the time the gateway sees the snapshot without
/// the CQ, the CQ has left TX liveness, so the TX worker drops its next frame.
#[tokio::test]
async fn remote_stop_cq_cancels_the_cq_before_its_next_tx() {
    let h = harness(4, true).await;
    h.send(QsoMessage::StartCq {
        frequency: 1200,
        tx_parity: None,
        remote_origin: true,
        remote_client_key_id: Some("client-a".to_string()),
    })
    .await;
    let tx = collect_until(&h.tx, |m| {
        tx_text(m).is_some_and(|t| t.starts_with("CQ K1TEST"))
    })
    .await;
    assert!(tx
        .iter()
        .filter_map(tx_text)
        .any(|t| t.starts_with("CQ K1TEST")));
    let active = h.manager.get_active_qsos().await;
    assert_eq!(active.len(), 1, "exactly the remote CQ is active");
    let id = active[0].0.to_string();
    assert!(super::super::tx_qso_is_live(
        Some(&id),
        &h.coordinator.active_tx_qsos.read().unwrap()
    ));
    let _ = collect_quiet(&h.gateway, 100).await;
    h.send(QsoMessage::StopCq).await;
    let gateway = collect_until(&h.gateway, |m| {
        matches!(&m.message_type, MessageType::ActiveQsosSnapshot { qsos, .. } if qsos.is_empty())
    })
    .await;
    assert!(
        gateway.iter().any(|m| matches!(&m.message_type,
            MessageType::ActiveQsosSnapshot { qsos, .. } if qsos.is_empty()
        )),
        "the stop must reach the display feed: {gateway:?}"
    );
    assert!(!super::super::tx_qso_is_live(
        Some(&id),
        &h.coordinator.active_tx_qsos.read().unwrap()
    ));
    assert_eq!(h.manager.active_qso_count().await, 0);
    let tx = collect_quiet(&h.tx, 500).await;
    assert!(!any_tx_containing(&tx, "CQ K1TEST"));
}
