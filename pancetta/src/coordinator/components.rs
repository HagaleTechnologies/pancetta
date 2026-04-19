use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep};
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
    /// Start QSO management component
    ///
    /// Wires decoded FT8 messages into the QSO manager for state tracking,
    /// auto-logging to SQLite at `~/.pancetta/qso.db`, and duplicate detection.
    pub(crate) async fn start_qso_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_qso");
        let _enter = span.enter();

        info!("Starting QSO component");

        let (_qso_tx, qso_rx) = self.message_bus.create_channel(ComponentId::Qso).await?;
        let message_bus = self.message_bus.clone();

        // Read station config for callsign/grid
        let config = self.config.read().await;
        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };
        drop(config);

        let qso_lookup = self.cached_lookup.clone();
        let cqdx_bridge = self.cqdx_bridge.clone();
        let active_qso_ap = self.active_qso_ap.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{LoggerConfig, QsoLogger, QsoManager, QsoManagerConfig};

                let qso_config = QsoManagerConfig {
                    our_callsign: our_callsign.clone(),
                    our_grid: our_grid.clone(),
                    ..Default::default()
                };

                let qso_manager = QsoManager::new(qso_config);
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }

                // Initialize QSO logger with SQLite database at ~/.pancetta/qso.db
                let db_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qso.db");
                let logger_config = LoggerConfig {
                    database_path: db_path.clone(),
                    ..Default::default()
                };

                let _logger = match QsoLogger::new(logger_config, qso_manager.clone()).await {
                    Ok(l) => {
                        info!("QSO logger initialized with database at {:?}", db_path);
                        let l = std::sync::Arc::new(l);
                        if let Err(e) = l.start().await {
                            warn!("QSO logger background tasks failed to start: {}", e);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize QSO logger (continuing without): {}",
                            e
                        );
                        None
                    }
                };

                // Seed worked-station history from the QSO database so that
                // previously-worked stations are recognised as duplicates across restarts.
                {
                    use pancetta_qso::QsoDatabase;

                    // Determine the current band from the rig's operating frequency,
                    // falling back to "20m".  This is a best-effort seed — the
                    // autonomous operator will always re-validate against the live
                    // worked-on-band set as QSOs complete.
                    let freq_hz = operating_frequency_hz.load(std::sync::atomic::Ordering::Relaxed);
                    let band = pancetta_cqdx::frequency_to_band(freq_hz)
                        .unwrap_or_else(|| "20m".to_string())
                        .to_uppercase();

                    match QsoDatabase::open(&db_path) {
                        Ok(db) => {
                            let callsigns = db.get_worked_callsigns(&band);
                            if callsigns.is_empty() {
                                tracing::info!(
                                    "QSO database has no prior contacts on {} — starting fresh",
                                    band
                                );
                            } else {
                                qso_lookup.seed_worked_from_list(&band, callsigns);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Could not open QSO database for startup seed ({}): {} — \
                                 previously-worked stations will not be detected as duplicates \
                                 until re-worked this session",
                                db_path.display(),
                                e
                            );
                        }
                    }
                }

                info!(
                    "QSO component ready (callsign={}, grid={:?})",
                    our_callsign, our_grid
                );

                // Spawn a task to forward QSO auto-sequence TX requests to the transmitter
                // and update AP decoding state for the FT8 decoder thread.
                let mut qso_events = qso_manager.subscribe();
                let tx_bus = message_bus.clone();
                let tx_shutdown = shutdown.clone();
                let tx_callsign = our_callsign.clone();
                let ap_state = active_qso_ap;
                tokio::spawn(async move {
                    while !tx_shutdown.load(Ordering::Acquire) {
                        match qso_events.recv().await {
                            Ok(pancetta_qso::QsoEvent::StateChanged { new_state, .. }) => {
                                // Map QSO state to AP context for AP3/AP4 decoding
                                let new_ap = match &new_state {
                                    pancetta_qso::QsoState::RespondingToCq {
                                        target_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::WaitingForReport {
                                        their_callsign: target_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::SendingReport {
                                        their_callsign: target_callsign,
                                        ..
                                    } => pancetta_ft8::QsoAp::new(
                                        target_callsign,
                                        pancetta_ft8::QsoApProgress::WaitingForReport,
                                    ),
                                    pancetta_qso::QsoState::WaitingForConfirmation {
                                        their_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::SendingConfirmation {
                                        their_callsign,
                                        ..
                                    } => pancetta_ft8::QsoAp::new(
                                        their_callsign,
                                        pancetta_ft8::QsoApProgress::WaitingForConfirmation,
                                    ),
                                    // Terminal or idle states clear the AP context
                                    _ => None,
                                };
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = new_ap;
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                qso_id,
                                message,
                                frequency,
                            }) => {
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={})",
                                            text, frequency, qso_id
                                        );
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: text,
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = tx_bus.send_message(tx_msg).await {
                                            warn!("Failed to send auto-sequence TX: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to generate FT8 message for QSO {}: {}",
                                            qso_id, e
                                        );
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                                // Clear AP state on QSO completion
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = None;
                                }
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO completed with {}, marking as worked", their_call);
                                    let band = pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);

                                    // Report QSO to cqdx.io
                                    if let Some(ref bridge) = cqdx_bridge {
                                        bridge.report_qso(pancetta_cqdx::QsoRecord {
                                            callsign: their_call.clone(),
                                            remote_grid: metadata.grids.theirs.clone(),
                                            local_grid: metadata.grids.ours.clone(),
                                            frequency: metadata.frequency as u64,
                                            mode: metadata.mode.clone(),
                                            rst_sent: metadata.reports.sent.map(|r| r.to_string()),
                                            rst_received: metadata
                                                .reports
                                                .received
                                                .map(|r| r.to_string()),
                                            start_time: metadata.start_time,
                                            end_time: metadata
                                                .end_time
                                                .unwrap_or_else(chrono::Utc::now),
                                        });
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoFailed { metadata, .. }) => {
                                // Clear AP state on QSO failure
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = None;
                                }
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO failed with {}, adding backoff", their_call);
                                    qso_lookup.record_failure(their_call);
                                }
                            }
                            Ok(_) => {} // Other events (StateChanged, etc.)
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("QSO event subscriber lagged by {} events", n);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                });

                while !shutdown.load(Ordering::Acquire) {
                    match qso_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                // Decoded FT8 messages forwarded from the decoder
                                MessageType::DecodedMessage(ref decoded_msg) => {
                                    let raw_text = decoded_msg.text.clone();
                                    let frequency = decoded_msg.frequency_offset as f64;
                                    let snr = decoded_msg.snr_db as f32;

                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(
                                        &raw_text,
                                        &our_callsign,
                                    ) {
                                        Ok(msg_type) => {
                                            if let Err(e) = qso_manager
                                                .process_message(
                                                    msg_type,
                                                    raw_text.clone(),
                                                    frequency,
                                                    Some(snr),
                                                )
                                                .await
                                            {
                                                debug!("QSO process_message error: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            debug!(
                                                "Could not parse FT8 message '{}': {}",
                                                raw_text, e
                                            );
                                        }
                                    }
                                }

                                // QSO control messages (start QSO, log, etc.)
                                MessageType::QsoMessage(qso_msg) => {
                                    match qso_msg {
                                        crate::message_bus::QsoMessage::StartQso {
                                            callsign,
                                            frequency,
                                        } => {
                                            info!(
                                                "Starting QSO with {} on {} Hz",
                                                callsign, frequency
                                            );
                                            match qso_manager
                                                .respond_to_cq(callsign.clone(), frequency as f64)
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "QSO started with {}: {}",
                                                        callsign, qso_id
                                                    );
                                                    // Send grid reply as TX request
                                                    let grid =
                                                        our_grid.as_deref().unwrap_or("AA00");
                                                    let reply = format!(
                                                        "{} {} {}",
                                                        callsign, our_callsign, grid
                                                    );
                                                    let tx_msg = ComponentMessage::new(
                                                        ComponentId::Qso,
                                                        ComponentId::Ft8Transmitter,
                                                        MessageType::TransmitRequest {
                                                            message_text: reply,
                                                            frequency_offset: frequency as f64,
                                                            qso_id: Some(qso_id.to_string()),
                                                        },
                                                        Instant::now(),
                                                    );
                                                    if let Err(e) =
                                                        message_bus.send_message(tx_msg).await
                                                    {
                                                        warn!(
                                                            "Failed to send QSO TX request: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to start QSO with {}: {}",
                                                        callsign, e
                                                    );
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::LogQso { qso_data } => {
                                            debug!("Manual log QSO: {}", qso_data);
                                        }
                                        _ => {}
                                    }
                                }

                                _ => {}
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                info!("QSO component stopped");
                Ok(())
            })
        };

        self.named_task_handles.push((ComponentId::Qso, qso_handle));
        info!("QSO component started");
        Ok(())
    }

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

                while !shutdown.load(Ordering::Acquire) {
                    match tx_rx.try_recv() {
                        Ok(message) => {
                            // Helper: wait for slot boundary, assert PTT, TX audio, de-assert PTT.
                            let wait_for_slot = || {
                                use std::time::{SystemTime, UNIX_EPOCH};
                                let now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default();
                                let secs = now.as_secs();
                                let slot_pos = secs % 15;
                                let wait_secs = if slot_pos == 0 { 0 } else { 15 - slot_pos };
                                let sub_ms = now.subsec_millis() as u64;
                                let wait_ms = wait_secs
                                    .saturating_mul(1000)
                                    .saturating_add(1000u64.saturating_sub(sub_ms))
                                    .saturating_sub(200); // 200ms guard for PTT latency
                                Duration::from_millis(wait_ms.min(15000))
                            };

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

                                    // --- Step 1: Wait for next FT8 slot boundary ---
                                    let slot_wait = wait_for_slot();

                                    if slot_wait.as_millis() > 100 {
                                        info!(
                                            "Waiting {:.1}s for next TX slot boundary",
                                            slot_wait.as_secs_f64()
                                        );
                                        sleep(slot_wait).await;
                                    }

                                    // --- Step 2: Assert PTT ---
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
                                    sleep(Duration::from_millis(50)).await;

                                    // --- Step 3: Encode and modulate ---
                                    let encode_result = encoder.encode_message(&message_text, None);
                                    let (success, duration_ms) = match encode_result {
                                        Ok(symbols) => {
                                            match modulator
                                                .modulate_symbols(&symbols, frequency_offset)
                                            {
                                                Ok(samples) => {
                                                    let duration = (samples.len() as f64 / 12000.0
                                                        * 1000.0)
                                                        as u64;
                                                    info!(
                                                        "TX: '{}' -> {} samples ({:.1}s)",
                                                        message_text,
                                                        samples.len(),
                                                        duration as f64 / 1000.0
                                                    );

                                                    // --- Step 4: Route audio to output ---
                                                    let audio_msg = ComponentMessage::new(
                                                        ComponentId::Ft8Transmitter,
                                                        ComponentId::Audio,
                                                        MessageType::AudioOutput {
                                                            samples: samples.clone(),
                                                            sample_rate: 12000,
                                                        },
                                                        Instant::now(),
                                                    );
                                                    if let Err(e) =
                                                        message_bus.send_message(audio_msg).await
                                                    {
                                                        debug!("Audio output routing: {}", e);
                                                    }

                                                    sleep(Duration::from_millis(duration)).await;
                                                    (true, duration)
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Modulation failed for '{}': {}",
                                                        message_text, e
                                                    );
                                                    (false, 0)
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Encoding failed for '{}': {}", message_text, e);
                                            (false, 0)
                                        }
                                    };

                                    // --- Step 5: De-assert PTT ---
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

                                    // --- Step 1: Wait for slot boundary ---
                                    let slot_wait = wait_for_slot();
                                    if slot_wait.as_millis() > 100 {
                                        info!(
                                            "Waiting {:.1}s for next TX slot boundary",
                                            slot_wait.as_secs_f64()
                                        );
                                        sleep(slot_wait).await;
                                    }

                                    // --- Step 2: Assert PTT ---
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
                                    sleep(Duration::from_millis(50)).await;

                                    // --- Step 3: Encode each message, build multi-TX items ---
                                    let ft8_params = pancetta_ft8::ProtocolParams::ft8();
                                    let mut multi_items = Vec::new();
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

                                    // Build MultiTxItem references
                                    for (i, symbols) in symbol_sets.iter().enumerate() {
                                        multi_items.push(pancetta_ft8::MultiTxItem {
                                            symbols: symbols.as_slice(),
                                            frequency_offset: items[i].frequency_offset,
                                            params: &ft8_params,
                                        });
                                    }

                                    let (success, duration_ms) = if !multi_items.is_empty() {
                                        match pancetta_ft8::modulate_multi_tx(
                                            &multi_items,
                                            12000,
                                            1500.0,
                                            0.5,
                                        ) {
                                            Ok(samples) => {
                                                let duration = (samples.len() as f64 / 12000.0
                                                    * 1000.0)
                                                    as u64;
                                                info!(
                                                    "Multi-TX: {} messages -> {} samples ({:.1}s)",
                                                    multi_items.len(),
                                                    samples.len(),
                                                    duration as f64 / 1000.0
                                                );

                                                let audio_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Audio,
                                                    MessageType::AudioOutput {
                                                        samples: samples.clone(),
                                                        sample_rate: 12000,
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(audio_msg).await
                                                {
                                                    debug!("Audio output routing: {}", e);
                                                }

                                                sleep(Duration::from_millis(duration)).await;
                                                (true, duration)
                                            }
                                            Err(e) => {
                                                warn!("Multi-TX modulation failed: {}", e);
                                                (false, 0)
                                            }
                                        }
                                    } else {
                                        (false, 0)
                                    };

                                    // --- Step 5: De-assert PTT ---
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

    /// Start autonomous operator component
    pub(crate) async fn start_autonomous_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_autonomous");
        let _enter = span.enter();

        let config = self.config.read().await;
        let auto_config_enabled = config.autonomous.enabled;

        if !auto_config_enabled {
            info!("Autonomous operator disabled in configuration");
            drop(config);
            let _ = self
                .message_bus
                .create_channel(ComponentId::Autonomous)
                .await?;
            return Ok(());
        }

        info!("Starting autonomous operator component");

        let qso_auto_config = pancetta_qso::AutonomousConfig {
            enabled: config.autonomous.enabled,
            slot_parity: match config.autonomous.slot_parity {
                pancetta_config::autonomous::SlotParitySetting::Even => {
                    pancetta_qso::SlotParityConfig::Even
                }
                pancetta_config::autonomous::SlotParitySetting::Odd => {
                    pancetta_qso::SlotParityConfig::Odd
                }
                pancetta_config::autonomous::SlotParitySetting::Auto => {
                    pancetta_qso::SlotParityConfig::Auto
                }
            },
            cq_after_idle_cycles: config.autonomous.cq_after_idle_cycles,
            max_concurrent_qsos: config.autonomous.max_concurrent_qsos,
            tx_offset_hz: config.autonomous.tx_offset_hz,
            min_dx_score: config.autonomous.min_dx_score,
            min_multi_slot_score: config.autonomous.min_multi_slot_score,
            cq_direction: config.autonomous.cq_direction.clone(),
            listen_cycle: pancetta_qso::autonomous::ListenCycleConfig {
                initial_interval: config.autonomous.listen_cycle.initial_interval,
                backoff_interval: config.autonomous.listen_cycle.backoff_interval,
                collision_interval: config.autonomous.listen_cycle.collision_interval,
                backoff_threshold: config.autonomous.listen_cycle.backoff_threshold,
            },
            band_hopping: pancetta_qso::autonomous::BandHoppingConfig {
                enabled: config.autonomous.band_hopping.enabled,
                hop_threshold: config.autonomous.band_hopping.hop_threshold,
                bands: config
                    .autonomous
                    .band_hopping
                    .bands
                    .iter()
                    .map(|b| pancetta_qso::autonomous::BandEntry {
                        dial_frequency: b.dial_frequency,
                        band_name: b.band_name.clone(),
                        priority: b.priority,
                    })
                    .collect(),
            },
            frequency: pancetta_qso::frequency::FrequencyAllocatorConfig {
                decode_history_cycles: config.autonomous.frequency.decode_history_cycles,
                center_bias_hz: config.autonomous.frequency.center_bias_hz,
                dx_proximity_min_hz: config.autonomous.frequency.dx_proximity_min_hz,
                dx_proximity_max_hz: config.autonomous.frequency.dx_proximity_max_hz,
                min_separation_hz: config.autonomous.frequency.min_separation_hz,
                neighbor_guard_hz: config.autonomous.frequency.neighbor_guard_hz,
                ..Default::default()
            },
        };

        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };

        // Read priority weights before dropping config
        let priority_weights = pancetta_qso::priority::PriorityWeights {
            needed_dxcc: config.autonomous.priorities.needed_dxcc,
            needed_grid: config.autonomous.priorities.needed_grid,
            pota_sota: config.autonomous.priorities.pota_sota,
            rarity: config.autonomous.priorities.rarity,
            signal_strength: config.autonomous.priorities.signal_strength,
            duplicate_penalty: config.autonomous.priorities.duplicate_penalty,
            recent_failure_penalty: config.autonomous.priorities.recent_failure_penalty,
        };
        drop(config);

        let cached_lookup = self.cached_lookup.clone();

        let spot_reporter_callsign = our_callsign.clone();
        let spot_reporter_grid = our_grid.clone();
        let operator = std::sync::Arc::new(tokio::sync::Mutex::new(
            pancetta_qso::AutonomousOperator::new(qso_auto_config, our_callsign, our_grid),
        ));

        let (waterfall_to_auto_tx, waterfall_to_auto_rx) =
            crossbeam_channel::bounded::<Vec<Vec<f32>>>(2);
        self.waterfall_to_auto_tx = Some(waterfall_to_auto_tx);

        let evaluator: std::sync::Arc<dyn pancetta_qso::DxEvaluator> = std::sync::Arc::new(
            pancetta_qso::PriorityScorer::new(priority_weights, Box::new((*cached_lookup).clone())),
        );

        let (_auto_tx, auto_rx) = self
            .message_bus
            .create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();

        let cqdx_bridge_for_auto = self.cqdx_bridge.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let auto_handle = {
            let shutdown = self.shutdown_signal.clone();
            let operator = operator.clone();
            let evaluator = evaluator.clone();

            tokio::spawn(async move {
                info!("Autonomous operator started");

                let mut slot_messages: Vec<pancetta_qso::DecodedMessageInfo> = Vec::new();
                let mut slot_interval = tokio::time::interval(Duration::from_secs(15));

                loop {
                    tokio::select! {
                        _ = slot_interval.tick() => {
                            // Report decoded spots to cqdx.io
                            if let Some(ref bridge) = cqdx_bridge_for_auto {
                                let dial_freq = operating_frequency_hz.load(Ordering::Relaxed);
                                let spot_reports: Vec<pancetta_cqdx::SpotReport> = slot_messages
                                    .iter()
                                    .filter_map(|msg| {
                                        msg.callsign.as_ref().map(|call| pancetta_cqdx::SpotReport {
                                            callsign: call.clone(),
                                            grid: None,
                                            frequency: dial_freq + msg.frequency_hz as u64,
                                            mode: "FT8".to_string(),
                                            snr: msg.snr,
                                            timestamp: chrono::Utc::now(),
                                            reporter: spot_reporter_callsign.clone(),
                                            reporter_grid: spot_reporter_grid.clone(),
                                        })
                                    })
                                    .collect();
                                bridge.report_spots(spot_reports);
                            }

                            let mut op = operator.lock().await;

                            // Update spectral data from waterfall
                            if let Ok(rows) = waterfall_to_auto_rx.try_recv() {
                                if let Some(first_row) = rows.first() {
                                    let num_bins = first_row.len();
                                    let mut avg = vec![0.0f32; num_bins];
                                    for row in &rows {
                                        for (i, &v) in row.iter().enumerate().take(num_bins) {
                                            avg[i] += v;
                                        }
                                    }
                                    let n = rows.len() as f32;
                                    for v in &mut avg {
                                        *v /= n;
                                    }
                                    op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                                        power_bins: avg,
                                        freq_min_hz: 200.0,
                                        freq_max_hz: 4000.0,
                                    });
                                }
                            }

                            if let Some(ref bridge) = cqdx_bridge_for_auto {
                                let spot_freqs = bridge.spot_frequencies().await;
                                op.update_live_spots(&spot_freqs);
                            }

                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());
                            slot_messages.clear();
                            let actions = op.decide();
                            drop(op);

                            // Collect Transmit actions, then bundle into a
                            // single MultiTransmitRequest (or single TransmitRequest).
                            let mut tx_items: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();

                            for action in actions {
                                match action {
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                        }
                                        tx_items.push(crate::message_bus::TransmitRequestItem {
                                            message_text: message_text.clone(),
                                            frequency_offset,
                                            qso_id: qso_id.clone(),
                                        });
                                    }
                                    pancetta_qso::OperatorAction::ChangeBand { dial_frequency } => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Hamlib,
                                            MessageType::RigControl(
                                                crate::message_bus::RigControlMessage::SetFrequency {
                                                    vfo: 0,
                                                    frequency: dial_frequency,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send ChangeBand: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::StatusUpdate(status) => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Tui,
                                            MessageType::AutonomousStatus(
                                                crate::message_bus::AutonomousStatusData {
                                                    enabled: status.enabled,
                                                    state: status.state,
                                                    slot_parity: status.slot_parity,
                                                    listen_counter: status.listen_counter,
                                                    active_qsos: status.active_qsos,
                                                    max_qsos: status.max_qsos,
                                                    idle_cycles: status.idle_cycles,
                                                    band_name: status.band_name,
                                                    tx_offset_hz: status.tx_offset_hz,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send AutonomousStatus: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::Listen
                                    | pancetta_qso::OperatorAction::CollisionListen => {}
                                    pancetta_qso::OperatorAction::FrequencyShift { new_offset_hz } => {
                                        info!("Autonomous: TX offset shifted to {:.0} Hz", new_offset_hz);
                                    }
                                }
                            }

                            // Bundle collected TX items into a single message.
                            if tx_items.len() == 1 {
                                let item = tx_items.remove(0);
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TransmitRequest {
                                        message_text: item.message_text,
                                        frequency_offset: item.frequency_offset,
                                        qso_id: item.qso_id,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send TransmitRequest: {}", e);
                                }
                            } else if tx_items.len() > 1 {
                                info!("Bundling {} TX items into MultiTransmitRequest", tx_items.len());
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::MultiTransmitRequest { items: tx_items },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send MultiTransmitRequest: {}", e);
                                }
                            }
                        }

                        _ = async {
                            loop {
                                match auto_rx.try_recv() {
                                    Ok(message) => {
                                        if let MessageType::DecodedMessage(decoded_msg) = message.message_type {
                                            slot_messages.push(pancetta_qso::DecodedMessageInfo {
                                                callsign: decoded_msg.message.from_callsign.clone(),
                                                frequency_hz: decoded_msg.frequency_offset,
                                                snr: decoded_msg.snr_db as i32,
                                                message_text: decoded_msg.text.clone(),
                                            });
                                        }
                                    }
                                    Err(crossbeam_channel::TryRecvError::Empty) => {
                                        tokio::task::yield_now().await;
                                        break;
                                    }
                                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                                }
                            }
                        } => {}
                    }

                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }

                info!("Autonomous operator stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Autonomous, auto_handle));
        info!("Autonomous operator component started");
        Ok(())
    }

    /// Start DX cluster component for real-time spot monitoring
    pub(crate) async fn start_dx_cluster_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.dx_cluster.enabled {
            info!("DX cluster disabled in configuration");
            drop(config);
            // Still create channel so message bus doesn't complain
            let _ = self
                .message_bus
                .create_channel(ComponentId::DxCluster)
                .await?;
            return Ok(());
        }

        let cluster_hostname = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string());
        let cluster_port = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.port)
            .unwrap_or(23);
        let our_callsign = config.station.callsign.clone();
        drop(config);

        info!(
            "Starting DX cluster component ({}:{})",
            cluster_hostname, cluster_port
        );

        let (_dx_tx, _dx_rx) = self
            .message_bus
            .create_channel(ComponentId::DxCluster)
            .await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::cluster::{ClusterConfig, DxClusterClient};

                let mut client = DxClusterClient::with_config(ClusterConfig {
                    hostname: cluster_hostname.clone(),
                    port: cluster_port,
                    callsign: our_callsign.clone(),
                    timeout_seconds: 30,
                    reconnect_delay_seconds: 30,
                    auto_reconnect: true,
                    filter_settings: Default::default(),
                    use_websocket: false,
                    websocket_url: None,
                });

                match client.connect().await {
                    Ok(_) => {
                        info!("Connected to DX cluster");

                        // Login with our callsign
                        if let Err(e) = client.login().await {
                            warn!("DX cluster login failed: {}. Continuing without.", e);
                        }

                        // Monitor spots and forward to TUI
                        while !shutdown.load(Ordering::Acquire) {
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                client.receive_spot(),
                            )
                            .await
                            {
                                Ok(Some(spot)) => {
                                    debug!(
                                        "DX spot: {} on {} Hz by {}",
                                        spot.callsign, spot.frequency, spot.spotter
                                    );

                                    let msg = ComponentMessage::new(
                                        ComponentId::DxCluster,
                                        ComponentId::Tui,
                                        MessageType::DxMessage(
                                            crate::message_bus::DxMessage::Spot {
                                                callsign: spot.callsign,
                                                frequency: spot.frequency,
                                                spotter: spot.spotter,
                                                comment: spot.comment.unwrap_or_default(),
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        debug!("Failed to forward DX spot: {}", e);
                                    }
                                }
                                Ok(None) => {
                                    // No spot available, yield
                                    tokio::task::yield_now().await;
                                }
                                Err(_) => {
                                    // Timeout -- normal, just loop
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to DX cluster: {}. Feature disabled.", e);
                    }
                }

                info!("DX cluster component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::DxCluster, dx_handle));
        info!("DX cluster component started");
        Ok(())
    }

    /// Start PSKReporter upload component
    ///
    /// Receives decoded FT8 messages, batches them, and uploads to PSKReporter
    /// at the configured interval (default: 5 minutes).
    pub(crate) async fn start_pskreporter_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.psk_reporter.enabled {
            info!("PSKReporter upload disabled in configuration");
            drop(config);
            let _ = self
                .message_bus
                .create_channel(ComponentId::PskReporter)
                .await?;
            return Ok(());
        }

        let our_callsign = config.station.callsign.clone();
        let our_grid = config.station.grid_square.clone();
        let upload_interval = config.network.psk_reporter.upload_interval_seconds;
        let antenna = config
            .network
            .psk_reporter
            .reporter_info
            .antenna_info
            .clone()
            .unwrap_or_default();
        let software = format!(
            "{}/{}",
            config.network.psk_reporter.reporter_info.software_name,
            config.network.psk_reporter.reporter_info.software_version
        );
        drop(config);

        info!(
            "Starting PSKReporter upload component (interval: {}s)",
            upload_interval
        );

        let (_psk_tx, psk_rx) = self
            .message_bus
            .create_channel(ComponentId::PskReporter)
            .await?;

        let psk_operating_freq = self.operating_frequency_hz.clone();
        let psk_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::pskreporter::{
                    PskReporterUploadConfig, PskReporterUploader, ReceptionReport,
                };

                let upload_config = PskReporterUploadConfig {
                    reporter_callsign: our_callsign,
                    reporter_grid: our_grid,
                    antenna,
                    software,
                    upload_interval_secs: upload_interval,
                    ..Default::default()
                };

                let mut uploader = PskReporterUploader::new(upload_config);
                let mut upload_timer = interval(Duration::from_secs(upload_interval));

                while !shutdown.load(Ordering::Acquire) {
                    // Drain incoming decoded messages
                    loop {
                        match psk_rx.try_recv() {
                            Ok(message) => {
                                if let MessageType::DecodedMessage(ref decoded_msg) =
                                    message.message_type
                                {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64;

                                    if let Some(ref callsign) = decoded_msg.message.from_callsign {
                                        let dial_freq = psk_operating_freq.load(Ordering::Relaxed);
                                        uploader.add_report(ReceptionReport {
                                            tx_callsign: callsign.clone(),
                                            frequency: dial_freq
                                                + decoded_msg.frequency_offset as u64,
                                            snr: Some(decoded_msg.snr_db as i32),
                                            mode: "FT8".to_string(),
                                            tx_grid: decoded_msg.message.grid_square.clone(),
                                            timestamp,
                                        });
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                info!("PSKReporter channel disconnected");
                                return Ok(());
                            }
                        }
                    }

                    // Check if it's time to upload
                    tokio::select! {
                        _ = upload_timer.tick() => {
                            if uploader.pending_count() > 0 {
                                match uploader.flush().await {
                                    Ok(count) => {
                                        info!("PSKReporter: uploaded {} spots", count);
                                    }
                                    Err(e) => {
                                        warn!("PSKReporter upload failed: {}", e);
                                    }
                                }
                            }
                        }
                        _ = sleep(Duration::from_millis(100)) => {
                            // Short sleep to avoid busy-looping
                        }
                    }
                }

                // Flush remaining on shutdown
                if uploader.pending_count() > 0 {
                    if let Err(e) = uploader.flush().await {
                        warn!("PSKReporter final flush failed: {}", e);
                    }
                }

                info!("PSKReporter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::PskReporter, psk_handle));
        info!("PSKReporter component started");
        Ok(())
    }
}
