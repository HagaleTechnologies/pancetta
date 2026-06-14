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
        let active_qso_freq_hz = self.active_qso_freq_hz.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{LoggerConfig, QsoManager, QsoManagerConfig};

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

                // ADIF source-of-truth writer. Subscribes to QsoEvent::QsoCompleted
                // and appends one ADIF record per completed QSO. Fail-soft: if open
                // fails, we log but proceed with DB-only — every operator should at
                // least get duplicate detection from the DB.
                let adif_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qsos.adi");

                let _adif_writer = match pancetta_qso::AdifLogWriter::open(&adif_path).await {
                    Ok(w) => {
                        info!("ADIF log open at {}", adif_path.display());
                        let w = std::sync::Arc::new(w);
                        start_adif_subscriber(w.clone(), qso_manager.subscribe(), shutdown.clone());
                        Some(w)
                    }
                    Err(e) => {
                        warn!(
                            "ADIF writer init failed at {}: {} — continuing; QSOs this \
                             session will be DB-only",
                            adif_path.display(),
                            e,
                        );
                        None
                    }
                };

                // Async QSO logger — subscribes independently to QsoEvent::QsoCompleted
                // and inserts into the rebuildable SQLite index. Comes AFTER the ADIF
                // writer so that a crash between the two is recoverable by Task 5's
                // startup replay (ADIF is source of truth; DB is cache).
                let logger_config = LoggerConfig {
                    database_path: db_path.clone(),
                    ..Default::default()
                };

                let _async_logger = match pancetta_qso::async_logger::AsyncQsoLogger::new(
                    logger_config,
                    qso_manager.clone(),
                )
                .await
                {
                    Ok(l) => {
                        info!(
                            "Async QSO logger initialized with database at {}",
                            db_path.display()
                        );
                        let l = std::sync::Arc::new(l);
                        if let Err(e) = l.start().await {
                            warn!("Async QSO logger background tasks failed to start: {}", e);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize async QSO logger (continuing without): {}",
                            e
                        );
                        None
                    }
                };

                // Seed worked-station history from the QSO database so that
                // previously-worked stations are recognised as duplicates across restarts.
                //
                // Three-case startup decision:
                //   1. Migration: ADIF missing but legacy DB exists → dump DB to ADIF first
                //      so contacts are not lost; future runs use ADIF as source of truth.
                //   2. Replay: index missing or older than ADIF → drop + replay so duplicate
                //      detection sees every prior contact.
                //   3. Open as-is: normal startup; index is current.
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

                    // Case 1: migration — ADIF missing but legacy DB exists.
                    let adif_exists = tokio::fs::try_exists(&adif_path).await.unwrap_or(false);
                    let db_exists = tokio::fs::try_exists(&db_path).await.unwrap_or(false);

                    if !adif_exists && db_exists {
                        info!(
                            "ADIF missing but legacy DB present — migrating QSOs from {} to {}",
                            db_path.display(),
                            adif_path.display(),
                        );
                        match AsyncQsoDatabase::open(&db_path).await {
                            Ok(db) => {
                                if let Err(e) = db.export_to_adif(&adif_path).await {
                                    warn!(
                                        "DB→ADIF migration failed: {} — index continues to work, \
                                         but ADIF source-of-truth will only contain QSOs logged \
                                         from now on",
                                        e,
                                    );
                                } else {
                                    info!("DB→ADIF migration succeeded");
                                }
                            }
                            Err(e) => {
                                warn!("Could not open legacy DB for migration: {} — skipping", e);
                            }
                        }
                    }

                    // Case 2: replay — index missing or older than ADIF.
                    let needs_replay = match (
                        tokio::fs::metadata(&db_path).await.ok(),
                        tokio::fs::metadata(&adif_path).await.ok(),
                    ) {
                        (None, Some(_)) => {
                            info!(
                                "Index missing at {} — replaying from ADIF",
                                db_path.display()
                            );
                            true
                        }
                        (Some(db_meta), Some(adif_meta)) => {
                            match (db_meta.modified().ok(), adif_meta.modified().ok()) {
                                (Some(d), Some(a)) if a > d => {
                                    info!(
                                        "Index at {} is older than ADIF at {} — replaying",
                                        db_path.display(),
                                        adif_path.display(),
                                    );
                                    true
                                }
                                _ => false,
                            }
                        }
                        // No ADIF and no DB: fresh install; coordinator creates both later.
                        _ => false,
                    };

                    let db_for_seed = if needs_replay {
                        match AsyncQsoDatabase::replay_from_adif(&db_path, &adif_path).await {
                            Ok(db) => Some(db),
                            Err(e) => {
                                warn!(
                                    "ADIF replay failed: {} — falling back to existing index \
                                     (may be stale)",
                                    e,
                                );
                                AsyncQsoDatabase::open(&db_path).await.ok()
                            }
                        }
                    } else {
                        // Case 3: open as-is.
                        AsyncQsoDatabase::open(&db_path).await.ok()
                    };

                    if let Some(db) = db_for_seed {
                        let callsigns = db.get_worked_callsigns(&band).await;
                        if callsigns.is_empty() {
                            info!(
                                "QSO database has no prior contacts on {} — starting fresh",
                                band
                            );
                        } else {
                            qso_lookup.seed_worked_from_list(&band, callsigns);
                        }
                    } else {
                        warn!(
                            "Could not open QSO database for startup seed ({}) — \
                             previously-worked stations will not be detected as duplicates \
                             until re-worked this session",
                            db_path.display(),
                        );
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
                let qso_freq_state = active_qso_freq_hz;
                let snapshot_qso_manager = qso_manager.clone();
                let snapshot_bus = tx_bus.clone();
                tokio::spawn(async move {
                    while !tx_shutdown.load(Ordering::Acquire) {
                        match qso_events.recv().await {
                            Ok(pancetta_qso::QsoEvent::StateChanged { new_state, .. }) => {
                                // Map QSO state to AP context for AP3/AP4 decoding.
                                //
                                // WSJT-X Improved-style a8 wiring: also enumerate
                                // the expected next-message texts from the
                                // partner so that the FT8 decoder's a8 path
                                // (gated on `Ft8Config::a8_qso_state_ap_enabled`)
                                // can relax the AP confidence floor for decodes
                                // that match. Inspired by spec ref
                                // `spec-wsjtx-improved-a8-decoding.md`. When
                                // a8 is disabled the templates are still
                                // populated but never consulted, so wiring
                                // is byte-safe.
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
                                    )
                                    .map(|q| {
                                        let texts = pancetta_ft8::ap::enumerate_a8_expected_texts(
                                            &tx_callsign,
                                            target_callsign,
                                            pancetta_ft8::QsoApProgress::WaitingForReport,
                                        );
                                        q.with_expected_texts(texts)
                                    }),
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
                                    )
                                    .map(|q| {
                                        let texts = pancetta_ft8::ap::enumerate_a8_expected_texts(
                                            &tx_callsign,
                                            their_callsign,
                                            pancetta_ft8::QsoApProgress::WaitingForConfirmation,
                                        );
                                        q.with_expected_texts(texts)
                                    }),
                                    // Terminal or idle states clear the AP context
                                    _ => None,
                                };
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = new_ap;
                                }

                                // hb-091 scoped fast-path: mirror the AP
                                // update with the partner's audio freq.
                                // `QsoState::frequency()` returns Some for
                                // the in-QSO states and None for Idle /
                                // Failed / Completed.
                                if let Ok(mut guard) = qso_freq_state.write() {
                                    *guard = if new_state.is_active() {
                                        new_state.frequency()
                                    } else {
                                        None
                                    };
                                }

                                // Push an updated snapshot of in-progress
                                // QSOs to the TUI banner. The QSO state
                                // machine is the source of truth; the TUI
                                // replaces its list each push.
                                let snapshot =
                                    build_active_qso_snapshot(&snapshot_qso_manager).await;
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot { qsos: snapshot },
                                    Instant::now(),
                                );
                                if let Err(e) = snapshot_bus.send_message(snap_msg).await {
                                    debug!("Failed to push active-QSOs snapshot: {}", e);
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                qso_id,
                                message,
                                frequency,
                                tx_parity,
                            }) => {
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={}, tx_parity={:?})",
                                            text, frequency, qso_id, tx_parity
                                        );
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: text,
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                                tx_parity,
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
                                // hb-091: also clear the partner freq.
                                if let Ok(mut guard) = qso_freq_state.write() {
                                    *guard = None;
                                }
                                // Push fresh snapshot so the banner drops
                                // the just-completed QSO from the active list.
                                let snapshot =
                                    build_active_qso_snapshot(&snapshot_qso_manager).await;
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot { qsos: snapshot },
                                    Instant::now(),
                                );
                                let _ = snapshot_bus.send_message(snap_msg).await;
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
                                // Push fresh snapshot so the banner drops
                                // the failed QSO.
                                let snapshot =
                                    build_active_qso_snapshot(&snapshot_qso_manager).await;
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot { qsos: snapshot },
                                    Instant::now(),
                                );
                                let _ = snapshot_bus.send_message(snap_msg).await;
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
                                            dx_parity,
                                        } => {
                                            info!(
                                                "Starting QSO with {} on {} Hz (manual)",
                                                callsign, frequency
                                            );
                                            // Operator-initiated MANUAL call:
                                            //  - bypasses the self-duplicate gate (operator
                                            //    explicitly chose to work/re-work this DX), and
                                            //  - keep-calls every TX slot under the manual
                                            //    watchdog (5 min / 10 calls).
                                            //
                                            // respond_to_cq_manual emits the first
                                            // CqResponse as a QsoEvent::MessageToSend,
                                            // which the event-forwarding task above turns
                                            // into a TransmitRequest with the latched
                                            // tx_parity. The watchdog re-arm
                                            // (QsoManager::rearm_manual_calls) re-emits the
                                            // same MessageToSend once per slot until the DX
                                            // answers or the watchdog fires — so there is no
                                            // separate TransmitRequest here (that would
                                            // double-send the first call).
                                            match qso_manager
                                                .respond_to_cq_manual(
                                                    callsign.clone(),
                                                    frequency as f64,
                                                    dx_parity,
                                                )
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Manual QSO started with {}: {} \
                                                         (keep-calling under watchdog)",
                                                        callsign, qso_id
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Calling {} — TX queued ({} Hz)",
                                                            callsign, frequency
                                                        ),
                                                    )
                                                    .await;
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

/// Build a flat snapshot of in-progress QSOs from the QSO manager,
/// suitable for `MessageType::ActiveQsosSnapshot`. The TUI banner and
/// QSO-detail panel both render from this.
async fn build_active_qso_snapshot(
    qso_manager: &pancetta_qso::QsoManager,
) -> Vec<crate::message_bus::ActiveQsoSnapshotItem> {
    let active = qso_manager.get_active_qsos().await;

    // FIX 3 (defense-in-depth): the QSO engine now supersedes older active
    // QSOs per (callsign, band) at start time, so a callsign should appear at
    // most once here. Dedup anyway, keeping the most-recently-started QSO, so
    // the TUI "exchanges" list never shows two entries for one (callsign,
    // band) even if a transient race ever surfaced both.
    let mut latest: std::collections::HashMap<(String, String), pancetta_qso::QsoProgress> =
        std::collections::HashMap::new();
    for (_id, progress) in active {
        let Some(their) = progress
            .state
            .their_callsign()
            .map(str::to_string)
            .or_else(|| progress.metadata.their_callsign.clone())
        else {
            continue;
        };
        let band = pancetta_qso::utils::frequency_to_band(progress.metadata.frequency);
        let key = (their, band);
        match latest.get(&key) {
            Some(existing) if existing.metadata.start_time >= progress.metadata.start_time => {}
            _ => {
                latest.insert(key, progress);
            }
        }
    }

    latest
        .values()
        .filter_map(snapshot_item_from_progress)
        .collect()
}

/// Flatten one `QsoProgress` into the bus snapshot item. Pure read of
/// state the QSO engine already tracks — no behavioral change to the
/// engine. Returns `None` when the contra callsign is unknown (nothing
/// useful to render yet).
///
/// Batch 94: in addition to the banner fields, derives the QSO-detail
/// panel fields — last message exchanged in each direction (from
/// `progress.messages`), measured RX SNR (signal strength of the last
/// received message), reports sent/received (from
/// `metadata.reports`), and the exchange count.
fn snapshot_item_from_progress(
    progress: &pancetta_qso::QsoProgress,
) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
    use pancetta_qso::{MessageDirection, QsoState};
    let their = progress
        .state
        .their_callsign()
        .map(str::to_string)
        .or_else(|| progress.metadata.their_callsign.clone())?;
    let frequency_hz = progress
        .state
        .frequency()
        .unwrap_or(progress.metadata.frequency);
    let state = match &progress.state {
        QsoState::Idle => "idle",
        QsoState::CallingCq { .. } => "calling CQ",
        QsoState::RespondingToCq { .. } => "→ called",
        QsoState::WaitingForReport { .. } => "wait rpt",
        QsoState::SendingReport { .. } => "sending rpt",
        QsoState::WaitingForConfirmation { .. } => "wait RR73",
        QsoState::SendingConfirmation { .. } => "sending RR73",
        QsoState::Completed { .. } => "done",
        QsoState::Failed { .. } => "failed",
        QsoState::Contest(pancetta_qso::ContestState::ExchangingInfo { .. }) => "contest exch",
        QsoState::Contest(pancetta_qso::ContestState::ContestCompleted { .. }) => "contest done",
    }
    .to_string();

    let last_tx = progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Sent);
    let last_rx = progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Received);

    Some(crate::message_bus::ActiveQsoSnapshotItem {
        their_callsign: their,
        state,
        started_at: progress.metadata.start_time,
        frequency_hz,
        tx_parity: progress.metadata.tx_parity,
        last_tx_text: last_tx.map(|m| m.raw_text.clone()),
        last_tx_at: last_tx.map(|m| m.timestamp),
        last_rx_text: last_rx.map(|m| m.raw_text.clone()),
        last_rx_at: last_rx.map(|m| m.timestamp),
        snr_rx: last_rx.and_then(|m| m.signal_strength).map(|s| s as i32),
        report_sent: progress.metadata.reports.sent.map(i32::from),
        report_received: progress.metadata.reports.received.map(i32::from),
        exchange_count: progress.messages.len() as u32,
    })
}

#[cfg(test)]
mod snapshot_tests {
    use super::snapshot_item_from_progress;
    use chrono::{Duration, Utc};
    use pancetta_qso::{
        GridSquares, MessageDirection, QsoMetadata, QsoProgress, QsoState, SignalReports,
    };

    /// Build a QsoProgress mid-exchange: we called them, sent our grid,
    /// and just received their report.
    fn fixture_progress() -> QsoProgress {
        let start = Utc::now() - Duration::seconds(45);
        let their_call = "JA1ABC".to_string();
        let messages = vec![
            pancetta_qso::states::QsoMessage {
                timestamp: start + Duration::seconds(15),
                direction: MessageDirection::Sent,
                message_type: pancetta_qso::states::MessageType::CqResponse {
                    calling_station: their_call.clone(),
                    responding_station: "K5ARH".to_string(),
                    grid: Some("EM10".to_string()),
                },
                raw_text: "JA1ABC K5ARH EM10".to_string(),
                signal_strength: None,
                frequency: 1500.0,
            },
            pancetta_qso::states::QsoMessage {
                timestamp: start + Duration::seconds(30),
                direction: MessageDirection::Received,
                message_type: pancetta_qso::states::MessageType::SignalReport {
                    to_station: "K5ARH".to_string(),
                    from_station: their_call.clone(),
                    report: -12,
                },
                raw_text: "K5ARH JA1ABC -12".to_string(),
                signal_strength: Some(-12.4),
                frequency: 1500.0,
            },
        ];
        QsoProgress {
            state: QsoState::SendingReport {
                their_callsign: their_call.clone(),
                their_report: Some(-12),
                our_report: -8,
                frequency: 1500.0,
                started_at: start,
            },
            state_history: Vec::new(),
            messages,
            metadata: QsoMetadata {
                qso_id: pancetta_qso::QsoId::new_v4(),
                our_callsign: "K5ARH".to_string(),
                their_callsign: Some(their_call),
                frequency: 1500.0,
                mode: "FT8".to_string(),
                start_time: start,
                end_time: None,
                reports: SignalReports {
                    sent: Some(-8),
                    received: Some(-12),
                },
                grids: GridSquares::default(),
                contest_info: None,
                tags: std::collections::HashMap::new(),
                notes: None,
                tx_parity: Some(pancetta_core::slot::SlotParity::Odd),
                initiated_by: Default::default(),
                call_count: 0,
                first_call_at: None,
                last_call_at: None,
            },
        }
    }

    /// All detail-panel fields derive from state the engine already
    /// tracks: last message per direction, measured RX SNR, reports,
    /// exchange count, plus the original banner fields.
    #[test]
    fn snapshot_derives_detail_fields_from_progress() {
        let item = snapshot_item_from_progress(&fixture_progress()).expect("item");
        assert_eq!(item.their_callsign, "JA1ABC");
        assert_eq!(item.state, "sending rpt");
        assert_eq!(item.frequency_hz, 1500.0);
        assert_eq!(item.tx_parity, Some(pancetta_core::slot::SlotParity::Odd));
        assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH EM10"));
        assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert!(item.last_tx_at.is_some());
        assert!(item.last_rx_at.is_some());
        assert_eq!(item.snr_rx, Some(-12));
        assert_eq!(item.report_sent, Some(-8));
        assert_eq!(item.report_received, Some(-12));
        assert_eq!(item.exchange_count, 2);
    }

    /// The most recent message per direction wins, not the first.
    #[test]
    fn snapshot_picks_latest_message_per_direction() {
        let mut progress = fixture_progress();
        progress.messages.push(pancetta_qso::states::QsoMessage {
            timestamp: Utc::now(),
            direction: MessageDirection::Sent,
            message_type: pancetta_qso::states::MessageType::ReportAck {
                to_station: "JA1ABC".to_string(),
                from_station: "K5ARH".to_string(),
                report: -8,
            },
            raw_text: "JA1ABC K5ARH R-8".to_string(),
            signal_strength: None,
            frequency: 1500.0,
        });
        let item = snapshot_item_from_progress(&progress).expect("item");
        assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH R-8"));
        // RX side unchanged by a new TX.
        assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert_eq!(item.exchange_count, 3);
    }

    /// No callsign known yet (e.g. CallingCq with empty metadata) →
    /// nothing useful to render → None.
    #[test]
    fn snapshot_skips_qso_without_callsign() {
        let mut progress = fixture_progress();
        progress.state = QsoState::CallingCq {
            frequency: 1500.0,
            started_at: Utc::now(),
            call_count: 1,
        };
        progress.metadata.their_callsign = None;
        assert!(snapshot_item_from_progress(&progress).is_none());
    }

    /// A QSO with no messages yet (just started) still produces an item
    /// with empty detail fields — the panel renders placeholders.
    #[test]
    fn snapshot_handles_empty_message_history() {
        let mut progress = fixture_progress();
        progress.messages.clear();
        let item = snapshot_item_from_progress(&progress).expect("item");
        assert!(item.last_tx_text.is_none());
        assert!(item.last_rx_text.is_none());
        assert!(item.snr_rx.is_none());
        assert_eq!(item.exchange_count, 0);
    }
}

/// Spawn a background task that listens for `QsoEvent::QsoCompleted` and
/// appends one ADIF record to the durable log for each completed QSO.
///
/// ADIF is the source of truth: a failed write is logged at ERROR level because
/// it indicates a real problem (disk full, permissions, etc.) that the operator
/// should investigate. The task handles receiver lag and channel closure
/// gracefully so it never blocks or panics.
fn start_adif_subscriber(
    writer: std::sync::Arc<pancetta_qso::AdifLogWriter>,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::spawn(async move {
        while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
            match events.recv().await {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                    if let Err(e) = writer.append(&metadata).await {
                        // ADIF is the source of truth. A failed write deserves
                        // a loud signal — disk full, permissions, etc.
                        tracing::error!(
                            "ADIF append failed for QSO {} with {}: {}",
                            metadata.qso_id,
                            metadata.their_callsign.as_deref().unwrap_or("?"),
                            e,
                        );
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ADIF subscriber lagged by {n} QSO events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
