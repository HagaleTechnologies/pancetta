use super::*;

fn controller(db: f32) -> TxDrive {
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    config.audio.auto_drive.start_db = db;
    TxDrive::from_config(&config)
}

fn reading(alc: Option<f32>, power: Option<ForwardPower>) -> TxMeterReading {
    TxMeterReading {
        alc,
        forward_power: power,
    }
}

fn context() -> DriveContext {
    DriveContext {
        rig_generation: 1,
        ft_mode: 8,
        stream_count: 1,
    }
}

fn burst(drive: &TxDrive, samples: &[TxMeterReading]) -> DriveDecision {
    let snapshot = drive.snapshot_for_band(Some(Band::Band20m));
    drive.complete(snapshot, samples, context())
}

#[test]
fn holds_in_the_inclusive_alc_band() {
    for alc in [0.10, 0.30, 0.50] {
        let drive = controller(-2.0);
        assert_eq!(
            burst(&drive, &[reading(Some(alc), None)]),
            DriveDecision::Held
        );
        assert_eq!(drive.snapshot_for_band(Some(Band::Band20m)).db, -2.0);
    }
}

#[test]
fn clean_bursts_stop_at_ceiling() {
    let drive = controller(0.0);
    assert_eq!(
        burst(&drive, &[reading(Some(0.0), None)]),
        DriveDecision::Held
    );
    assert_eq!(drive.snapshot_for_band(Some(Band::Band20m)).db, 0.0);
}

#[test]
fn overdrive_at_floor_requests_rig_input_reduction() {
    let drive = controller(-20.0);
    let high = reading(Some(0.8), None);
    assert_eq!(burst(&drive, &[high, high]), DriveDecision::Floor);
    assert_eq!(drive.snapshot_for_band(Some(Band::Band20m)).db, -20.0);
}

#[test]
fn two_flat_power_steps_stop_the_climb() {
    let drive = controller(-4.0);
    let clean = |w| [reading(Some(0.02), Some(ForwardPower::Watts(w)))];
    assert_eq!(burst(&drive, &clean(10.0)), DriveDecision::Changed(-3.5));
    assert_eq!(burst(&drive, &clean(10.5)), DriveDecision::Changed(-3.0));
    assert_eq!(burst(&drive, &clean(10.8)), DriveDecision::Held);
    assert_eq!(drive.snapshot_for_band(Some(Band::Band20m)).db, -3.0);
}

#[test]
fn missing_alc_never_changes_drive_and_manual_invalidates_old_report() {
    let drive = controller(-3.0);
    let old = drive.snapshot_for_band(Some(Band::Band20m));
    assert_eq!(
        drive.complete(old, &[reading(None, None)], context()),
        DriveDecision::Unavailable
    );
    assert_eq!(drive.snapshot_for_band(Some(Band::Band20m)).db, -3.0);
    assert_eq!(drive.adjust_band(Some(Band::Band20m), 1).db, -2.0);
    assert_eq!(
        drive.complete(old, &[reading(Some(0.0), None)], context()),
        DriveDecision::Stale
    );
}

#[test]
fn missing_samples_do_not_break_high_run_but_a_valid_low_sample_does() {
    let drive = controller(-1.0);
    let high = reading(Some(0.8), None);
    assert_eq!(
        burst(&drive, &[high, reading(None, None), high]),
        DriveDecision::Overdrive(-3.0)
    );
    let drive = controller(-1.0);
    assert_eq!(
        burst(&drive, &[high, reading(Some(0.1), None), high]),
        DriveDecision::Held
    );
}

#[test]
fn incompatible_power_units_break_flat_streak() {
    let drive = controller(-4.0);
    let clean = |power| [reading(Some(0.02), Some(power))];
    assert_eq!(
        burst(&drive, &clean(ForwardPower::Watts(10.0))),
        DriveDecision::Changed(-3.5)
    );
    assert_eq!(
        burst(&drive, &clean(ForwardPower::FractionOfMax(0.1))),
        DriveDecision::Changed(-3.0)
    );
    assert_eq!(
        burst(&drive, &clean(ForwardPower::Watts(10.0))),
        DriveDecision::Changed(-2.5)
    );
}

#[test]
fn dirty_generation_tracks_first_valid_report_and_manual_changes() {
    let drive = controller(-2.0);
    assert!(!drive.needs_persist());
    let snapshot = drive.snapshot_for_band(Some(Band::Band20m));
    assert!(!drive.needs_persist());
    assert_eq!(
        drive.complete(snapshot, &[reading(None, None)], context()),
        DriveDecision::Unavailable
    );
    assert!(!drive.needs_persist());
    assert_eq!(
        burst(&drive, &[reading(Some(0.2), None)]),
        DriveDecision::Held
    );
    assert!(drive.needs_persist());
    let (_, generation) = drive.restored_map_with_generation();
    drive.mark_persisted(generation);
    assert!(!drive.needs_persist());
    assert_eq!(
        burst(&drive, &[reading(Some(0.2), None)]),
        DriveDecision::Held
    );
    assert!(!drive.needs_persist());
    drive.adjust_band(Some(Band::Band20m), 0);
    assert!(drive.needs_persist());
    // A stale writer may only acknowledge the generation it actually saved.
    drive.mark_persisted(generation);
    assert!(drive.needs_persist());
}
