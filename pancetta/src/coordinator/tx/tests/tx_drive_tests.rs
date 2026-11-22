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
    use crate::coordinator::tx::worker_gate_tests::{request, worker_harness_with_config};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_meter_summary_scales_the_next_real_worker_burst() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use std::sync::atomic::Ordering;

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
    let audio_rx = audio_receiver(&harness).await;

    let (first, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.02]).await;
    let (second, diagnostics) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.8, 0.9]).await;
    assert!(
        diagnostics
            .iter()
            .any(|text| text == "ALC high: drive −5.5 dB. Peak ALC 0.90 (limit 0.50)."),
        "{diagnostics:?}"
    );
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

type WorkerHarness = crate::coordinator::tx::worker_gate_tests::Harness;

/// One worker burst: request `text`, wait for its keyed waveform, confirm the
/// key-down and start playback as Hamlib and Audio would, feed `alc` as the
/// burst's meter samples (`None`: a read without ALC), then wait for the
/// completion. Returns the waveform, TUI diagnostics and drive readouts.
async fn transmit_readings(
    h: &WorkerHarness,
    audio_rx: &crossbeam_channel::Receiver<crate::message_bus::ComponentMessage>,
    text: &str,
    alc: &[Option<f32>],
) -> (
    Vec<f32>,
    Vec<String>,
    Vec<crate::message_bus::RigControlMessage>,
) {
    use crate::coordinator::tx::worker_gate_tests::request;
    use crate::message_bus::{MessageType, RigControlMessage};
    use std::time::{Duration, Instant};
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
    for level in alc {
        attempt.sample(pancetta_core::tx_meter::TxMeterReading {
            alc: *level,
            forward_power: None,
        });
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut diagnostics = Vec::new();
    let mut drive_states = Vec::new();
    let mut completed = None;
    while completed.is_none() && Instant::now() < deadline {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            drop(msg);
        }
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if message_text == text {
                    completed = Some(success);
                }
            }
        }
        // The worker publishes drive state before its completion report.
        while let Ok(msg) = h.tui_rx.try_recv() {
            match msg.message_type {
                MessageType::DiagnosticEvent { text, .. } => diagnostics.push(text),
                MessageType::RigControl(state @ RigControlMessage::TxDriveState { .. }) => {
                    drive_states.push(state)
                }
                _ => (),
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(completed, Some(true), "burst {text} completed");
    (samples, diagnostics, drive_states)
}

async fn transmit(
    h: &WorkerHarness,
    audio_rx: &crossbeam_channel::Receiver<crate::message_bus::ComponentMessage>,
    text: &str,
    alc: &[f32],
) -> (Vec<f32>, Vec<String>) {
    let readings: Vec<Option<f32>> = alc.iter().copied().map(Some).collect();
    let (samples, diagnostics, _) = transmit_readings(h, audio_rx, text, &readings).await;
    (samples, diagnostics)
}

async fn audio_receiver(
    h: &WorkerHarness,
) -> crossbeam_channel::Receiver<crate::message_bus::ComponentMessage> {
    h.coordinator
        .message_bus
        .get_or_create_channel(crate::message_bus::ComponentId::Audio)
        .await
        .unwrap()
        .1
}

/// Drive of a real FT waveform relative to the modulator's normalized peak.
fn waveform_db(samples: &[f32]) -> f32 {
    let peak = samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
    20.0 * (peak / FT_NORMALIZED_PEAK).log10()
}

fn assert_waveform_db(samples: &[f32], expected: f32, what: &str) {
    let measured = waveform_db(samples);
    assert!(
        (measured - expected).abs() < 0.1,
        "{what}: {measured} dB, expected {expected} dB"
    );
}

fn saved(db: f32) -> super::super::tx_drive::SavedTxDrive {
    super::super::tx_drive::SavedTxDrive {
        db,
        mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_burst_uses_the_split_aware_tx_bands_drive() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use std::sync::atomic::Ordering;
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    // An unlearned band would start at −10 dB, so −6 dB proves the 40 m entry.
    config.audio.auto_drive.start_db = -10.0;
    let harness = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
        coordinator
            .tx_drive
            .restore_map(&std::collections::HashMap::from([
                ("20m".to_string(), saved(-3.0)),
                ("40m".to_string(), saved(-6.0)),
            ]));
    })
    .await;
    let audio_rx = audio_receiver(&harness).await;
    // In the 0.10..0.50 ALC band: each band holds its learned drive.
    let (twenty, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&twenty, -3.0, "20 m burst");
    harness
        .coordinator
        .operating_frequency_hz
        .store(7_074_000, Ordering::Release);
    let (forty, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&forty, -6.0, "first 40 m burst after the band change");
    // RX back on 20 m with split TX on 40 m: the TX dial selects 40 m.
    harness
        .coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Release);
    harness
        .coordinator
        .split_tx_frequency_hz()
        .store(7_074_000, Ordering::Release);
    let (split, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&split, -6.0, "first split burst transmitting on 40 m");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlearned_band_starts_at_minus_six_and_ramps() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use std::sync::atomic::Ordering;
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let harness = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(21_074_000, Ordering::Release);
    })
    .await;
    let audio_rx = audio_receiver(&harness).await;
    let (first, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.02]).await;
    assert_waveform_db(&first, -6.0, "first 15 m burst");
    let (second, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.02]).await;
    assert_waveform_db(&second, -5.5, "second 15 m burst");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_invalid_alc_does_not_change_drive() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use crate::message_bus::RigControlMessage;
    use std::sync::atomic::Ordering;
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let harness = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
    })
    .await;
    let audio_rx = audio_receiver(&harness).await;
    let unavailable = |states: &[RigControlMessage]| {
        states.iter().any(|state| {
            matches!(
                state,
                RigControlMessage::TxDriveState {
                    available: false,
                    mode: pancetta_core::tx_drive_mode::TxDriveMode::Auto,
                    ..
                }
            )
        })
    };
    // A meter read without ALC, then an invalid ALC value: no change, and
    // each burst republishes Auto as unavailable.
    let (first, _, states) = transmit_readings(&harness, &audio_rx, "CQ K5ARH EM10", &[None]).await;
    assert_waveform_db(&first, -6.0, "first burst");
    assert!(unavailable(&states), "{states:?}");
    let (second, _, states) =
        transmit_readings(&harness, &audio_rx, "CQ K5ARH EM10", &[Some(f32::NAN)]).await;
    assert_waveform_db(&second, -6.0, "burst after no ALC");
    assert!(unavailable(&states), "{states:?}");
    let (third, _, _) = transmit_readings(&harness, &audio_rx, "CQ K5ARH EM10", &[]).await;
    assert_waveform_db(&third, -6.0, "burst after invalid ALC");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn learned_drive_scales_the_first_worker_buffer_after_restart() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_in_home;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    config.audio.auto_drive.start_db = -3.5;
    let session = super::super::session_state::session_state_path(Some(home.path())).unwrap();
    {
        let harness = worker_harness_in_home(config.clone(), home.path().to_path_buf(), |c| {
            c.operating_frequency_hz
                .store(14_074_000, Ordering::Release);
        })
        .await;
        let audio_rx = audio_receiver(&harness).await;
        let (first, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.02]).await;
        assert_waveform_db(&first, -3.5, "first 20 m burst before restart");
        // The station learned −3 dB; wait for its save to land.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let super::super::session_state::SessionLoad::Record(record) =
                super::super::session_state::load(&session)
            {
                if record.tx_drive_by_band.get("20m").map(|s| s.db) == Some(-3.0) {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "learned drive was not saved");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // Restart: a new coordinator reads the same session home at startup.
    let restarted = worker_harness_in_home(config, home.path().to_path_buf(), |c| {
        c.operating_frequency_hz
            .store(14_074_000, Ordering::Release);
    })
    .await;
    let audio_rx = audio_receiver(&restarted).await;
    let (first, _) = transmit(&restarted, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&first, -3.0, "first 20 m burst after restart");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_burst_steps_drive_once_with_one_key_down() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, RigControlMessage, TransmitRequestItem,
        TxOrigin,
    };
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let harness = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
        coordinator
            .tx_drive
            .restore_map(&std::collections::HashMap::from([(
                "20m".to_string(),
                saved(-4.0),
            )]));
    })
    .await;
    let audio_rx = audio_receiver(&harness).await;
    let texts = ["CQ N0CALL FN20", "CQ N0CALL EN27"];
    let item = |text: &str, offset: f64| TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: offset,
        qso_id: None,
        origin: TxOrigin::Local,
        remote_client_key_id: None,
    };
    harness
        .coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![item(texts[0], 1000.0), item(texts[1], 1600.0)],
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut key_downs = Vec::new();
    let mut audio = Vec::new();
    let mut completed = std::collections::HashSet::new();
    let mut fed = false;
    while completed.len() < texts.len() {
        assert!(Instant::now() < deadline, "bundle did not complete");
        while let Ok(msg) = harness.hamlib_rx.try_recv() {
            if matches!(
                msg.message_type,
                MessageType::RigControl(RigControlMessage::SetPtt { state: true })
            ) {
                key_downs.push(msg.tx_drive_attempt);
            }
        }
        while let Ok(msg) = audio_rx.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                playback_started,
                ..
            } = msg.message_type
            {
                audio.push((samples, playback_started));
            }
        }
        if !fed && !key_downs.is_empty() && !audio.is_empty() {
            fed = true;
            let attempt = key_downs[0]
                .clone()
                .expect("FT key-down carries an attempt");
            attempt.confirm();
            audio[0]
                .1
                .take()
                .expect("audio ack")
                .started(chrono::Utc::now());
            attempt.sample(alc_sample(0.02));
            attempt.sample(alc_sample(0.03));
        }
        while let Ok(msg) = harness.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success: true,
                message_text,
                ..
            } = msg.message_type
            {
                completed.insert(message_text);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(key_downs.len(), 1, "one key-down for the bundle");
    assert_eq!(audio.len(), 1, "one waveform for the bundle");
    assert_waveform_db(&audio[0].0, -4.0, "bundle burst");
    // Two items, one burst: exactly one +0.5 dB step.
    let (next, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&next, -3.5, "burst after the bundle");
}

#[test]
fn calibrate_band_seeds_auto_establishes_and_bumps_generation() {
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let drive = TxDrive::from_config(&config);
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let prior = drive.snapshot_for_band(Some(band));
    let selected = drive.calibrate_band(band, -3.0);
    assert_eq!(selected.db, -3.0);
    assert_eq!(selected.mode, pancetta_core::TxDriveMode::Auto);
    assert_ne!(selected.generation, prior.generation);
    assert!(drive.needs_persist());
    assert_eq!(drive.restored_map_with_generation().0["20m"].db, -3.0);
}

#[test]
fn a_stale_burst_summary_cannot_undo_calibration() {
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let drive = TxDrive::from_config(&config);
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let old = drive.snapshot_for_band(Some(band));
    drive.calibrate_band(band, -3.0);
    let result = drive.complete(
        old,
        &[],
        DriveContext {
            rig_generation: 0,
            ft_mode: 0,
            stream_count: 1,
        },
    );
    assert_eq!(result, DriveDecision::Stale);
    assert_eq!(drive.snapshot_for_band(Some(band)).db, -3.0);
}

#[tokio::test]
async fn calibrated_drive_scales_the_next_real_burst() {
    use crate::coordinator::tx::worker_gate_tests::worker_harness_with;
    use std::sync::atomic::Ordering;
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
                "20m".to_string(),
                saved(-8.0),
            )]));
    })
    .await;
    harness.coordinator.tx_drive.calibrate_band(band, -3.0);
    let audio_rx = audio_receiver(&harness).await;
    let (samples, _) = transmit(&harness, &audio_rx, "CQ K5ARH EM10", &[0.2]).await;
    assert_waveform_db(&samples, -3.0, "first burst after calibration");
}

#[test]
fn calibrate_band_clamps_to_configured_bounds() {
    let mut config = pancetta_config::Config::default();
    config.audio.auto_drive.enabled = true;
    let drive = TxDrive::from_config(&config);
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    assert_eq!(drive.calibrate_band(band, -40.0).db, -20.0);
    assert_eq!(drive.calibrate_band(band, 20.0).db, 0.0);
}
