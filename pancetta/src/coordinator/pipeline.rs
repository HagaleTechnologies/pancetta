use anyhow::Result;
use geographiclib_rs::InverseGeodesic;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ForwardOutcome {
    Sent,
    Dropped,
    Disconnected,
}

pub(crate) fn forward_or_drop<T>(
    tx: &crossbeam_channel::Sender<T>,
    item: T,
    timeout: Duration,
) -> ForwardOutcome {
    match tx.send_timeout(item, timeout) {
        Ok(()) => ForwardOutcome::Sent,
        Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => ForwardOutcome::Dropped,
        Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => ForwardOutcome::Disconnected,
    }
}

pub(crate) const DECODE_FORWARD_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) struct DecodePipelineHandles {
    pub(crate) audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
    pub(crate) audio_to_dsp_rx: crossbeam_channel::Receiver<Vec<f32>>,
    pub(crate) dsp_to_ft8_tx: crossbeam_channel::Sender<Vec<f32>>,
    pub(crate) dsp_to_ft8_rx: crossbeam_channel::Receiver<Vec<f32>>,
    pub(crate) ft8_to_tui_tx: crossbeam_channel::Sender<pancetta_ft8::DecodedMessage>,
    ft8_to_tui_rx: Option<crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>>,
    pub(crate) waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
    waterfall_rx: Option<crossbeam_channel::Receiver<Vec<Vec<f32>>>>,
    pub(crate) audio_level_tx: crossbeam_channel::Sender<f32>,
    audio_level_rx: Option<crossbeam_channel::Receiver<f32>>,
    pub(crate) health_dsp_windows: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) health_last_rms: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) health_total_decodes: Arc<std::sync::atomic::AtomicU64>,
}

impl DecodePipelineHandles {
    pub(crate) fn new() -> Self {
        let (audio_to_dsp_tx, audio_to_dsp_rx) = crossbeam_channel::bounded(100);
        let (dsp_to_ft8_tx, dsp_to_ft8_rx) = crossbeam_channel::bounded(2);
        let (ft8_to_tui_tx, ft8_to_tui_rx) = crossbeam_channel::bounded(500);
        let (waterfall_tx, waterfall_rx) = crossbeam_channel::bounded(100);
        let (audio_level_tx, audio_level_rx) = crossbeam_channel::bounded(1);
        Self {
            audio_to_dsp_tx,
            audio_to_dsp_rx,
            dsp_to_ft8_tx,
            dsp_to_ft8_rx,
            ft8_to_tui_tx,
            ft8_to_tui_rx: Some(ft8_to_tui_rx),
            waterfall_tx,
            waterfall_rx: Some(waterfall_rx),
            audio_level_tx,
            audio_level_rx: Some(audio_level_rx),
            health_dsp_windows: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            health_last_rms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            health_total_decodes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn take_terminal_receivers(
        &mut self,
    ) -> Result<(
        crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>,
        crossbeam_channel::Receiver<Vec<Vec<f32>>>,
        crossbeam_channel::Receiver<f32>,
    )> {
        Ok((
            self.ft8_to_tui_rx
                .take()
                .ok_or_else(|| anyhow::anyhow!("FT8 terminal receiver already taken"))?,
            self.waterfall_rx
                .take()
                .ok_or_else(|| anyhow::anyhow!("waterfall terminal receiver already taken"))?,
            self.audio_level_rx
                .take()
                .ok_or_else(|| anyhow::anyhow!("audio-level terminal receiver already taken"))?,
        ))
    }
}

pub(crate) async fn register_decode_bus_channels(
    bus: &crate::message_bus::MessageBus,
) -> Result<()> {
    let _ = bus.get_or_create_channel(ComponentId::Dsp).await?;
    let _ = bus.get_or_create_channel(ComponentId::Ft8Decoder).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;

    #[tokio::test]
    async fn decode_bus_channels_register_idempotently() {
        let bus = MessageBus::new(64).expect("bus construction");
        register_decode_bus_channels(&bus)
            .await
            .expect("first registration should succeed");
        register_decode_bus_channels(&bus)
            .await
            .expect("re-registration must not error");
    }

    #[test]
    fn forward_or_drop_times_out_instead_of_blocking_when_full() {
        let (tx, _rx) = crossbeam_channel::bounded::<u8>(1);
        assert_eq!(
            forward_or_drop(&tx, 1, Duration::from_millis(10)),
            ForwardOutcome::Sent
        );
        let started = Instant::now();
        assert_eq!(
            forward_or_drop(&tx, 2, Duration::from_millis(10)),
            ForwardOutcome::Dropped
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn forward_or_drop_reports_disconnected_when_all_receivers_dropped() {
        let (tx, rx) = crossbeam_channel::bounded::<u8>(1);
        drop(rx);
        assert_eq!(
            forward_or_drop(&tx, 1, Duration::from_millis(10)),
            ForwardOutcome::Disconnected
        );
    }

    #[test]
    fn retained_decode_handles_outlive_a_dead_stage() {
        let handles = DecodePipelineHandles::new();
        drop((
            handles.audio_to_dsp_rx.clone(),
            handles.dsp_to_ft8_tx.clone(),
        ));
        handles.audio_to_dsp_tx.send(vec![1.0; 4]).unwrap();
        assert_eq!(handles.audio_to_dsp_rx.recv().unwrap(), vec![1.0; 4]);
        handles.dsp_to_ft8_tx.send(vec![0.0; 4]).unwrap();
        assert!(handles.dsp_to_ft8_rx.recv().is_ok());
    }
}

impl super::ApplicationCoordinator {
    /// Start the core pipeline with proper point-to-point channels.
    ///
    /// Creates direct crossbeam channels between components:
    ///   audio_tx -> dsp_rx  (raw audio)
    ///   dsp_tx   -> ft8_rx  (processed windows)
    ///   ft8_tx   -> tui_rx  (decoded messages)
    pub(crate) async fn start_pipeline(&mut self) -> Result<()> {
        self.init_decode_handles();
        let (ft8_to_tui_rx, waterfall_rx, audio_level_rx) = self
            .decode_handles
            .as_mut()
            .expect("decode handles initialized")
            .take_terminal_receivers()?;
        let handles = self.decode_handles()?;
        let audio_to_dsp_tx = handles.audio_to_dsp_tx.clone();
        let health_dsp_windows = handles.health_dsp_windows.clone();
        let health_total_decodes = handles.health_total_decodes.clone();
        let health_last_rms = handles.health_last_rms.clone();

        // TX audio channel: Ft8Transmitter -> Audio thread for playback
        let (tx_audio_tx, tx_audio_rx) = crossbeam_channel::bounded::<(Vec<f32>, u32, bool)>(4);

        // Pipeline health tracking (atomics shared across threads)
        let health_audio_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let ft8lib_native = pancetta_ft8::ft8lib_is_available();
        info!(
            "Pipeline starting: ft8_lib={}, audio_device={}",
            if ft8lib_native {
                "native-C"
            } else {
                "stub (pure-Rust only)"
            },
            if self.headless { "stub" } else { "real" },
        );
        if !ft8lib_native {
            warn!(
                "ft8_lib C decoder NOT compiled in (ft8lib_stub) — decode recall is degraded. \
                 Fix: git submodule update --init && cargo build --release"
            );
        }

        // Also create message bus channels for control messages (hamlib, autonomous, etc.)
        let (_audio_bus_tx, audio_bus_rx) =
            self.message_bus.create_channel(ComponentId::Audio).await?;
        register_decode_bus_channels(&self.message_bus).await?;
        let (_tui_bus_tx, tui_bus_rx) = self.message_bus.create_channel(ComponentId::Tui).await?;

        if !ft8lib_native {
            let msg = ComponentMessage::new(
                ComponentId::Ft8Decoder,
                ComponentId::Tui,
                MessageType::DiagnosticEvent {
                    target: "decode.engine",
                    level: pancetta_core::DiagnosticLevel::Warn,
                    text:
                        "ft8_lib C decoder not compiled in (stub build) — decode recall degraded. \
                           Fix: git submodule update --init, then rebuild."
                            .to_string(),
                    qso_id: None,
                    callsign: None,
                },
                Instant::now(),
            );
            if let Err(e) = self.message_bus.send_message(msg).await {
                tracing::debug!("stub-build DiagnosticEvent relay failed (no TUI?): {e}");
            }
        }

        // --- Audio component ---
        self.start_audio_pipeline(audio_to_dsp_tx, tx_audio_rx, health_audio_alive.clone())
            .await?;

        // --- Audio TX relay: message bus AudioOutput -> audio thread ---
        {
            let shutdown = self.shutdown_signal.clone();
            let handle = tokio::spawn(async move {
                info!("Audio TX relay started");
                while !shutdown.load(Ordering::Acquire) {
                    match audio_bus_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::AudioOutput {
                                samples,
                                sample_rate,
                                flush_first,
                            } = message.message_type
                            {
                                info!(
                                    "Audio TX relay: {} samples at {} Hz from {:?}",
                                    samples.len(),
                                    sample_rate,
                                    message.source
                                );
                                if tx_audio_tx
                                    .send((samples, sample_rate, flush_first))
                                    .is_err()
                                {
                                    warn!("Audio TX relay: audio thread channel closed");
                                    break;
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }
                info!("Audio TX relay stopped");
                Ok(())
            });
            self.named_task_handles.push((ComponentId::Audio, handle));
        }

        // --- DSP component ---
        self.start_dsp_pipeline().await?;

        // --- FT8 decoder component ---
        self.start_ft8_pipeline().await?;

        // --- TUI component ---
        if !self.headless {
            self.start_tui_pipeline(
                ft8_to_tui_rx,
                tui_bus_rx,
                waterfall_rx,
                audio_level_rx,
                health_audio_alive.clone(),
                health_dsp_windows.clone(),
                health_last_rms.clone(),
                health_total_decodes.clone(),
            )
            .await?;
        } else {
            // In headless mode, drain decoded messages / waterfall and log health
            let shutdown = self.shutdown_signal.clone();
            let health_audio_alive_hl = health_audio_alive.clone();
            let health_dsp_windows_hl = health_dsp_windows.clone();
            let health_total_decodes_hl = health_total_decodes.clone();
            let handle = tokio::spawn(async move {
                let mut last_health_log = Instant::now();
                while !shutdown.load(Ordering::Acquire) {
                    // Drain decoded messages
                    match ft8_to_tui_rx.try_recv() {
                        Ok(msg) => {
                            info!(
                                "Decoded: {} (SNR: {:.0}, freq: {:.1} Hz)",
                                msg.text, msg.snr_db, msg.frequency_offset
                            );
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }

                    // Drain waterfall to prevent unbounded growth
                    while waterfall_rx.try_recv().is_ok() {}

                    // Periodic health logging (every 60 seconds)
                    if last_health_log.elapsed() >= Duration::from_secs(60) {
                        info!(
                            "Pipeline health: ft8_lib={}, dsp_windows={}, total_decodes={}, audio={}",
                            if pancetta_ft8::ft8lib_is_available() { "C" } else { "stub" },
                            health_dsp_windows_hl.load(Ordering::Relaxed),
                            health_total_decodes_hl.load(Ordering::Relaxed),
                            if health_audio_alive_hl.load(Ordering::Relaxed) { "alive" } else { "no-data" },
                        );
                        last_health_log = Instant::now();
                    }

                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(())
            });
            self.named_task_handles.push((ComponentId::Tui, handle));
        }

        Ok(())
    }
}
