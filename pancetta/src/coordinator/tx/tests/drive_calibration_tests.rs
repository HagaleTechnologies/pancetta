use super::pan115_worker_gate_tests::{worker_harness_in_home, worker_harness_with};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, RigControlMessage};
use pancetta_config::Config;
use pancetta_core::tx_meter::TxMeterCapability;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

async fn station() -> super::pan115_worker_gate_tests::Harness {
    let mut config = Config::default();
    config.audio.auto_drive.enabled = true;
    config.audio.levels.output_gain_ceiling_db = 5.0;
    let h = worker_harness_with(config, |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
    })
    .await;
    *h.coordinator
        .message_bus
        .tx_meter_capability
        .lock()
        .unwrap() = Some(TxMeterCapability {
        alc: true,
        forward_power: false,
        alc_limit: None,
    });
    h
}

async fn request(h: &super::pan115_worker_gate_tests::Harness) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::DriveCalibrationRequest {
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn calibration_steps_from_the_floor_and_saves_one_step_below_first_onset() {
    let h = station().await;
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    request(&h).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut levels = Vec::new();
    let mut durations = Vec::new();
    let mut on = 0;
    let mut off = 0;
    while Instant::now() < deadline && off == 0 {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    on += 1;
                } else {
                    off += 1;
                }
            }
        }
        while let Ok(msg) = audio.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                sample_rate,
                ..
            } = msg.message_type
            {
                if samples.is_empty() {
                    continue;
                }
                durations.push(samples.len() as u64 * 1000 / sample_rate as u64);
                let peak = samples.iter().map(|v| v.abs()).fold(0.0_f32, f32::max);
                let db = (20.0 * (peak / 0.5).log10()).round();
                levels.push(db);
                let sink = h
                    .coordinator
                    .message_bus
                    .active_drive_calibration
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap();
                sink.ticks.lock().unwrap().push((
                    Instant::now() + Duration::from_millis(200),
                    Some(if db >= -2.0 { 0.11 } else { 0.01 }),
                ));
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!((on, off), (1, 1));
    assert_eq!(levels.first(), Some(&-20.0));
    assert!(levels.contains(&-3.0));
    assert_eq!(durations[0], 750);
    assert!(durations[1..].iter().all(|d| *d == 600));
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    assert_eq!(
        h.coordinator.tx_drive.snapshot_for_band(Some(band)).db,
        -3.0
    );
    let mut reports = Vec::new();
    let report_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < report_deadline {
        reports.extend(h.tui_rx.try_iter());
        if reports.iter().any(|msg| {
            matches!(&msg.message_type,
            MessageType::DiagnosticEvent { text, .. } if text.starts_with("Drive calibrated:"))
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reports.iter().any(|msg| matches!(msg.message_type,
        MessageType::RigControl(RigControlMessage::TxDriveState { db, .. }) if db == -3.0)));
    assert!(
        reports.iter().any(|msg| matches!(&msg.message_type,
        MessageType::DiagnosticEvent { text, .. }
        if text == "Drive calibrated: 20 m → −3 dB (was −6 dB)")),
        "{reports:?}"
    );
    assert!(h
        .coordinator
        .message_bus
        .active_drive_calibration
        .lock()
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn drive_calibration_is_hard_muted_under_disabled_policy() {
    let h = station().await;
    h.coordinator
        .tx_policy
        .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    request(&h).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(audio.try_recv().is_err());
    assert!(!h.hamlib_rx.try_iter().any(|msg| matches!(
        msg.message_type,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true })
    )));
    assert!(h.tui_rx.try_iter().any(|msg| matches!(
        msg.message_type,
        MessageType::DiagnosticEvent {
            target: "tx.policy",
            ..
        }
    )));
}

#[tokio::test]
async fn calibration_refuses_without_alc_capability() {
    let h = station().await;
    *h.coordinator
        .message_bus
        .tx_meter_capability
        .lock()
        .unwrap() = Some(TxMeterCapability::default());
    request(&h).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!h.hamlib_rx.try_iter().any(|msg| matches!(
        msg.message_type,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true })
    )));
}

struct RunResult {
    levels: Vec<f32>,
    keyed_for: Duration,
    flushed: bool,
    diagnostics: Vec<String>,
    saved_db: f32,
    persisted_db: Option<f32>,
    sink_cleared: bool,
}

async fn run_script(initial: Option<f32>, alc_for_level: impl Fn(f32) -> Option<f32>) -> RunResult {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.audio.auto_drive.enabled = true;
    config.audio.levels.output_gain_ceiling_db = 5.0;
    let h = worker_harness_in_home(config, home.path().to_path_buf(), |coordinator| {
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
    })
    .await;
    *h.coordinator
        .message_bus
        .tx_meter_capability
        .lock()
        .unwrap() = Some(TxMeterCapability {
        alc: true,
        forward_power: false,
        alc_limit: None,
    });
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    if let Some(db) = initial {
        h.coordinator.tx_drive.calibrate_band(band, db);
    }
    let before = h.coordinator.tx_drive.snapshot_for_band(Some(band)).db;
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    request(&h).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut levels = Vec::new();
    let mut on = None;
    let mut off = None;
    let mut flushed = false;
    let mut diagnostics = Vec::new();
    while Instant::now() < deadline && off.is_none() {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    assert!(on.is_none());
                    on = Some(Instant::now());
                } else {
                    off = Some(Instant::now());
                }
            }
        }
        while let Ok(msg) = audio.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                flush_first,
                ..
            } = msg.message_type
            {
                if samples.is_empty() {
                    flushed |= flush_first;
                    continue;
                }
                let peak = samples.iter().map(|v| v.abs()).fold(0.0_f32, f32::max);
                let db = (20.0 * (peak / 0.5).log10()).round();
                levels.push(db);
                let sink = h
                    .coordinator
                    .message_bus
                    .active_drive_calibration
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap();
                sink.ticks.lock().unwrap().push((
                    Instant::now() + Duration::from_millis(200),
                    alc_for_level(db),
                ));
            }
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                diagnostics.push(text);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let keyed_for = off.expect("PTT-off") - on.expect("PTT-on");
    while let Ok(msg) = h.tui_rx.try_recv() {
        if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
            diagnostics.push(text);
        }
    }
    let saved_db = h.coordinator.tx_drive.snapshot_for_band(Some(band)).db;
    let mut persisted_db = None;
    if saved_db != before {
        if let Some(path) = h.coordinator.session_state_path.as_ref() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let super::super::session_state::SessionLoad::Record(record) =
                    super::super::session_state::load(path)
                {
                    persisted_db = record.tx_drive_by_band.get("20m").map(|v| v.db);
                    if persisted_db == Some(saved_db) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    assert!(before.is_finite());
    let sink_cleared = h
        .coordinator
        .message_bus
        .active_drive_calibration
        .lock()
        .unwrap()
        .is_none();
    RunResult {
        levels,
        keyed_for,
        flushed,
        diagnostics,
        saved_db,
        persisted_db,
        sink_cleared,
    }
}

#[tokio::test]
async fn calibration_unkeys_within_the_tune_time_limit() {
    let result = run_script(None, |_| Some(0.0)).await;
    assert!(
        result.keyed_for < Duration::from_secs(12),
        "{:?}",
        result.keyed_for
    );
    assert_eq!(result.saved_db, 5.0);
    assert!(result.sink_cleared);
}

#[tokio::test]
async fn calibration_overwrites_a_learned_value_for_the_next_transmission() {
    let result = run_script(Some(-8.0), |db| Some(if db >= -2.0 { 0.11 } else { 0.01 })).await;
    assert_eq!(result.saved_db, -3.0);
    assert_eq!(result.persisted_db, Some(-3.0));
    assert_eq!(result.levels.first(), Some(&-20.0));
    assert!(result
        .diagnostics
        .iter()
        .any(|s| s.contains("was -8 dB") || s.contains("was −8 dB")));
}

#[tokio::test]
async fn calibration_aborts_when_alc_readings_stop_mid_run() {
    let result = run_script(Some(-8.0), |_| None).await;
    assert_eq!(result.saved_db, -8.0);
    assert!(result.flushed);
    assert!(result.sink_cleared);
    assert!(result
        .diagnostics
        .iter()
        .any(|s| s.contains("ALC readings stopped mid-run")));
}

#[tokio::test]
async fn calibration_runs_are_never_sampled_after_the_end() {
    let result = run_script(None, |_| Some(0.2)).await;
    assert!(result.sink_cleared);
}

#[tokio::test]
async fn drive_calibration_aborts_on_operator_halt() {
    let h = station().await;
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let before = h.coordinator.tx_drive.snapshot_for_band(Some(band)).db;
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    request(&h).await;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut keyed = false;
    let mut flushed = false;
    let mut off = false;
    while Instant::now() < deadline && !off {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    keyed = true;
                } else {
                    off = true;
                }
            }
        }
        while let Ok(msg) = audio.try_recv() {
            if let MessageType::AudioOutput {
                samples,
                flush_first,
                ..
            } = msg.message_type
            {
                if !samples.is_empty() {
                    h.coordinator
                        .abort_current_tx
                        .store(true, Ordering::Release);
                } else {
                    flushed |= flush_first;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(keyed && off && flushed);
    let mut warned = false;
    let warning_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < warning_deadline && !warned {
        warned = h.tui_rx.try_iter().any(|msg| {
            matches!(msg.message_type,
            MessageType::DiagnosticEvent { target: "tx.drive", text, .. }
            if text.contains("operator halt"))
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(warned);
    assert_eq!(
        h.coordinator.tx_drive.snapshot_for_band(Some(band)).db,
        before
    );
}

#[tokio::test]
async fn calibration_aborts_if_audio_stalls_mid_run() {
    let h = station().await;
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let before = h.coordinator.tx_drive.snapshot_for_band(Some(band)).db;
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    request(&h).await;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut keyed = false;
    let mut off = false;
    while Instant::now() < deadline && !off {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    keyed = true;
                } else {
                    off = true;
                }
            }
        }
        while let Ok(msg) = audio.try_recv() {
            if let MessageType::AudioOutput { samples, .. } = msg.message_type {
                if !samples.is_empty() {
                    h.coordinator
                        .audio_output_alive
                        .store(false, Ordering::Release);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(keyed && off);
    assert_eq!(
        h.coordinator.tx_drive.snapshot_for_band(Some(band)).db,
        before
    );
}

#[tokio::test]
async fn calibration_does_not_save_if_audio_segment_is_dropped() {
    let h = station().await;
    let band = pancetta_core::Band::from_frequency(14_074_000).unwrap();
    let before = h.coordinator.tx_drive.snapshot_for_band(Some(band)).db;
    let _audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    // Keep the receiver attached but undrained so the calibration's first
    // AudioOutput is dropped by the bounded bus route.
    let mut full = false;
    for _ in 0..20_000 {
        let queued = h
            .coordinator
            .message_bus
            .send_message_checked(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Audio,
                MessageType::AudioOutput {
                    samples: Vec::new(),
                    sample_rate: 48_000,
                    flush_first: false,
                    playback_started: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        if !queued {
            full = true;
            break;
        }
    }
    assert!(full, "Audio route did not fill");
    request(&h).await;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut keyed = false;
    let mut off = false;
    while Instant::now() < deadline && !off {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if let MessageType::RigControl(RigControlMessage::SetPtt { state }) = msg.message_type {
                if state {
                    keyed = true;
                } else {
                    off = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(keyed && off);
    assert_eq!(
        h.coordinator.tx_drive.snapshot_for_band(Some(band)).db,
        before
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut warned = false;
    while Instant::now() < deadline && !warned {
        warned = h.tui_rx.try_iter().any(|msg| {
            matches!(msg.message_type,
            MessageType::DiagnosticEvent { target: "tx.drive", text, .. }
            if text.contains("audio output dropped"))
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(warned);
}
