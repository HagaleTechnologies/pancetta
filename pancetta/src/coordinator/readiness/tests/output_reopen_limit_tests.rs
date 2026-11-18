//! PAN-295: the readiness snapshot carries the output reopen limit beside,
//! never instead of, the `AudioOutputDead` mute cause.
use super::tests::sources;
use super::*;

#[test]
fn pan295_exhaustion_tracks_audio_readiness() {
    let s = sources();
    let mut publisher = ReadinessPublisher::default();
    let healthy = s.snapshot(false, false);
    assert!(!healthy.audio_output_reopen_exhausted);
    assert!(healthy.tx_mute_causes.is_empty());
    publisher.observe(&healthy);

    // Dead, recovery still allowed: the plain audio blocker only.
    s.audio_output_alive.store(false, Ordering::Release);
    let dead = s.snapshot(false, false);
    assert_eq!(dead.tx_mute_causes, vec![TuiTxMuteCause::AudioOutputDead]);
    assert!(!dead.audio_output_reopen_exhausted);
    assert_eq!(publisher.observe(&dead), (true, false));

    // Budget spent: same mute causes, but the snapshot changes and is pushed.
    s.audio_output_reopen_exhausted
        .store(true, Ordering::Release);
    let limited = s.snapshot(false, false);
    assert_eq!(limited.tx_mute_causes, dead.tx_mute_causes);
    assert!(limited.audio_output_reopen_exhausted);
    assert_eq!(publisher.observe(&limited), (true, false));
    assert_eq!(publisher.observe(&s.snapshot(false, false)), (false, false));

    // Expiry clears the limit; the dead blocker stays until progress.
    s.audio_output_reopen_exhausted
        .store(false, Ordering::Release);
    let expired = s.snapshot(false, false);
    assert_eq!(expired, dead);
    assert_eq!(publisher.observe(&expired), (true, false));

    // Callback progress clears both.
    s.audio_output_alive.store(true, Ordering::Release);
    assert_eq!(s.snapshot(false, false), healthy);
}

#[tokio::test]
async fn pan295_inactive_audio_modes_do_not_show_exhaustion() {
    let scratch = tempfile::tempdir().unwrap();
    let mut c = crate::coordinator::ApplicationCoordinator::new(
        pancetta_config::Config::default(),
        None,
        true, // no_audio
        true, // headless
        None,
        None,
        None,
        1500.0,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
        scratch.path().join("pancetta.toml"),
        true,
        None,
    )
    .await
    .unwrap();
    let (to_dsp, _dsp_rx) = crossbeam_channel::unbounded();
    let (_tx_audio, tx_audio_rx) = crossbeam_channel::unbounded();
    c.start_audio_pipeline(to_dsp, tx_audio_rx, Arc::new(AtomicBool::new(false)))
        .await
        .unwrap();
    assert!(c.audio_output_alive.load(Ordering::Acquire));
    assert!(!c.audio_output_reopen_exhausted.load(Ordering::Acquire));
    let snap = ReadinessSources::from_coordinator(&c).snapshot(false, false);
    assert!(!snap.audio_output_reopen_exhausted);
    assert!(!snap
        .tx_mute_causes
        .contains(&TuiTxMuteCause::AudioOutputDead));
}
