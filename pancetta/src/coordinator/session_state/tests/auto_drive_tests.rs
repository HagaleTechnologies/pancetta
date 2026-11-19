use super::*;
use pancetta_core::TxDriveMode;

#[test]
fn legacy_session_restores_and_band_entries_round_trip() {
    let (dir, path) = tests::session_file(
        r#"{"tx_freq_mode":null,"decode_effort":null,"decode_effort_config_at_save":null}"#,
    );
    assert!(matches!(load(&path), SessionLoad::Record(_)));
    let mut record = SessionRecord::default();
    record.tx_drive_by_band.insert(
        "20m".into(),
        super::super::tx_drive::SavedTxDrive {
            db: -3.0,
            mode: TxDriveMode::Auto,
        },
    );
    record.tx_drive_by_band.insert(
        "40m".into(),
        super::super::tx_drive::SavedTxDrive {
            db: -6.0,
            mode: TxDriveMode::Manual,
        },
    );
    save(&path, &record).unwrap();
    assert_eq!(load(&path), SessionLoad::Record(record));
    drop(dir);
}

#[test]
fn bad_drive_entry_does_not_discard_other_choices() {
    let (_dir, path) = tests::session_file(
        r#"{"tx_freq_mode":"AUTO","tx_drive_by_band":{"20m":{"db":-3.0,"mode":"auto"},"40m":{"db":-6.0,"mode":"bogus"},"nope":{"db":-2.0,"mode":"auto"}}}"#,
    );
    let SessionLoad::Record(record) = load(&path) else {
        panic!("expected valid record")
    };
    assert_eq!(record.tx_freq_mode, Some(TxFreqMode::Auto));
    assert_eq!(record.tx_drive_by_band.len(), 1);
    assert_eq!(record.tx_drive_by_band["20m"].db, -3.0);
}

#[test]
fn failed_save_retries_without_overwriting_newer_choices() {
    let (_dir, path) = tests::session_file("");
    let shared = tests::shared_record(SessionRecord::default());
    let drive = super::super::tx_drive::TxDrive::from_config(&pancetta_config::Config::default());
    let _ = drive.snapshot_for_band(Some(pancetta_core::Band::Band20m));
    drive.adjust_band(Some(pancetta_core::Band::Band20m), 1);
    let blocked_parent = path.parent().unwrap().join("blocked");
    std::fs::write(&blocked_parent, "file, not a directory").unwrap();
    let blocked_path = blocked_parent.join("session_state.json");
    assert!(persist_tx_drive_current(&shared, &Some(blocked_path), &drive).is_err());
    assert!(drive.needs_persist());
    drive.adjust_band(Some(pancetta_core::Band::Band20m), 1);
    // A later ordinary choice rewrites the current record, including the
    // drive map that stayed live after the first write failed.
    persist_session_choice(
        &shared,
        &Some(path.clone()),
        SessionChoiceUpdate::TxFreqMode(TxFreqMode::Auto),
    );
    persist_tx_drive_current(&shared, &Some(path.clone()), &drive).unwrap();
    assert!(!drive.needs_persist());
    assert_eq!(
        tests::read_record(&path).tx_drive_by_band,
        drive.restored_map_with_generation().0
    );
    assert_eq!(
        tests::read_record(&path).tx_freq_mode,
        Some(TxFreqMode::Auto)
    );
}
