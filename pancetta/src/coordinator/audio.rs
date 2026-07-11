//! Audio component startup.
//!
//! Spawns the audio capture thread (cpal-driven `AudioManager`) plus an
//! async relay that forwards 48 kHz samples to the DSP stage. Failures
//! during init or the runtime loop surface to the TUI via
//! `MessageType::Error` so a wedged audio device doesn't silently
//! starve the rest of the pipeline.
//!
//! Two modes:
//!  - `PANCETTA_STUB_AUDIO=1` — synthesize a 1500 Hz tone for offline testing
//!  - default — real `AudioManager` reading from the configured input device

use anyhow::Result;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// A request to switch the live audio device(s) without restarting pancetta.
///
/// Sent by the TUI `SelectDevice` handler (`tui_relay.rs`) over the
/// coordinator's `audio_reopen_tx` channel and consumed by the dedicated audio
/// thread, which owns the (non-`Send`) cpal streams. The thread calls
/// [`AudioManager::reopen_devices`](pancetta_audio::AudioManager::reopen_devices),
/// then reports the outcome back over `respond` so the handler can surface a
/// success/failure `StatusUpdate` to the operator. On failure the audio thread
/// has already rolled back to the previous device (see `reopen_devices`), so the
/// `Err` here means "stayed on the old device", not "audio is dead".
pub struct AudioReopenRequest {
    /// New input device name pattern, or `None` to leave input unchanged.
    pub input: Option<String>,
    /// New output device name pattern, or `None` to leave output unchanged.
    pub output: Option<String>,
    /// Force a full stream teardown+reopen even if the resolved device names are
    /// unchanged. Set `true` for an explicit operator pick from the device
    /// picker: re-selecting the SAME device is how the operator *reclaims* a
    /// device that something else (e.g. a remote-desktop client like Jump
    /// Desktop) hijacked at the OS level — the names match but the stream must
    /// still be rebuilt to re-grab the hardware. The unchanged-is-no-op
    /// optimization only applies to non-forced callers.
    pub force: bool,
    /// One-shot reply channel: `Ok(())` on a successful live switch, or
    /// `Err(message)` describing why it failed (with the old device kept live).
    pub respond: tokio::sync::oneshot::Sender<Result<(), String>>,
}

impl super::ApplicationCoordinator {
    pub(crate) async fn start_audio_pipeline(
        &mut self,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
        tx_audio_rx: crossbeam_channel::Receiver<(Vec<f32>, u32)>,
        health_audio_alive: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        if self.no_audio {
            info!("Audio processing disabled");
            return Ok(());
        }

        let span = span!(Level::INFO, "start_audio");
        let _enter = span.enter();

        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();

        if use_stub {
            info!("Starting audio component in STUB mode");

            let config = self.config.read().await;
            let sample_rate = config.audio.sample_rate;
            let buffer_size = config.audio.buffer_size as usize;
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            let handle = tokio::spawn(async move {
                let mut phase = 0.0f32;
                let frequency = 1500.0;
                let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
                let mut process_interval =
                    interval(Duration::from_millis(buffer_duration_ms.max(5)));

                while !shutdown.load(Ordering::Acquire) {
                    process_interval.tick().await;

                    let mut samples = Vec::with_capacity(buffer_size);
                    for _ in 0..buffer_size {
                        let sample = 0.1 * phase.sin();
                        samples.push(sample);
                        phase += 2.0 * std::f32::consts::PI * frequency / sample_rate as f32;
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                    }

                    // Perf (Pass 1 / A10): lock-free atomic store (was an async
                    // RwLock write on every audio batch — the hottest lock).
                    last_timestamp.store(super::now_epoch_ms(), Ordering::Relaxed);

                    if audio_to_dsp_tx.send(samples).is_err() {
                        break;
                    }
                }

                info!("Audio stub stopped");
                Ok(())
            });

            self.named_task_handles.push((ComponentId::Audio, handle));
        } else {
            info!("Starting audio component with real AudioManager");

            let config = self.config.read().await;
            // CLI --audio-device overrides BOTH input and output, since the
            // typical ham-radio rig presents bidirectional USB audio (CODEC
            // input == receiver output, CODEC output == modulator input).
            let (input_dev, output_dev) = if let Some(ref dev) = self.audio_device {
                info!(
                    "--audio-device override: '{}' for both input and output",
                    dev
                );
                (dev.clone(), dev.clone())
            } else {
                (
                    config.audio.input_device.clone(),
                    config.audio.output_device.clone(),
                )
            };
            let audio_config = AudioManagerConfig {
                input_device: Some(input_dev),
                output_device: Some(output_dev),
                sample_rate: config.audio.sample_rate,
                buffer_size: config.audio.buffer_size as usize,
                channels: config.audio.input_channels as u16,
                enable_monitoring: false,
                target_latency_ms: 1.0,
                input_gain_db: config.audio.levels.input_gain_db,
            };
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            // Live device-switch command channel into the audio thread. The TUI
            // `SelectDevice` handler sends an `AudioReopenRequest` here; the
            // thread reopens the cpal stream(s) on the new device(s) in place.
            let (reopen_tx, reopen_rx) = crossbeam_channel::unbounded::<AudioReopenRequest>();
            // docs/audio-robustness-plan.md item 4: the relay task's stale-
            // timeout arm below needs its own sender to self-trigger a
            // recovery reopen through the SAME channel + drain loop the
            // operator's TUI device picker uses — reusing that path means no
            // new cross-thread coordination is needed (the audio thread's
            // loop already serializes every reopen_devices call).
            let reopen_tx_watchdog = reopen_tx.clone();
            self.audio_reopen_tx = Some(reopen_tx);

            // Audio thread sends samples via a tokio mpsc to an async relay
            let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(100);

            // Audio runs on a dedicated std::thread (not tokio), but failures
            // need to surface to the operator via the TUI — silent log-only
            // failures were the root cause of "no decodes over SSH/tmux"
            // type reports. Capture the bus + runtime handle so the audio
            // thread can dispatch async sends without owning a runtime.
            let audio_bus = self.message_bus.clone();
            let runtime_handle = tokio::runtime::Handle::current();
            let audio_output_default = self.audio_output_default.clone();

            std::thread::spawn(move || {
                let report_audio_error = {
                    let bus = audio_bus.clone();
                    let rt = runtime_handle.clone();
                    move |msg: String| {
                        let bus = bus.clone();
                        rt.spawn(async move {
                            let err_msg = ComponentMessage::new(
                                ComponentId::Audio,
                                ComponentId::Tui,
                                MessageType::Error {
                                    component_id: ComponentId::Audio,
                                    error_message: msg,
                                    error_code: None,
                                },
                                Instant::now(),
                            );
                            let _ = bus.send_message(err_msg).await;
                        });
                    }
                };

                let mut audio_manager = match AudioManager::with_config(audio_config) {
                    Ok(manager) => manager,
                    Err(e) => {
                        let msg = format!("Audio init failed: {}", e);
                        error!("{}", msg);
                        report_audio_error(msg);
                        return;
                    }
                };

                if let Err(e) = audio_manager.start() {
                    let msg = format!("Audio stream start failed: {}", e);
                    error!("{}", msg);
                    report_audio_error(msg);
                    return;
                }

                info!("AudioManager started in dedicated thread");

                // TX-output misconfig: the resolved OUTPUT device fell back to
                // the system default rather than an explicit rig CODEC. This is
                // the classic "PTT keys the rig, but TX audio goes to the laptop
                // speakers" trap — previously only a log line. Surface it to the
                // operator via the TUI error path AND latch a flag the TUI uses
                // for a persistent station-panel badge.
                if audio_manager.output_is_system_default() {
                    audio_output_default.store(true, Ordering::Relaxed);
                    report_audio_error(
                        "TX audio is routed to the SYSTEM DEFAULT output (e.g. speakers), \
                         not an explicit rig CODEC. PTT will key the rig but no RF audio \
                         reaches it. Set [audio] output_device (run `pancetta test-audio --list`)."
                            .to_string(),
                    );
                } else {
                    audio_output_default.store(false, Ordering::Relaxed);
                }

                // Rate-limit recurring runtime errors so a wedged device
                // doesn't flood the TUI status with hundreds of identical
                // messages per second. Init errors above are unconditional
                // because they happen once.
                let mut last_runtime_report =
                    std::time::Instant::now() - std::time::Duration::from_secs(60);
                let runtime_report_min_gap = std::time::Duration::from_secs(5);
                let mut maybe_report_runtime = |kind: &str, e: String| {
                    if last_runtime_report.elapsed() >= runtime_report_min_gap {
                        report_audio_error(format!("Audio {}: {}", kind, e));
                        last_runtime_report = std::time::Instant::now();
                    }
                };

                // docs/audio-robustness-plan.md item 1: unlike report_audio_error
                // (an ephemeral MessageType::Error, meant for the TUI's
                // overwrite-prone status line), a "still retrying" recovery
                // state should be RETAINED so the operator can see it wasn't a
                // one-frame blip. Uses the DiagnosticEvent bus (PR #84).
                //
                // Named report_audio_diagnostic (not report_recovery_diagnostic)
                // because Task 4 reuses it for the periodic drop-rate
                // diagnostic — it is a generic level+target+text emitter, not
                // recovery-specific — target is a caller-supplied parameter
                // (not hardcoded) precisely so a later, unrelated diagnostic
                // doesn't get mislabeled under audio.recovery.
                let report_audio_diagnostic = {
                    let bus = audio_bus.clone();
                    let rt = runtime_handle.clone();
                    move |level: Level, target: &'static str, text: String| {
                        let bus = bus.clone();
                        let diagnostic_level = if level == Level::WARN {
                            pancetta_core::DiagnosticLevel::Warn
                        } else {
                            pancetta_core::DiagnosticLevel::Info
                        };
                        rt.spawn(async move {
                            let msg = ComponentMessage::new(
                                ComponentId::Audio,
                                ComponentId::Tui,
                                MessageType::DiagnosticEvent {
                                    target,
                                    level: diagnostic_level,
                                    text,
                                    qso_id: None,
                                    callsign: None,
                                },
                                Instant::now(),
                            );
                            let _ = bus.send_message(msg).await;
                        });
                    }
                };
                let mut recovery = crate::coordinator::audio_recovery::RecoveryBackoff::new();
                let mut last_recovery_report =
                    std::time::Instant::now() - std::time::Duration::from_secs(60);
                let recovery_report_min_gap = std::time::Duration::from_secs(10);
                let mut last_drop_report = std::time::Instant::now();
                let drop_report_interval = std::time::Duration::from_secs(30);

                loop {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }

                    // Live device switch: drain any reopen requests from the TUI
                    // picker. Performed here (on the thread that owns the cpal
                    // streams) so the non-Send streams never cross threads.
                    while let Ok(req) = reopen_rx.try_recv() {
                        let AudioReopenRequest {
                            input,
                            output,
                            force,
                            respond,
                        } = req;
                        let result = match audio_manager.reopen_devices(
                            input.as_deref(),
                            output.as_deref(),
                            force,
                        ) {
                            Ok(()) => {
                                info!(
                                    "Live audio device switch applied: in={:?} out={:?}",
                                    input, output
                                );
                                // Re-evaluate the TX-output-misconfig flag for
                                // the new output device so the TUI badge tracks
                                // the live selection.
                                audio_output_default.store(
                                    audio_manager.output_is_system_default(),
                                    Ordering::Relaxed,
                                );
                                Ok(())
                            }
                            Err(e) => {
                                let s = e.to_string();
                                error!("Live audio device switch failed: {}", s);
                                Err(s)
                            }
                        };
                        // The receiver may have dropped (e.g. TUI gone); ignore.
                        let _ = respond.send(result);
                    }

                    // Check for TX audio to play out
                    match tx_audio_rx.try_recv() {
                        Ok((samples, sample_rate)) => {
                            info!(
                                "Audio TX: queueing {} samples at {} Hz",
                                samples.len(),
                                sample_rate
                            );
                            if let Err(e) = audio_manager.queue_output(&samples, sample_rate) {
                                let s = e.to_string();
                                error!("Audio TX output error: {}", s);
                                maybe_report_runtime("TX output error", s);
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            info!("Audio TX channel disconnected");
                        }
                    }

                    match audio_manager.process_audio() {
                        Ok(Some(samples)) => {
                            if result_tx.blocking_send(samples).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(e) => {
                            let s = e.to_string();
                            error!("Audio processing error: {}", s);
                            maybe_report_runtime("processing error", s);

                            // docs/audio-robustness-plan.md item 1: auto-
                            // recovery. force=true is REQUIRED — with no new
                            // device name, resolve_reopen_targets treats
                            // input=None/output=None as "nothing requested"
                            // and reopen_devices no-ops without force (see
                            // the "Jump Desktop reclaim" case this same
                            // mechanism already handles for the operator
                            // picker). The first attempt after a fresh error
                            // (or after a prior success) fires with zero
                            // delay — a brief USB blip should recover on the
                            // very next loop iteration, not after a sleep.
                            let delay = recovery.next_delay();
                            if !delay.is_zero() {
                                std::thread::sleep(delay);
                            }
                            match audio_manager.reopen_devices(None, None, true) {
                                Ok(()) => {
                                    info!(
                                        "Audio auto-recovery: device reopened \
                                         successfully after {} attempt(s)",
                                        recovery.attempts()
                                    );
                                    if recovery.attempts() > 1 {
                                        report_audio_diagnostic(
                                            Level::INFO,
                                            "audio.recovery",
                                            "Audio device recovered".to_string(),
                                        );
                                    }
                                    recovery.reset();
                                }
                                Err(reopen_err) => {
                                    warn!(
                                        "Audio auto-recovery attempt {} failed: {}",
                                        recovery.attempts(),
                                        reopen_err
                                    );
                                    if last_recovery_report.elapsed() >= recovery_report_min_gap {
                                        report_audio_diagnostic(
                                            Level::WARN,
                                            "audio.recovery",
                                            format!(
                                                "Audio device lost — retrying \
                                                 (attempt {}): {}",
                                                recovery.attempts(),
                                                reopen_err
                                            ),
                                        );
                                        last_recovery_report = std::time::Instant::now();
                                    }
                                }
                            }
                        }
                    }

                    if last_drop_report.elapsed() >= drop_report_interval {
                        last_drop_report = std::time::Instant::now();
                        let dropped = audio_manager.dropped_samples();
                        if dropped > 0 {
                            let rate = audio_manager.drop_rate_percent();
                            let level = if rate > 1.0 { Level::WARN } else { Level::INFO };
                            report_audio_diagnostic(
                                level,
                                "audio.health",
                                format!(
                                    "Audio RX drop rate {:.2}% ({} samples dropped)",
                                    rate, dropped
                                ),
                            );
                        }
                    }
                }

                let _ = audio_manager.stop();
                info!("Audio manager thread stopped");
            });

            // Async relay: tokio mpsc -> crossbeam point-to-point
            let health_audio_alive_relay = health_audio_alive.clone();
            let handle = tokio::spawn(async move {
                let mut relay_count: u64 = 0;
                let mut stale_watchdog = crate::coordinator::audio_recovery::StaleWatchdog::new();
                // Record 90 seconds of raw 48kHz stereo audio for diagnostics
                // (covers ~6 FT8 windows regardless of boundary alignment)
                let raw_capture_samples = 48000 * 2 * 90; // 90s stereo
                let mut raw_recorder: Option<Vec<f32>> =
                    Some(Vec::with_capacity(raw_capture_samples));
                // docs/audio-robustness-plan.md item 3: `health_audio_alive`
                // was write-once-true — set on the first batch and NEVER
                // reset, so once ANY audio flowed the health log/TUI reported
                // "alive" forever even after the stream genuinely died (device
                // unplugged, stuck in the Err-backoff loop above). Batches
                // normally arrive every ~170ms (buffer_size sizing), so 2s
                // with no batch is comfortably past jitter and means the
                // stream actually stalled — flip the flag back to false.
                const AUDIO_STALE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
                loop {
                    let samples =
                        match tokio::time::timeout(AUDIO_STALE_TIMEOUT, result_rx.recv()).await {
                            Ok(Some(samples)) => {
                                stale_watchdog.on_data();
                                samples
                            }
                            Ok(None) => break, // sender dropped — audio thread stopped
                            Err(_elapsed) => {
                                health_audio_alive_relay.store(false, Ordering::Relaxed);
                                // docs/audio-robustness-plan.md item 4: a
                                // wedged device (callbacks stopped without a
                                // cpal StreamError) sets no flag anywhere
                                // today — process_audio() just keeps
                                // returning Ok(None) forever. Fire exactly
                                // once per stale episode (StaleWatchdog
                                // edge-detects this) rather than every 2s
                                // tick, since each signal is a real cpal
                                // teardown+rebuild on the audio thread.
                                if stale_watchdog.on_timeout() {
                                    warn!(
                                        "Audio watchdog: no samples for {:?} — \
                                         requesting a self-triggered device reopen",
                                        AUDIO_STALE_TIMEOUT
                                    );
                                    let (respond_tx, respond_rx) =
                                        tokio::sync::oneshot::channel();
                                    let sent = reopen_tx_watchdog.send(AudioReopenRequest {
                                        input: None,
                                        output: None,
                                        force: true,
                                        respond: respond_tx,
                                    });
                                    if sent.is_err() {
                                        warn!(
                                            "Audio watchdog: reopen channel closed, \
                                             cannot self-trigger recovery"
                                        );
                                    } else {
                                        tokio::spawn(async move {
                                            match respond_rx.await {
                                                Ok(Ok(())) => info!(
                                                    "Audio watchdog: self-triggered \
                                                     reopen succeeded"
                                                ),
                                                Ok(Err(e)) => warn!(
                                                    "Audio watchdog: self-triggered \
                                                     reopen failed: {}",
                                                    e
                                                ),
                                                Err(_) => {} // audio thread dropped the responder — shutting down
                                            }
                                        });
                                    }
                                }
                                continue;
                            }
                        };
                    // Perf (Pass 1 / A10): lock-free atomic store (was an async
                    // RwLock write on every audio batch — the hottest lock).
                    last_timestamp.store(super::now_epoch_ms(), Ordering::Relaxed);

                    // Capture raw 48kHz for diagnostic comparison
                    if let Some(ref mut buf) = raw_recorder {
                        buf.extend_from_slice(&samples);
                        if buf.len() >= raw_capture_samples {
                            let raw_path = dirs::home_dir()
                                .unwrap_or_default()
                                .join(".pancetta/recordings/raw_48khz_diagnostic.wav");
                            let spec = hound::WavSpec {
                                channels: 2,
                                sample_rate: 48000,
                                bits_per_sample: 16,
                                sample_format: hound::SampleFormat::Int,
                            };
                            if let Ok(mut w) = hound::WavWriter::create(&raw_path, spec) {
                                for &s in buf.iter() {
                                    let _ = w.write_sample((s * i16::MAX as f32) as i16);
                                }
                                let _ = w.finalize();
                                info!("Raw 48kHz diagnostic WAV saved: {} ({} samples, {:.0}s stereo)",
                                    raw_path.display(), buf.len() / 2, buf.len() as f64 / (48000.0 * 2.0));
                            }
                            raw_recorder = None; // only once
                        }
                    }

                    let len = samples.len();
                    if audio_to_dsp_tx.send(samples).is_err() {
                        info!(
                            "Audio relay: DSP channel closed after {} sends",
                            relay_count
                        );
                        break;
                    }
                    health_audio_alive_relay.store(true, Ordering::Relaxed);
                    relay_count += 1;
                    if relay_count == 1 {
                        info!("Audio relay: first batch sent ({} samples)", len);
                    } else if relay_count.is_multiple_of(1000) {
                        info!("Audio relay: {} batches sent so far", relay_count);
                    }
                }

                info!("Audio relay task stopped (total: {} batches)", relay_count);
                Ok(())
            });

            self.named_task_handles.push((ComponentId::Audio, handle));
        }

        info!("Audio component started");
        Ok(())
    }
}
