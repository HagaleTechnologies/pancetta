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

/// FT8 nominal pre-roll: audio starts 500ms past the slot boundary.
const DELAY_MS: u64 = 500;

/// Output of `schedule_tx`: where to TX, how much silence to pad in
/// front, and how far into the modulated waveform to start emitting.
#[derive(Debug, Clone, Copy)]
pub struct TxSchedule {
    /// UTC time of the slot boundary we're targeting.
    pub target_slot: chrono::DateTime<chrono::Utc>,
    /// Number of zero samples to emit before the modulated waveform.
    pub silent_pad_samples: usize,
    /// Sample offset into the waveform — caller emits `waveform[cursor..]`.
    pub cursor_offset_samples: usize,
}

/// WSJT-X-style late-start TX scheduler.
///
/// Picks the slot to TX in (current slot if parity matches and we're
/// within `tx_late_max_ms`, otherwise next slot of `required_parity`),
/// then decides how to align audio relative to that slot's boundary:
///
/// - **Early or just-arrived** (`mstr < DELAY_MS`): pad `(DELAY_MS - mstr)`
///   ms of zeros in front, emit the full 12.64s waveform starting at
///   slot+500ms (the FT8 pre-roll).
/// - **Late but viable** (`DELAY_MS <= mstr <= tx_late_max_ms`): skip
///   `(mstr - DELAY_MS)` ms into the waveform. WSJT-X's `m_ic` analogue.
/// - **Too late** (`mstr > tx_late_max_ms`): defer to the next slot of
///   the required parity (30s away), recompute as the early case.
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
) -> TxSchedule {
    use pancetta_core::slot::{current_slot_start, next_slot_with_parity, SlotParity};

    let cur_start = current_slot_start(now);
    let cur_parity = SlotParity::of(cur_start);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let target = if cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms {
        cur_start
    } else {
        next_slot_with_parity(now, required_parity)
    };

    // mstr relative to the chosen target. When target is in the future,
    // (now - target) is negative; clamp so we hit the early branch.
    let mstr_signed = (now - target).num_milliseconds();
    let mstr_unsigned = mstr_signed.max(0) as u64;

    let (silent_pad_ms, cursor_ms) = if mstr_unsigned < DELAY_MS {
        (DELAY_MS - mstr_unsigned, 0)
    } else {
        (0, mstr_unsigned - DELAY_MS)
    };

    TxSchedule {
        target_slot: target,
        silent_pad_samples: (silent_pad_ms as usize) * (sample_rate as usize) / 1000,
        cursor_offset_samples: (cursor_ms as usize) * (sample_rate as usize) / 1000,
    }
}

/// Sleep for `total` duration, but wake early (return `true`) if EITHER
/// the shutdown flag or the abort-current-tx flag flips while we wait.
/// Polled in 50ms chunks so worst-case wake latency is ~50ms.
///
/// Used inside the TX worker's per-message arm to guarantee both Ctrl-Q
/// (whole-app shutdown) and F8 (abort current TX, keep app running)
/// take effect within ~50ms. Without this, each `sleep().await` was
/// uninterruptible and the worker could continue driving PTT and audio
/// for ~13 seconds after the operator asked it to stop.
///
/// The caller checks `shutdown.load()` after wake to distinguish:
/// - shutdown set → break the outer worker loop
/// - shutdown clear, abort set → reset abort, send TransmitComplete
///   failure, `continue` to the next message
async fn interruptible_sleep(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    use std::sync::atomic::Ordering;
    if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
        return true;
    }
    let chunk = Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + total;
    while tokio::time::Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(remaining.min(chunk)).await;
    }
    false
}

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

/// Observer-only RAII guard for the TUI's TX-active badge (Batch 93).
///
/// Constructed right after `PttGuard` when PTT is asserted; its `Drop`
/// sends `MessageType::TxStatus { active: false }` to the TUI relay.
/// Because every exit from a TX arm — normal completion, operator abort
/// (F8 / Shift+Q `continue`), or shutdown `break` — drops the guard,
/// the badge clears on abort paths too, not just clean completion.
///
/// Strictly observational: it never touches PTT, audio, or scheduling.
/// The corresponding `active: true` is sent explicitly via
/// `send_tx_status` at PTT assert (async context is available there;
/// `Drop` is not async, hence the spawned fire-and-forget task).
struct TxStatusGuard {
    message_bus: MessageBus,
}

impl TxStatusGuard {
    fn new(message_bus: MessageBus) -> Self {
        Self { message_bus }
    }
}

impl Drop for TxStatusGuard {
    fn drop(&mut self) {
        let bus = self.message_bus.clone();
        let _ = tokio::task::spawn(async move {
            let msg = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::TxStatus { active: false },
                Instant::now(),
            );
            if let Err(e) = bus.send_message(msg).await {
                tracing::debug!("TxStatus(false) relay failed (no TUI?): {}", e);
            }
        });
    }
}

/// Notify the TUI of TX activity. Best-effort: failure (e.g. headless,
/// no TUI channel) is logged at debug and never affects the TX path.
async fn send_tx_status(message_bus: &MessageBus, active: bool) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxStatus { active },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("TxStatus({}) relay failed (no TUI?): {}", active, e);
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

        // Capture config snapshot for TX timing parameters.
        let (tx_late_max_ms, tx_self_parity, ptt_lead_ms, sample_rate) = {
            let cfg = self.config.read().await;
            (
                cfg.station.tx_late_max_ms,
                cfg.station.tx_self_parity,
                cfg.station.ptt_lead_ms,
                12000u32, // FT8 sample rate
            )
        };

        let tx_handle = {
            let shutdown = self.shutdown_signal.clone();
            let abort_current_tx = self.abort_current_tx.clone();

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

                while !shutdown.load(Ordering::Acquire) {
                    // Reset the per-message abort flag at the start of every
                    // try_recv cycle. Keeps a stale F8 from earlier (when no
                    // TX was in flight) from killing the next legitimate TX.
                    abort_current_tx.store(false, Ordering::Release);
                    match tx_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                MessageType::TransmitRequest {
                                    message_text,
                                    frequency_offset,
                                    qso_id,
                                    tx_parity,
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
                                    let (samples, _duration_ms) = match encoder
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

                                    // --- Step 2: Resolve required parity ---
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        chrono::Utc::now(),
                                    );

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                    );

                                    info!(
                                        "TX scheduled: parity={:?} target_slot={} pad={} samples cursor={} samples",
                                        required_parity,
                                        schedule.target_slot.format("%H:%M:%S%.3f UTC"),
                                        schedule.silent_pad_samples,
                                        schedule.cursor_offset_samples,
                                    );

                                    // --- Step 3: Build the audio buffer to ship ---
                                    // Pad zeros in front (early branch); skip cursor into
                                    // waveform (late branch); never both at the same time.
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        // Defensive: if cursor outran the waveform (shouldn't
                                        // happen because too-late defers), emit nothing and
                                        // skip TX.
                                        warn!("schedule_tx cursor exceeded waveform length; skipping TX");
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
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;

                                    // --- Step 4: Sleep until PTT engage instant ---
                                    let ptt_target_utc = schedule.target_slot
                                        - chrono::Duration::milliseconds(ptt_lead_ms as i64);
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_ptt, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted before PTT engage by shutdown");
                                            break;
                                        }
                                        info!("TX aborted before PTT engage by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 5: Assert PTT ---
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;
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
                                        warn!("PTT ON failed (rig not keyed): {} — if you are transmitting, TX audio may be going to the wrong device", e);
                                    }

                                    // --- Step 6: Sleep precisely until target slot start ---
                                    // (audio_out itself includes any silent_pad needed past
                                    // the slot boundary; we send it at the boundary.)
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_slot, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        // ptt_guard in scope — drop on `break`/`continue`
                                        // fires PTT-off either way.
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted between PTT and slot by shutdown");
                                            break;
                                        }
                                        info!("TX aborted between PTT and slot by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 7: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for audio playback to complete ---
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted during playback by shutdown");
                                            break;
                                        }
                                        info!("TX aborted during playback by operator (F8)");
                                        continue;
                                    }
                                    let success = true;
                                    let duration_ms = audio_duration_ms;

                                    // --- Step 9: De-assert PTT (with tail delay) ---
                                    if interruptible_sleep(
                                        Duration::from_millis(50),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted during tail by shutdown");
                                            break;
                                        }
                                        info!("TX aborted during tail by operator (F8)");
                                        continue;
                                    }
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
                                        warn!("PTT OFF failed (rig may be stuck in TX!): {}", e);
                                    }
                                    ptt_guard.disarm();

                                    // --- Step 10: Send TransmitComplete ---
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

                                MessageType::MultiTransmitRequest { items, tx_parity } => {
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

                                    let (samples_opt, _duration_ms_encode) = if !multi_items
                                        .is_empty()
                                    {
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

                                    // --- Step 2: Resolve required parity ---
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        chrono::Utc::now(),
                                    );

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                    );

                                    info!(
                                        "Multi-TX scheduled: parity={:?} target_slot={} pad={} samples cursor={} samples ({} items)",
                                        required_parity,
                                        schedule.target_slot.format("%H:%M:%S%.3f UTC"),
                                        schedule.silent_pad_samples,
                                        schedule.cursor_offset_samples,
                                        item_texts.len(),
                                    );

                                    // --- Step 3: Build the audio buffer ---
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform; skipping");
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
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;

                                    // --- Step 4: Sleep until PTT engage instant ---
                                    let ptt_target_utc = schedule.target_slot
                                        - chrono::Duration::milliseconds(ptt_lead_ms as i64);
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_ptt, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted before PTT by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted before PTT by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 5: Assert PTT ---
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;
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
                                        warn!("PTT ON failed (rig not keyed): {} — if you are transmitting, TX audio may be going to the wrong device", e);
                                    }

                                    // --- Step 6: Sleep precisely until target slot ---
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_slot, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!(
                                                "Multi-TX aborted between PTT and slot by shutdown"
                                            );
                                            break;
                                        }
                                        info!("Multi-TX aborted between PTT and slot by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 7: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for playback to complete ---
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted during playback by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted during playback by operator (F8)");
                                        continue;
                                    }
                                    let success = true;
                                    let duration_ms = audio_duration_ms;

                                    // --- Step 9: De-assert PTT (with tail delay) ---
                                    if interruptible_sleep(
                                        Duration::from_millis(50),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted during tail by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted during tail by operator (F8)");
                                        continue;
                                    }
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
                                        warn!("PTT OFF failed (rig may be stuck in TX!): {}", e);
                                    }
                                    ptt_guard.disarm();

                                    // --- Step 10: Send TransmitComplete for each item ---
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

                                MessageType::TuneRequest {
                                    duration_secs,
                                    tone_offset_hz,
                                } => {
                                    info!("Tune: {}s tone at {} Hz", duration_secs, tone_offset_hz);

                                    // Generate a continuous sine wave
                                    // (single tone, zero-bandwidth on air).
                                    // Amplitude 0.5 — operator manages rig
                                    // power. WSJT-X uses peak amplitude;
                                    // we run gentler so a forgotten rig
                                    // power setting is less likely to
                                    // overdrive.
                                    let n_samples =
                                        (duration_secs as usize) * (sample_rate as usize);
                                    let omega = 2.0 * std::f64::consts::PI * tone_offset_hz
                                        / sample_rate as f64;
                                    let tone_samples: Vec<f32> = (0..n_samples)
                                        .map(|i| ((i as f64) * omega).sin() as f32 * 0.5)
                                        .collect();
                                    let audio_duration_ms = (duration_secs as u64) * 1000;

                                    // Engage PTT immediately. No slot
                                    // scheduling: tune happens NOW.
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;
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
                                        warn!("Tune: PTT ON failed (rig not keyed): {}", e);
                                    }

                                    // Emit the audio buffer.
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: tone_samples,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Tune: audio output routing: {}", e);
                                    }

                                    // Wait for the duration. F4-toggle-off
                                    // and F8-halt both flip
                                    // abort_current_tx and wake the sleep
                                    // within 50ms; on wake, ptt_guard's
                                    // Drop fires PTT-off and we exit the
                                    // arm cleanly.
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Tune aborted by shutdown");
                                            break;
                                        }
                                        info!("Tune aborted by operator");
                                        // Drop ptt_guard via continue.
                                        continue;
                                    }

                                    // Natural completion: explicit PTT-off
                                    // (matches the regular TX path).
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
                                        warn!(
                                            "Tune: PTT OFF failed (rig may be stuck in TX!): {}",
                                            e
                                        );
                                    }
                                    ptt_guard.disarm();
                                    info!("Tune: complete");
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

/// Resolve the slot parity to use for a given TX request, falling back
/// to the configured self-parity (`Auto` picks whichever next slot is
/// closer to `now`).
pub fn resolve_required_parity(
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    now: chrono::DateTime<chrono::Utc>,
) -> pancetta_core::slot::SlotParity {
    use pancetta_config::station::TxSelfParity;
    use pancetta_core::slot::{next_slot_with_parity, SlotParity};
    if let Some(p) = tx_parity {
        return p;
    }
    match tx_self_parity {
        TxSelfParity::Even => SlotParity::Even,
        TxSelfParity::Odd => SlotParity::Odd,
        TxSelfParity::Auto => {
            let next_even = next_slot_with_parity(now, SlotParity::Even);
            let next_odd = next_slot_with_parity(now, SlotParity::Odd);
            if next_even <= next_odd {
                SlotParity::Even
            } else {
                SlotParity::Odd
            }
        }
    }
}

#[cfg(test)]
mod schedule_tx_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_core::slot::SlotParity;

    fn at(seconds: f64) -> chrono::DateTime<chrono::Utc> {
        // Reference: 2026-01-01 00:00:00 UTC. timestamp() = 1767225600,
        // divisible by 15. Slot 0 is Even (= 1767225600 / 15 % 2 = 0).
        let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        base + chrono::Duration::nanoseconds((seconds * 1_000_000_000.0) as i64)
    }

    #[test]
    fn early_pads_silent_no_skip() {
        // now = :05.0 (Even slot 0). Required = Odd. Current slot is Even
        // (wrong), so we advance to next Odd = :15. mstr_relative_to_target
        // = max(0, :05 - :15) = 0. 0 < 500 → pad 500ms, cursor 0.
        let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12); // 12 samples/ms at 12kHz
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn on_time_no_pad_no_skip() {
        // now = :15.500 (Odd slot 1). Required = Odd. Current slot matches;
        // mstr_in_current_slot = 500ms ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 500 = DELAY_MS → pad 0, cursor 0.
        let s = schedule_tx(at(15.5), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn late_skips_cursor_in_current_slot() {
        // now = :20.0 (Odd slot 1, 5s in). Required = Odd. Current matches;
        // mstr_in_current_slot = 5000 ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 5000 > 500 → cursor = 4500ms × SR.
        let s = schedule_tx(at(20.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 4500 * 12);
    }

    #[test]
    fn too_late_defers_to_next_opposite_slot() {
        // now = :24.5 (Odd slot 1, 9.5s in). Required = Odd. Current
        // matches but mstr_in_current_slot = 9500 > 8000 → too late;
        // advance to next Odd = :45. mstr_relative_to_target = 0 (target
        // is in future) → pad 500ms, cursor 0.
        let s = schedule_tx(at(24.5), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn collision_avoidance_does_not_pick_same_parity() {
        // now = :14.6 (Even slot 0, near end). DX on Even → required Odd.
        // Current parity Even ≠ Odd → advance to next Odd = :15.
        // mstr_relative_to_target = max(0, :14.6 - :15) = 0 → pad 500ms,
        // cursor 0. Most importantly: target is :15, NEVER :30 (the
        // collision case the original bug produced).
        let s = schedule_tx(at(14.6), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_ne!((s.target_slot - at(0.0)).num_milliseconds(), 30_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn at_exact_boundary_targets_current_slot() {
        // now = :15.000 exactly. Required = Odd. The :15 slot is Odd
        // and we're 0ms in — fully viable. Target the current slot,
        // pad 500ms before audio starts.
        let s = schedule_tx(at(15.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn current_slot_correct_parity_but_too_late_defers() {
        // now = :29.0 (Odd slot 1, 14s in — past tx_late_max_ms=8000).
        // Even though parity matches, we're too late for skip-ahead.
        // Defer to next Odd = :45.
        let s = schedule_tx(at(29.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Auto → returns Even
        let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Auto, at(5.0));
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_explicit_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Odd → tx_parity wins → Even
        let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Odd, at(5.0));
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_falls_back_to_config_when_none() {
        use pancetta_config::station::TxSelfParity;
        let p = resolve_required_parity(None, TxSelfParity::Even, at(5.0));
        assert_eq!(p, SlotParity::Even);
        let p = resolve_required_parity(None, TxSelfParity::Odd, at(5.0));
        assert_eq!(p, SlotParity::Odd);
    }

    #[test]
    fn resolve_required_parity_auto_picks_nearest_next_slot() {
        use pancetta_config::station::TxSelfParity;
        // now = :05 (in Even slot 0). Next Even = :30, next Odd = :15.
        // Odd is closer → Auto picks Odd.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0));
        assert_eq!(p, SlotParity::Odd);
        // now = :20 (in Odd slot 1). Next Odd = :45, next Even = :30.
        // Even is closer → Auto picks Even.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(20.0));
        assert_eq!(p, SlotParity::Even);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_completes_when_no_shutdown() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let interrupted = interruptible_sleep(Duration::from_millis(120), &shutdown, &abort).await;
        assert!(
            !interrupted,
            "should not flag interrupted when both flags stay false"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_returns_immediately_if_already_shutdown() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(true));
        let abort = Arc::new(AtomicBool::new(false));
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        assert!(
            interrupted,
            "must signal interrupted when shutdown is already set"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "should return without sleeping (elapsed={:?})",
            elapsed
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_returns_immediately_if_already_aborted() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(true));
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        assert!(
            interrupted,
            "must signal interrupted when abort is already set"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "should return without sleeping (elapsed={:?})",
            elapsed
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_wakes_within_one_chunk_when_shutdown_fires() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let s2 = shutdown.clone();
        // After 200ms, flip the shutdown flag.
        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            s2.store(true, Ordering::Release);
        });
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(30), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        let _ = setter.await;
        assert!(
            interrupted,
            "should signal interrupted when flag flips mid-sleep"
        );
        // Polling chunk is 50ms, so wake latency from flag flip is at most one chunk.
        // Total elapsed ≈ 200ms (when flag flipped) + ≤ 50ms (chunk poll) = ≤ 250ms.
        // Allow 300ms slack for test-runner jitter.
        assert!(
            elapsed < Duration::from_millis(300),
            "wake latency exceeded one chunk (elapsed={:?})",
            elapsed
        );
    }
}
