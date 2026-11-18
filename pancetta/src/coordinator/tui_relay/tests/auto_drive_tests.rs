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
