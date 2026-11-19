use super::*;

#[test]
fn manual_override_rejects_old_summaries_until_reenabled() {
    let config = pancetta_config::Config::default();
    let drive = super::super::tx_drive::TxDrive::from_config(&config);
    let band = pancetta_core::Band::from_frequency(14_074_000);
    let old = drive.snapshot_for_band(band);
    let manual = drive.adjust_band(band, 1);
    assert_eq!(
        manual.mode,
        pancetta_core::tx_drive_mode::TxDriveMode::Manual
    );
    assert_eq!(manual.db, -5.0);
    let sample = pancetta_core::tx_meter::TxMeterReading {
        alc: Some(0.8),
        forward_power: None,
    };
    let context = super::super::tx_drive::DriveContext {
        rig_generation: 1,
        ft_mode: 0,
        stream_count: 1,
    };
    assert_eq!(
        drive.complete(old, &[sample, sample], context),
        super::super::tx_drive::DriveDecision::Stale
    );
    assert_eq!(drive.snapshot_for_band(band).db, -5.0);
    let renewed = drive.toggle_band(band);
    assert_eq!(
        renewed.mode,
        pancetta_core::tx_drive_mode::TxDriveMode::Auto
    );
    assert_eq!(
        drive.complete(renewed, &[sample, sample], context),
        super::super::tx_drive::DriveDecision::Overdrive(-7.0)
    );
}

#[test]
fn late_worker_report_cannot_replace_a_newer_readout() {
    use std::sync::atomic::AtomicU64;
    let config = pancetta_config::Config::default();
    let drive = super::super::tx_drive::TxDrive::from_config(&config);
    let band20 = pancetta_core::Band::from_frequency(14_074_000);
    let operating = AtomicU64::new(14_074_000);
    let split = AtomicU64::new(0);
    // F8 after the worker took its snapshot: the report must show Manual.
    let _ = drive.snapshot_for_band(band20);
    let manual = drive.adjust_band(band20, 1);
    match live_tx_drive_message(band20, &drive, &operating, &split) {
        Some(pancetta_tui::tui_runner::TuiMessage::TxDriveState { db, mode, .. }) => {
            assert_eq!(db, manual.db);
            assert_eq!(mode, pancetta_core::tx_drive_mode::TxDriveMode::Manual);
        }
        other => panic!("expected the live 20 m readout, got {other:?}"),
    }
    // The band changed to 40 m before the 20 m report arrived.
    operating.store(7_074_000, std::sync::atomic::Ordering::Release);
    assert!(live_tx_drive_message(band20, &drive, &operating, &split).is_none());
    // A split TX dial on 20 m makes 20 m the TX band again.
    split.store(14_074_000, std::sync::atomic::Ordering::Release);
    assert!(live_tx_drive_message(band20, &drive, &operating, &split).is_some());
    // Unknown dial: the station-wide fallback report still reaches the TUI.
    operating.store(0, std::sync::atomic::Ordering::Release);
    split.store(0, std::sync::atomic::Ordering::Release);
    assert!(live_tx_drive_message(None, &drive, &operating, &split).is_some());
    assert!(live_tx_drive_message(band20, &drive, &operating, &split).is_none());
}
