use super::*;

#[tokio::test]
async fn a_read_begun_before_playback_does_not_train() {
    let ib = inboxes().await;
    let rig = MeterRig::new(Levels::Fail).await;
    rig.script(TxMeter::Alc, vec![Ok(0.2)]);
    rig.script(TxMeter::ForwardPowerWatts, vec![Ok(40.0)]);
    let attempt = TxDriveAttempt::new(true, None);
    attempt.confirm();
    *ib.bus.active_tx_drive_attempt.lock().unwrap() = Some(attempt.clone());
    // Playback starts while the ALC read is in flight.
    *rig.start_on_read.lock().unwrap() = Some(attempt.clone());
    let mut g = guard_with(0.5);
    tx_meter_poll_step(&rig, &BOTH, &mut g, true, &ib.bus).await;
    assert!(attempt.sampling());
    assert!(attempt.finish().is_empty());
}

#[test]
fn watchdog_invalidated_burst_keeps_no_summary() {
    let band = pancetta_core::Band::from_frequency(14_074_000);
    let attempt = TxDriveAttempt::new(true, band);
    attempt.confirm();
    attempt.started();
    attempt.sample(TxMeterReading {
        alc: Some(0.02),
        forward_power: None,
    });
    attempt.invalidate();
    assert!(!attempt.sampling());
    assert!(attempt.summary().is_none());
    assert!(attempt.finish().is_empty());
}

#[test]
fn closed_sampling_keeps_samples_and_warn_only_suppression() {
    let band = pancetta_core::Band::from_frequency(14_074_000);
    let attempt = TxDriveAttempt::new(true, band);
    attempt.confirm();
    attempt.started();
    attempt.sample(TxMeterReading {
        alc: Some(0.02),
        forward_power: None,
    });
    attempt.close_sampling();
    // The unkey tail: still keyed, so Auto keeps owning the ALC alert.
    assert!(attempt.suppress_warn_only());
    attempt.sample(TxMeterReading {
        alc: Some(0.9),
        forward_power: None,
    });
    assert_eq!(attempt.summary().map(|s| s.len()), Some(1));
}

#[test]
fn unconfirmed_or_unplayed_attempt_has_no_summary() {
    let attempt = TxDriveAttempt::new(true, None);
    assert!(attempt.summary().is_none());
    let attempt = TxDriveAttempt::new(true, None);
    attempt.confirm();
    assert!(attempt.summary().is_none());
    let attempt = TxDriveAttempt::new(true, None);
    attempt.confirm();
    attempt.started();
    assert_eq!(attempt.summary(), Some(Vec::new()));
}
