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

impl super::ApplicationCoordinator {
    /// Start the core pipeline with proper point-to-point channels.
    ///
    /// Creates direct crossbeam channels between components:
    ///   audio_tx -> dsp_rx  (raw audio)
    ///   dsp_tx   -> ft8_rx  (processed windows)
    ///   ft8_tx   -> tui_rx  (decoded messages)
    pub(crate) async fn start_pipeline(&mut self) -> Result<()> {
        // Point-to-point channels for the data path
        let (audio_to_dsp_tx, audio_to_dsp_rx) = crossbeam_channel::bounded::<Vec<f32>>(100);
        let (dsp_to_ft8_tx, dsp_to_ft8_rx) = crossbeam_channel::bounded::<Vec<f32>>(2);
        let (ft8_to_tui_tx, ft8_to_tui_rx) =
            crossbeam_channel::bounded::<pancetta_ft8::DecodedMessage>(500);
        let (waterfall_tx, waterfall_rx) = crossbeam_channel::bounded::<Vec<Vec<f32>>>(100);
        let (audio_level_tx, audio_level_rx) = crossbeam_channel::bounded::<f32>(1);

        // TX audio channel: Ft8Transmitter -> Audio thread for playback
        let (tx_audio_tx, tx_audio_rx) = crossbeam_channel::bounded::<(Vec<f32>, u32)>(4);

        // Pipeline health tracking (atomics shared across threads)
        let health_dsp_windows = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let health_total_decodes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let health_last_rms = Arc::new(std::sync::atomic::AtomicU32::new(0)); // f32 bits
        let health_audio_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));

        info!(
            "Pipeline starting: ft8_lib={}, audio_device={}",
            if pancetta_ft8::ft8lib_is_available() {
                "native-C"
            } else {
                "stub (pure-Rust only)"
            },
            if self.headless { "stub" } else { "real" },
        );

        // Also create message bus channels for control messages (hamlib, autonomous, etc.)
        let (_audio_bus_tx, audio_bus_rx) =
            self.message_bus.create_channel(ComponentId::Audio).await?;
        let (_dsp_bus_tx, _dsp_bus_rx) = self.message_bus.create_channel(ComponentId::Dsp).await?;
        let (_ft8_bus_tx, _ft8_bus_rx) = self
            .message_bus
            .create_channel(ComponentId::Ft8Decoder)
            .await?;
        let (_tui_bus_tx, tui_bus_rx) = self.message_bus.create_channel(ComponentId::Tui).await?;

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
                            } = message.message_type
                            {
                                info!(
                                    "Audio TX relay: {} samples at {} Hz from {:?}",
                                    samples.len(),
                                    sample_rate,
                                    message.source
                                );
                                if tx_audio_tx.send((samples, sample_rate)).is_err() {
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
        self.start_dsp_pipeline(
            audio_to_dsp_rx,
            dsp_to_ft8_tx,
            waterfall_tx.clone(),
            audio_level_tx,
            health_dsp_windows.clone(),
            health_last_rms.clone(),
        )
        .await?;

        // --- FT8 decoder component ---
        self.start_ft8_pipeline(
            dsp_to_ft8_rx,
            ft8_to_tui_tx,
            waterfall_tx,
            health_total_decodes.clone(),
        )
        .await?;

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
