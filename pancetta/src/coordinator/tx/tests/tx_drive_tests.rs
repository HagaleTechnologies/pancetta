use super::*;

fn assert_db(baseline: &[f32], adjusted: &[f32], expected_db: f32) {
    assert_eq!(baseline.len(), adjusted.len());
    let baseline_power: f64 = baseline.iter().map(|s| f64::from(*s).powi(2)).sum();
    let adjusted_power: f64 = adjusted.iter().map(|s| f64::from(*s).powi(2)).sum();
    let measured = 10.0 * (adjusted_power / baseline_power).log10();
    assert!(
        (measured - f64::from(expected_db)).abs() < 0.02,
        "{measured} dB"
    );
    let gain = 10.0_f32.powf(expected_db / 20.0);
    for (before, after) in baseline.iter().zip(adjusted) {
        assert!((before * gain - after).abs() < 0.00001);
    }
}

#[test]
fn drive_down_changes_only_the_next_ft8_buffer() {
    let drive = TxDrive::new(0.0, 0.0);
    let baseline = [0.0, 0.25, -0.5, 0.5];
    let mut queued = baseline;
    let first_key_db = drive.get();
    apply(&mut queued, first_key_db, drive.ceiling_db(), false);
    assert_eq!(queued, baseline);
    drive.adjust(-1);
    assert_eq!(queued, baseline);
    let mut next = baseline;
    apply(&mut next, drive.get(), drive.ceiling_db(), false);
    assert_db(&baseline, &next, -1.0);
}

#[test]
fn drive_limits_preserve_default_buffers_and_positive_headroom() {
    let drive = TxDrive::new(0.0, 0.0);
    assert_eq!(drive.adjust(1), 0.0);
    let original = vec![0.0, 0.5, -0.5, 0.95];
    let mut unchanged = original.clone();
    apply(&mut unchanged, drive.get(), drive.ceiling_db(), true);
    assert_eq!(unchanged, original);
    for _ in 0..40 {
        drive.adjust(-1);
    }
    assert_eq!(drive.get(), -20.0);
    assert_eq!(drive.adjust(-1), -20.0);
    let boosted = TxDrive::new(5.0, 5.0);
    for (baseline, ft_frame) in [(0.5, false), (FT_NORMALIZED_PEAK, true)] {
        let mut output = [baseline, -baseline];
        apply(&mut output, boosted.get(), boosted.ceiling_db(), ft_frame);
        assert!(output.iter().all(|s| s.is_finite()));
        assert!(output
            .iter()
            .all(|s| s.abs() <= 10.0_f32.powf(-1.0 / 20.0) + 0.00001));
    }
}

#[test]
fn configured_minus_six_scales_single_bundle_and_rekey() {
    let drive = TxDrive::new(-6.0, 0.0);
    let baseline = vec![0.0, 0.1, 0.5, -0.5, -0.2];
    for bundle in [false, true] {
        let mut first = baseline.clone();
        apply(&mut first, drive.get(), drive.ceiling_db(), bundle);
        assert_db(&baseline, &first, -6.0);
        drive.adjust(1);
        let mut rekey = baseline.clone();
        apply(&mut rekey, drive.get(), drive.ceiling_db(), bundle);
        assert_db(&baseline, &rekey, -5.0);
        drive.adjust(-1);
    }
}

#[test]
fn tune_uses_the_same_drive() {
    let baseline: Vec<f32> = (0..12000)
        .map(|i| ((i as f64) * 2.0 * std::f64::consts::PI * 1500.0 / 12000.0).sin() as f32 * 0.5)
        .collect();
    let mut output = baseline.clone();
    apply(&mut output, -3.0, 0.0, false);
    assert_db(&baseline, &output, -3.0);
}

#[test]
fn saved_drive_is_used_by_the_first_buffer_after_restart() {
    let mut config = pancetta_config::Config::default();
    config.audio.levels.output_gain_db = -4.0;
    let fresh = TxDrive::from_config(&config);
    let mut output = [0.5, -0.5];
    apply(&mut output, fresh.get(), fresh.ceiling_db(), false);
    assert_db(&[0.5, -0.5], &output, -4.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_drive_scales_real_worker_audio() {
    use crate::coordinator::tx::pan115_worker_gate_tests::{request, worker_harness_with_config};
    use crate::message_bus::{ComponentId, MessageType};
    use std::time::Duration;

    async fn capture(db: f32) -> Vec<f32> {
        let mut config = pancetta_config::Config::default();
        config.audio.auto_drive.enabled = false;
        config.audio.levels.output_gain_db = db;
        let harness = worker_harness_with_config(config).await;
        let (_, receiver) = harness
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();
        request(&harness, "CQ K5ARH EM10").await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(message) = receiver.try_recv() {
                    if let MessageType::AudioOutput { samples, .. } = message.message_type {
                        return samples;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker emitted TX audio")
    }

    let (baseline, driven) = tokio::join!(capture(0.0), capture(-6.0));
    let peak = |samples: &[f32]| {
        samples
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max)
    };
    let ratio_db = 20.0 * (peak(&driven) / peak(&baseline)).log10();
    assert!(
        (ratio_db + 6.0).abs() < 0.02,
        "worker audio delta {ratio_db} dB"
    );
}

/// Codex P1 on #705: a single FT8 frame leaves the modulator normalized to a
/// 0.95 peak, not 0.5, so a positive ceiling must reserve headroom for it too.
#[test]
fn positive_drive_keeps_a_real_single_frame_below_minus_one_dbfs() {
    let mut modulator = pancetta_ft8::Ft8Modulator::new_default().unwrap();
    let symbols: [u8; pancetta_ft8::NUM_SYMBOLS] =
        std::array::from_fn(|i| u8::try_from(i % 8).unwrap());
    let frame = modulator.modulate_symbols(&symbols, 1500.0).unwrap();
    let peak = |s: &[f32]| s.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
    assert!(
        (peak(&frame) - FT_NORMALIZED_PEAK).abs() < 0.01,
        "{}",
        peak(&frame)
    );
    let limit = 10.0_f32.powf(-1.0 / 20.0) + 0.00001;
    for ceiling in [1.0, 3.0, 5.0] {
        let drive = TxDrive::new(ceiling, ceiling);
        let mut output = frame.clone();
        apply(&mut output, drive.get(), drive.ceiling_db(), true);
        assert!(
            peak(&output) <= limit,
            "+{ceiling} dB peaks at {}",
            peak(&output)
        );
        // One step down is still exactly 1 dB quieter than the ceiling.
        let mut lower = frame.clone();
        apply(&mut lower, drive.get() - 1.0, drive.ceiling_db(), true);
        let ratio = peak(&lower) / peak(&output);
        assert!((20.0 * ratio.log10() + 1.0).abs() < 0.01, "{ratio}");
    }
}

fn automatic_drive() -> TxDrive {
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    TxDrive::from_config(&config)
}

fn alc_sample(alc: f32) -> pancetta_core::tx_meter::TxMeterReading {
    pancetta_core::tx_meter::TxMeterReading {
        alc: Some(alc),
        forward_power: None,
    }
}

fn completed_burst(
    drive: &TxDrive,
    band: pancetta_core::Band,
    db: f32,
    samples: &[pancetta_core::tx_meter::TxMeterReading],
) {
    let snapshot = drive.snapshot_for_band(Some(band));
    assert_eq!(snapshot.db, db);
    let _ = drive.complete(
        snapshot,
        samples,
        super::super::tx_drive::DriveContext {
            rig_generation: 1,
            ft_mode: 0,
            stream_count: 1,
        },
    );
}

#[test]
fn clean_meter_summary_scales_next_buffer() {
    let drive = automatic_drive();
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    for expected in [-6.0, -5.5, -5.0, -4.5, -4.0] {
        completed_burst(&drive, band, expected, &[alc_sample(0.02)]);
    }
    let snapshot = drive.snapshot_for_band(Some(band));
    assert_eq!(snapshot.db, -3.5);
    let baseline = [0.0, 0.25, -0.5, 0.95];
    let mut next = baseline;
    apply(&mut next, snapshot.db, drive.ceiling_db(), true);
    assert_db(&baseline, &next, -3.5);
}

#[test]
fn overdrive_backs_off_the_next_burst_once() {
    let drive = automatic_drive();
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    for expected in [-6.0, -5.5, -5.0, -4.5, -4.0, -3.5, -3.0, -2.5, -2.0, -1.5] {
        completed_burst(&drive, band, expected, &[alc_sample(0.01)]);
    }
    completed_burst(&drive, band, -1.0, &[alc_sample(0.6), alc_sample(0.7)]);
    assert_eq!(drive.snapshot_for_band(Some(band)).db, -3.0);
}

#[test]
fn first_burst_uses_the_split_aware_tx_bands_drive() {
    let drive = automatic_drive();
    let twenty = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let forty = pancetta_core::Band::from_frequency(7_074_000).unwrap();
    drive.restore_map(&std::collections::HashMap::from([
        (
            twenty.name().to_string(),
            super::super::tx_drive::SavedTxDrive {
                db: -3.0,
                mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
            },
        ),
        (
            forty.name().to_string(),
            super::super::tx_drive::SavedTxDrive {
                db: -6.0,
                mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
            },
        ),
    ]));
    let band = pancetta_core::Band::from_frequency(pancetta_qso::qso_manager::effective_tx_dial(
        14_074_000, 7_074_000,
    ));
    assert_eq!(drive.snapshot_for_band(band).db, -6.0);
}

#[test]
fn unlearned_band_starts_at_minus_six_and_ramps() {
    let drive = automatic_drive();
    let fifteen = pancetta_core::Band::from_frequency(21_074_000).unwrap();
    let baseline = [0.0, 0.25, -0.5, 0.95];
    let mut first = baseline;
    apply(
        &mut first,
        drive.snapshot_for_band(Some(fifteen)).db,
        drive.ceiling_db(),
        true,
    );
    assert_db(&baseline, &first, -6.0);
    completed_burst(&drive, fifteen, -6.0, &[alc_sample(0.01)]);
    let mut second = baseline;
    apply(
        &mut second,
        drive.snapshot_for_band(Some(fifteen)).db,
        drive.ceiling_db(),
        true,
    );
    assert_db(&baseline, &second, -5.5);
}

#[test]
fn missing_or_invalid_alc_does_not_change_drive() {
    let drive = automatic_drive();
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    completed_burst(&drive, band, -6.0, &[]);
    assert_eq!(drive.snapshot_for_band(Some(band)).db, -6.0);
    completed_burst(&drive, band, -6.0, &[alc_sample(f32::NAN)]);
    assert_eq!(drive.snapshot_for_band(Some(band)).db, -6.0);
}

#[test]
fn learned_drive_scales_the_first_worker_buffer_after_restart() {
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let first = automatic_drive();
    first.restore_map(&std::collections::HashMap::from([(
        band.name().to_string(),
        super::super::tx_drive::SavedTxDrive {
            db: -3.0,
            mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
        },
    )]));
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("session_state.json");
    let record = std::sync::Arc::new(std::sync::Mutex::new(
        super::super::session_state::SessionRecord::default(),
    ));
    super::super::session_state::persist_tx_drive_current(&record, &Some(path.clone()), &first)
        .unwrap();
    let loaded = match super::super::session_state::load(&path) {
        super::super::session_state::SessionLoad::Record(record) => record,
        other => panic!("unexpected session load {other:?}"),
    };
    let restarted = automatic_drive();
    restarted.restore_map(&loaded.tx_drive_by_band);
    let selected = restarted.snapshot_for_band(Some(band));
    assert_eq!(selected.db, -3.0);
    let baseline = [0.0, 0.25, -0.5, 0.95];
    let mut first_buffer = baseline;
    apply(&mut first_buffer, selected.db, restarted.ceiling_db(), true);
    assert_db(&baseline, &first_buffer, -3.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_meter_summary_scales_the_next_real_worker_burst() {
    use crate::coordinator::tx::pan115_worker_gate_tests::{
        await_completion, request, worker_harness_with,
    };
    use crate::message_bus::{ComponentId, MessageType, RigControlMessage};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let harness = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
        coordinator
            .tx_drive
            .restore_map(&std::collections::HashMap::from([(
                band.name().to_string(),
                super::super::tx_drive::SavedTxDrive {
                    db: -4.0,
                    mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
                },
            )]));
    })
    .await;
    let (_, audio_rx) = harness
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();

    async fn transmit(
        h: &crate::coordinator::tx::pan115_worker_gate_tests::Harness,
        audio_rx: &crossbeam_channel::Receiver<crate::message_bus::ComponentMessage>,
        text: &str,
        alc: &[f32],
    ) -> (Vec<f32>, Vec<String>) {
        request(h, text).await;
        let (attempt, samples, playback) = tokio::time::timeout(Duration::from_secs(25), async {
            let mut attempt = None;
            let mut audio = None;
            loop {
                while let Ok(msg) = h.hamlib_rx.try_recv() {
                    if matches!(
                        msg.message_type,
                        MessageType::RigControl(RigControlMessage::SetPtt { state: true })
                    ) {
                        attempt = msg.tx_drive_attempt;
                    }
                }
                while let Ok(msg) = audio_rx.try_recv() {
                    if let MessageType::AudioOutput {
                        samples,
                        playback_started,
                        ..
                    } = msg.message_type
                    {
                        audio = Some((samples, playback_started));
                    }
                }
                if attempt.is_some() && audio.is_some() {
                    let (samples, playback) = audio.take().unwrap();
                    break (attempt.take().unwrap(), samples, playback);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker sent one keyed waveform");
        attempt.confirm();
        playback.expect("audio ack").started(chrono::Utc::now());
        for &level in alc {
            attempt.sample(alc_sample(level));
        }
        let (completed, diagnostics, _) = await_completion(h, text, Duration::from_secs(20)).await;
        assert_eq!(completed, Some(true));
        (samples, diagnostics)
    }

    let (first, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.02]).await;
    let (second, diagnostics) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.8, 0.9]).await;
    let peak = |samples: &[f32]| samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
    let gain_db = 20.0 * (peak(&second) / peak(&first)).log10();
    assert!(
        (gain_db - 0.5).abs() < 0.02,
        "next worker burst gain {gain_db} dB"
    );
    assert!(
        diagnostics
            .iter()
            .any(|text| text.contains("ALC high: drive −5.5 dB")),
        "{diagnostics:?}"
    );
    let (third, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    let backoff_db = 20.0 * (peak(&third) / peak(&second)).log10();
    assert!(
        (backoff_db + 2.0).abs() < 0.02,
        "overdrive backoff {backoff_db} dB"
    );
}
