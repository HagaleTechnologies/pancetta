//! Mid-QSO TX-frequency hold (operator request): we hold the QSO's
//! latched TX audio offset for the whole exchange so long as it is
//! "working" (the DX keeps advancing) — offset is otherwise irrelevant
//! to whether the DX hears us, since FT8 receivers decode the whole
//! passband. What happens once it *stops* working (the DX stalls) is
//! `stall_detection_tests`, not this module — this module
//! previously also covered the fixed +300Hz repeat-frame "stuck-DX hop"
//! escape mechanism, removed by PAN-72 in favor of the silence-driven
//! stall detector there.
use super::*;

const OUR: &str = "K5ARH";
const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
    })
}

async fn freq_of(manager: &QsoManager, id: QsoId) -> f64 {
    manager.get_qso(id).await.unwrap().metadata.frequency
}

async fn send_report(manager: &QsoManager, report: i8, text: &str) {
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report,
            },
            text.to_string(),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
}

/// A normal advancing exchange holds the TX frequency unchanged.
#[tokio::test]
async fn frequency_held_across_advancing_exchange() {
    let manager = manager();
    let id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    assert_eq!(freq_of(&manager, id).await, FREQ);

    // DX report (advance), then their R-report ack (advance), then 73.
    send_report(&manager, -7, &format!("{OUR} {DX} -07")).await;
    assert_eq!(freq_of(&manager, id).await, FREQ, "held after report");
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} R-07"),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    assert_eq!(freq_of(&manager, id).await, FREQ, "held through the QSO");
}

#[test]
fn effective_tx_dial_simplex_and_split() {
    // Simplex: split == 0 → use RX dial.
    assert_eq!(super::effective_tx_dial(14_074_000, 0), 14_074_000);
    // Split: nonzero split → use split TX dial.
    assert_eq!(super::effective_tx_dial(14_074_000, 14_090_000), 14_090_000);
}

#[test]
fn split_source_overrides_rx_dial_for_stamp() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let rx = std::sync::Arc::new(AtomicU64::new(14_074_000));
    let split = std::sync::Arc::new(AtomicU64::new(14_090_000));
    let dial = super::effective_tx_dial(rx.load(Ordering::Relaxed), split.load(Ordering::Relaxed));
    assert_eq!(dial, 14_090_000);
    split.store(0, Ordering::Relaxed);
    let dial = super::effective_tx_dial(rx.load(Ordering::Relaxed), split.load(Ordering::Relaxed));
    assert_eq!(dial, 14_074_000);
}
