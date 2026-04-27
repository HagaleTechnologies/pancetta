//! QSO state-machine component.
//!
//! Wires decoded FT8 messages into the `pancetta-qso` state machine for
//! tracking, auto-logs completed exchanges to SQLite at
//! `~/.pancetta/qso.db`, and surfaces respond-to-CQ outcomes to the TUI
//! status bar (so Space-to-call says "Calling X — TX queued" or "Call X
//! failed: duplicate QSO …" instead of the previous optimistic
//! "Calling X..." that hid silent rejections).
//!
//! Subscribes to QSO state-machine events to:
//!  - update the FT8 decoder's AP context as state advances (so AP3/AP4
//!    decoding can lean on the active QSO's contra-callsign),
//!  - forward auto-sequence outbound messages to the transmitter,
//!  - record completed/failed QSOs in the worked-station lookup, and
//!  - report completed QSOs to cqdx.io via the bridge.

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Send a free-form status string to the TUI status bar via the message bus.
/// Used to surface QSO/TX state changes that the operator should see, even
/// when nothing failed at the transport layer (e.g. duplicate suppression,
/// QSO state-machine rejections).
async fn emit_status(message_bus: &MessageBus, text: impl Into<String>) {
    let msg = ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Tui,
        MessageType::StatusUpdate(text.into()),
        Instant::now(),
    );
    let _ = message_bus.send_message(msg).await;
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
                    use pancetta_qso::async_database::AsyncQsoDatabase;

                    // Determine the current band from the rig's operating frequency,
                    // falling back to "20m".  This is a best-effort seed — the
                    // autonomous operator will always re-validate against the live
                    // worked-on-band set as QSOs complete.
                    let freq_hz = operating_frequency_hz.load(std::sync::atomic::Ordering::Relaxed);
                    let band = pancetta_cqdx::frequency_to_band(freq_hz)
                        .unwrap_or_else(|| "20m".to_string())
                        .to_uppercase();

                    match AsyncQsoDatabase::open(&db_path).await {
                        Ok(db) => {
                            let callsigns = db.get_worked_callsigns(&band).await;
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
                                        // BUG: This encode failure leaves the QSO state machine
                                        // stuck waiting for a TX that will never happen. The QSO
                                        // will eventually time out, but ideally we'd send a
                                        // QsoFailed event here. The qso_manager is not accessible
                                        // from this forwarding task.
                                        error!(
                                            "Failed to generate FT8 message for QSO {} — QSO state machine may be stuck: {}",
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
                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
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
                                                            message_text: reply.clone(),
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
                                                        emit_status(
                                                            &message_bus,
                                                            format!(
                                                                "Call {}: TX bus send failed: {}",
                                                                callsign, e
                                                            ),
                                                        )
                                                        .await;
                                                    } else {
                                                        emit_status(
                                                            &message_bus,
                                                            format!(
                                                                "Calling {} — TX queued ({} Hz)",
                                                                callsign, frequency
                                                            ),
                                                        )
                                                        .await;
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to start QSO with {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Call {} failed: {}", callsign, e),
                                                    )
                                                    .await;
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
}
