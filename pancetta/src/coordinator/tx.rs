//! FT8 transmitter component.
//!
//! Owns the encode → modulate → slot-aligned audio output → PTT key/unkey
//! sequence. Runs on the message-bus channel for `MessageType::TransmitRequest`
//! arriving from the QSO state machine, the autonomous operator, or the
//! TUI command-forwarding loop.
//!
//! The `PttGuard` RAII helper ensures the radio is keyed back to RX even
//! if the transmitter task is cancelled mid-transmission — without it a
//! panic in the audio output path would leave the rig stuck on TX.

use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Guard that sends PTT-off when dropped, ensuring PTT is released
/// even if the transmitter task is cancelled mid-transmission.
struct PttGuard {
    message_bus: MessageBus,
    armed: bool,
}

impl PttGuard {
    fn new(message_bus: MessageBus) -> Self {
        Self {
            message_bus,
            armed: true,
        }
    }

    /// Disarm the guard after PTT-off has been sent normally.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        if self.armed {
            let bus = self.message_bus.clone();
            // Spawn a fire-and-forget task to send PTT-off.
            // This runs even if the parent task was cancelled.
            let _ = tokio::task::spawn(async move {
                let ptt_off_msg = ComponentMessage::new(
                    ComponentId::Ft8Transmitter,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: false,
                    }),
                    Instant::now(),
                );
                if let Err(e) = bus.send_message(ptt_off_msg).await {
                    tracing::error!("PTT GUARD: failed to force PTT off on drop: {}", e);
                } else {
                    tracing::warn!("PTT GUARD: forced PTT off due to task cancellation");
                }
            });
        }
    }
}

impl super::ApplicationCoordinator {
    /// Start FT8 transmitter component
    pub(crate) async fn start_transmitter_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_transmitter");
        let _enter = span.enter();

        info!("Starting FT8 transmitter component");

        let (_tx_sender, tx_rx) = self
            .message_bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await?;
        let message_bus = self.message_bus.clone();

        let tx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                info!("FT8 transmitter component ready");

                let mut encoder = Ft8Encoder::new();
                let mut modulator = match Ft8Modulator::new_default() {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Failed to create modulator: {}", e);
                        return Err(anyhow::anyhow!("Modulator init failed: {}", e));
                    }
                };

                // FT8 transmissions start at slot+500ms (the 0.5s pre-roll
                // convention). We engage PTT 200ms earlier so the relay is
                // settled when audio begins. Require at least 1s of total
                // lead so we have headroom for both.
                const PTT_LEAD: chrono::Duration = chrono::Duration::milliseconds(200);
                const MIN_LEAD: chrono::Duration = chrono::Duration::seconds(1);

                while !shutdown.load(Ordering::Acquire) {
                    match tx_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                MessageType::TransmitRequest {
                                    message_text,
                                    frequency_offset,
                                    qso_id,
                                } => {
                                    info!(
                                        "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                        message_text, frequency_offset, qso_id
                                    );

                                    // --- Step 1: Encode + modulate up front ---
                                    // Do this BEFORE any timing-critical work so encoding
                                    // latency can't push us past the slot boundary.
                                    //
                                    // TransmitRequest.frequency_offset is the ABSOLUTE audio
                                    // frequency in Hz (200-4000), not a delta. The modulator
                                    // adds its base_frequency to whatever we pass to
                                    // modulate_symbols, so to honor the request we set the
                                    // base to the requested frequency and pass 0 as the
                                    // additional offset.
                                    if let Err(e) = modulator.set_base_frequency(frequency_offset) {
                                        warn!(
                                            "Invalid TX frequency {} Hz for '{}': {}",
                                            frequency_offset, message_text, e
                                        );
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
                                    }
                                    let (samples, duration_ms) = match encoder
                                        .encode_message(&message_text, None)
                                        .and_then(|symbols| {
                                            modulator.modulate_symbols(&symbols, 0.0)
                                        }) {
                                        Ok(s) => {
                                            let dur = (s.len() as f64 / 12000.0 * 1000.0) as u64;
                                            info!(
                                                "TX: '{}' -> {} samples ({:.2}s)",
                                                message_text,
                                                s.len(),
                                                dur as f64 / 1000.0
                                            );
                                            (s, dur)
                                        }
                                        Err(e) => {
                                            warn!(
                                                "Encode/modulate failed for '{}': {}",
                                                message_text, e
                                            );
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue;
                                        }
                                    };

                                    // --- Step 2: Compute precise FT8 audio-start time ---
                                    let target_audio_utc = pancetta_core::slot::next_audio_start(
                                        chrono::Utc::now(),
                                        MIN_LEAD,
                                    );
                                    info!(
                                        "TX scheduled: audio at {} (PTT 200ms earlier)",
                                        target_audio_utc.format("%H:%M:%S%.3f UTC")
                                    );

                                    // --- Step 3: Sleep until PTT engage instant (audio - 200ms) ---
                                    let ptt_target_utc = target_audio_utc - PTT_LEAD;
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_ptt).await;

                                    // --- Step 4: Assert PTT ---
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        debug!("PTT on failed (no rig?): {}", e);
                                    }

                                    // --- Step 5: Sleep precisely until audio-start instant ---
                                    let to_audio = pancetta_core::slot::duration_until(
                                        target_audio_utc,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_audio).await;

                                    // --- Step 6: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples,
                                            sample_rate: 12000,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 7: Wait for audio playback to complete ---
                                    sleep(Duration::from_millis(duration_ms)).await;
                                    let success = true;

                                    // --- Step 8: De-assert PTT (with tail delay) ---
                                    sleep(Duration::from_millis(50)).await;
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        debug!("PTT off failed (no rig?): {}", e);
                                    }
                                    ptt_guard.disarm();

                                    // --- Step 6: Send TransmitComplete ---
                                    let complete_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Autonomous,
                                        MessageType::TransmitComplete {
                                            success,
                                            message_text,
                                            duration_ms,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(complete_msg).await {
                                        warn!("Failed to send TransmitComplete: {}", e);
                                    }
                                }

                                MessageType::MultiTransmitRequest { items } => {
                                    info!("Multi-TX request: {} messages", items.len());

                                    // --- Step 1: Encode + modulate up front ---
                                    let ft8_params = pancetta_ft8::ProtocolParams::ft8();
                                    let mut symbol_sets: Vec<Vec<u8>> = Vec::new();
                                    let mut item_texts: Vec<String> = Vec::new();

                                    for item in &items {
                                        match encoder.encode_message(&item.message_text, None) {
                                            Ok(symbols) => {
                                                item_texts.push(item.message_text.clone());
                                                symbol_sets.push(symbols.to_vec());
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Encoding failed for '{}': {}",
                                                    item.message_text, e
                                                );
                                            }
                                        }
                                    }

                                    // TransmitRequestItem.frequency_offset is the ABSOLUTE
                                    // audio frequency (matching TransmitRequest semantics).
                                    // modulate_multi_tx wants per-item OFFSETS from a shared
                                    // base. Use base=200 (lowest valid) so any audio frequency
                                    // in the 200-2500 Hz FT8 passband maps to a non-negative
                                    // per-item offset.
                                    const MULTI_TX_BASE_HZ: f64 = 200.0;
                                    let mut multi_items = Vec::new();
                                    for (i, symbols) in symbol_sets.iter().enumerate() {
                                        multi_items.push(pancetta_ft8::MultiTxItem {
                                            symbols: symbols.as_slice(),
                                            frequency_offset: items[i].frequency_offset
                                                - MULTI_TX_BASE_HZ,
                                            params: &ft8_params,
                                        });
                                    }

                                    let (samples_opt, duration_ms) = if !multi_items.is_empty() {
                                        match pancetta_ft8::modulate_multi_tx(
                                            &multi_items,
                                            12000,
                                            MULTI_TX_BASE_HZ,
                                            0.5,
                                        ) {
                                            Ok(samples) => {
                                                let dur = (samples.len() as f64 / 12000.0 * 1000.0)
                                                    as u64;
                                                info!(
                                                    "Multi-TX: {} messages -> {} samples ({:.2}s)",
                                                    multi_items.len(),
                                                    samples.len(),
                                                    dur as f64 / 1000.0
                                                );
                                                (Some(samples), dur)
                                            }
                                            Err(e) => {
                                                warn!("Multi-TX modulation failed: {}", e);
                                                (None, 0)
                                            }
                                        }
                                    } else {
                                        (None, 0)
                                    };

                                    let samples = match samples_opt {
                                        Some(s) => s,
                                        None => {
                                            for text in item_texts {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: text,
                                                        duration_ms: 0,
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
                                            }
                                            continue;
                                        }
                                    };

                                    // --- Step 2: Compute precise FT8 audio-start time ---
                                    let target_audio_utc = pancetta_core::slot::next_audio_start(
                                        chrono::Utc::now(),
                                        MIN_LEAD,
                                    );
                                    info!(
                                        "Multi-TX scheduled: audio at {} (PTT 200ms earlier)",
                                        target_audio_utc.format("%H:%M:%S%.3f UTC")
                                    );

                                    // --- Step 3: Sleep until PTT engage (audio - 200ms) ---
                                    let ptt_target_utc = target_audio_utc - PTT_LEAD;
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_ptt).await;

                                    // --- Step 4: Assert PTT ---
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        debug!("PTT on failed (no rig?): {}", e);
                                    }

                                    // --- Step 5: Sleep precisely until audio-start ---
                                    let to_audio = pancetta_core::slot::duration_until(
                                        target_audio_utc,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_audio).await;

                                    // --- Step 6: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples,
                                            sample_rate: 12000,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 7: Wait for playback to complete ---
                                    sleep(Duration::from_millis(duration_ms)).await;
                                    let success = true;

                                    // --- Step 8: De-assert PTT (with tail delay) ---
                                    sleep(Duration::from_millis(50)).await;
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        debug!("PTT off failed (no rig?): {}", e);
                                    }
                                    ptt_guard.disarm();

                                    // --- Step 6: Send TransmitComplete for each item ---
                                    for text in item_texts {
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success,
                                                message_text: text,
                                                duration_ms,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(complete_msg).await
                                        {
                                            warn!("Failed to send TransmitComplete: {}", e);
                                        }
                                    }
                                }

                                _ => {} // Ignore other message types
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                info!("FT8 transmitter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Ft8Transmitter, tx_handle));
        info!("FT8 transmitter component started");
        Ok(())
    }
}
