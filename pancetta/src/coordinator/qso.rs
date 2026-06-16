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
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// item-2-auto-73 tuning. When a station we JUST completed a *manual* QSO
/// with keeps re-sending us RR73/RRR (they did not copy our 73), we
/// auto-re-send our 73 — bounded so a stuck DX can never make us TX
/// forever:
///   - only for **manual** completions (never autonomous),
///   - only while within [`AUTO_73_WINDOW`] of completion,
///   - at most [`AUTO_73_MAX_RESENDS`] extra 73s per completed QSO,
///   - at most once per ~15 s FT8 slot (so two decodes of the same RR73 in
///     one slot fire only once),
///   - never when a live QSO with that station is already active.
const AUTO_73_WINDOW: chrono::Duration = chrono::Duration::minutes(3);
/// Maximum number of auto re-sends of our 73 per completed manual QSO.
const AUTO_73_MAX_RESENDS: u8 = 3;
/// Minimum spacing between auto re-sends (one FT8 slot is 15 s; we use a
/// slightly-under-slot guard so we fire at most once per slot even if the
/// DX's RR73 is decoded a hair early/late).
const AUTO_73_MIN_SPACING: chrono::Duration = chrono::Duration::seconds(14);

/// One recently-completed **manual** QSO, tracked so we can auto-re-send our
/// 73 if the DX keeps sending RR73/RRR. Keyed (in the map) by uppercased
/// callsign.
#[derive(Debug, Clone)]
struct RecentManualCompletion {
    /// When the QSO completed (window + pruning are measured from here).
    completed_at: chrono::DateTime<chrono::Utc>,
    /// Audio frequency (Hz) we last heard them on — where we send the 73.
    frequency_hz: f64,
    /// DX slot parity (so our 73 lands on the slot they expect). `None`
    /// lets the TX scheduler fall back to its default.
    dx_parity: Option<pancetta_core::slot::SlotParity>,
    /// How many auto re-sends we have already done (bounded by
    /// [`AUTO_73_MAX_RESENDS`]).
    resends: u8,
    /// When we last auto-re-sent (one-per-slot guard). `None` = never yet.
    last_resend_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Shared map of recently-completed manual QSOs. Populated by the QSO-event
/// task on `QsoCompleted` and consumed by the decode-processing loop when a
/// directed RR73/RRR arrives. Both live inside the same QSO component task.
type RecentManualCompletions = Arc<Mutex<HashMap<String, RecentManualCompletion>>>;

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

/// item-2-auto-73 trigger. When `msg_type` is a directed-at-us RR73/RRR
/// (`FinalConfirmation { to_station == our call }`) from a station we just
/// MANUALLY completed a QSO with, auto-re-send our 73 — bounded so it can
/// never run away:
///   - the sender must be in `completions` (a MANUAL completion stashed by
///     the QsoCompleted handler) and within [`AUTO_73_WINDOW`],
///   - `resends < AUTO_73_MAX_RESENDS`,
///   - at most once per [`AUTO_73_MIN_SPACING`] (≈ one FT8 slot, so two
///     decodes of the same RR73 in one slot fire only once),
///   - the global [`pancetta_core::TxPolicy`] must `allows_any_tx()`
///     (RESPOND-ONLY allows — it's a response; DISABLED blocks),
///   - there must be NO currently-active QSO with the sender (don't fight a
///     live exchange).
///
/// On success it sends our 73 via the same `respond_to_caller(SeventyThree)`
/// path the Callers/Space close uses; the resulting Completed QSO is handled
/// by the drop-stale-TX grace window (the 73 frame goes out, then drops), so
/// there is no runaway. After the cap/window the entry is dropped.
#[allow(clippy::too_many_arguments)]
async fn maybe_auto_resend_73(
    msg_type: &pancetta_qso::states::MessageType,
    our_callsign: &str,
    frequency_hz: f64,
    dx_parity: Option<pancetta_core::slot::SlotParity>,
    qso_manager: &pancetta_qso::QsoManager,
    completions: &RecentManualCompletions,
    tx_policy: &std::sync::atomic::AtomicU8,
    message_bus: &MessageBus,
) {
    use pancetta_qso::states::MessageType as Mt;

    // Only directed RR73/RRR (both parse to FinalConfirmation) addressed to us.
    let from_station = match msg_type {
        Mt::FinalConfirmation {
            to_station,
            from_station,
        } if to_station.eq_ignore_ascii_case(our_callsign) => from_station.clone(),
        _ => return,
    };
    let key = from_station.to_uppercase();

    // TX policy gate (DISABLED blocks; RESPOND-ONLY/FULL allow). Cheap check
    // first, before touching the map.
    let policy =
        pancetta_core::TxPolicy::from_u8(tx_policy.load(std::sync::atomic::Ordering::Relaxed));
    if !policy.allows_any_tx() {
        return;
    }

    let now = chrono::Utc::now();

    // Decide under the map lock: is this a stashed manual completion still in
    // window and under the cap, with the per-slot guard satisfied? We mutate
    // the entry (resends/last_resend_at) here so the bound holds even if RR73
    // arrives every slot. We do NOT call into the QSO manager while holding
    // the lock.
    {
        let mut map = completions.lock().await;
        // Prune expired entries every time we look.
        map.retain(|_, e| now.signed_duration_since(e.completed_at) < AUTO_73_WINDOW);

        let Some(entry) = map.get_mut(&key) else {
            return;
        };
        if entry.resends >= AUTO_73_MAX_RESENDS {
            // Cap reached — stop and drop the entry so we never reconsider it.
            map.remove(&key);
            return;
        }
        if let Some(last) = entry.last_resend_at {
            if now.signed_duration_since(last) < AUTO_73_MIN_SPACING {
                // Already re-sent this slot — ignore the duplicate decode.
                return;
            }
        }
        // Commit the send: increment + stamp BEFORE we drop the lock so two
        // decodes racing in the same slot can't both pass the per-slot guard.
        entry.resends += 1;
        entry.last_resend_at = Some(now);
        // Prefer the freq/parity we just heard them on (fresher); fall back to
        // the stashed completion values if the decode lacked parity.
        entry.frequency_hz = frequency_hz;
        if dx_parity.is_some() {
            entry.dx_parity = dx_parity;
        }
    }

    // Don't fight a live QSO with this station: if one is active, skip the
    // auto-73 (the QSO state machine is handling it). The counter was already
    // incremented above, which is fine — it only tightens the bound.
    let active = qso_manager.get_active_qsos().await;
    let has_active = active.iter().any(|(_, p)| {
        p.state
            .their_callsign()
            .map(|c| c.eq_ignore_ascii_case(&from_station))
            .unwrap_or(false)
            || p.metadata
                .their_callsign
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case(&from_station))
                .unwrap_or(false)
    });
    if has_active {
        return;
    }

    // Read back the resend count for logging (lock is released between the
    // commit and the send; the value can only have grown, never shrunk).
    let resend_n = completions
        .lock()
        .await
        .get(&key)
        .map(|e| e.resends)
        .unwrap_or(AUTO_73_MAX_RESENDS);

    info!(
        target: "qso",
        "auto-resending 73 to {} ({}/{}) — repeated RR73 after manual QSO completion",
        from_station, resend_n, AUTO_73_MAX_RESENDS
    );

    match qso_manager
        .respond_to_caller(
            from_station.clone(),
            frequency_hz,
            dx_parity,
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
        )
        .await
    {
        Ok(_) => {
            emit_status(
                message_bus,
                format!(
                    "Re-sending 73 to {} ({}/{}) — they repeated RR73",
                    from_station, resend_n, AUTO_73_MAX_RESENDS
                ),
            )
            .await;
        }
        Err(e) => {
            warn!(
                target: "qso",
                "auto-73 re-send to {} failed: {}", from_station, e
            );
        }
    }
}

/// Short, operator-facing description of why a QSO failed, for the TUI
/// status line (Batch 2 #3). Terminal QSOs are dropped from the active
/// snapshot, so this is the only place the operator learns the reason.
fn failure_reason_text(reason: &pancetta_qso::QsoFailureReason) -> String {
    use pancetta_qso::QsoFailureReason as R;
    match reason {
        R::Timeout => "watchdog timeout".to_string(),
        R::SignalLost => "signal lost".to_string(),
        R::Duplicate => "duplicate".to_string(),
        R::InvalidCallsign => "invalid callsign".to_string(),
        R::FrequencyConflict => "frequency conflict".to_string(),
        R::UserCancelled => "cancelled by operator".to_string(),
        R::Superseded => "superseded by a newer call".to_string(),
        R::StationQrt => "station went QRT".to_string(),
        R::ProtocolError(e) => format!("protocol error: {e}"),
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
        // Snapshot the opt-in QSO-upload settings. Only when at least one is
        // enabled do we build clients + spawn the upload subscriber.
        let clublog_cfg = config.network.clublog.clone();
        let qrz_cfg = config.network.qrz_logbook.clone();
        drop(config);

        let upload_enabled = clublog_cfg.enabled || qrz_cfg.enabled;

        let qso_lookup = self.cached_lookup.clone();
        let cqdx_bridge = self.cqdx_bridge.clone();
        let upload_our_callsign = our_callsign.clone();
        let active_qso_ap = self.active_qso_ap.clone();
        let active_qso_freq_hz = self.active_qso_freq_hz.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        // Shared with the TX worker — drives the "drop TX for ended QSOs"
        // gate. The QSO component keeps it in sync from the QsoEvent stream
        // below.
        let active_tx_qsos = self.active_tx_qsos.clone();
        // Global TX policy — the auto-73 re-send respects it (RESPOND-ONLY
        // allows, DISABLED blocks), exactly like every other response path.
        let tx_policy = self.tx_policy.clone();
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{LoggerConfig, QsoManager, QsoManagerConfig};

                let qso_config = QsoManagerConfig {
                    our_callsign: our_callsign.clone(),
                    our_grid: our_grid.clone(),
                    ..Default::default()
                };

                let mut qso_manager = QsoManager::new(qso_config);
                // Share the rig dial-frequency source so completed QSOs log the
                // real RF frequency (dial + audio offset), not the bare offset
                // (was producing ADIF FREQ ~0.001 / BAND 0MHZ).
                qso_manager.set_dial_frequency_source(operating_frequency_hz.clone());
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

                // Per-QSO log-upload subscriber (ClubLog + QRZ Logbook).
                // Opt-in: only spawned when at least one is enabled. Best-effort
                // and fully decoupled from the QSO pipeline — each upload runs in
                // its own task so a slow/failing service never blocks logging.
                if upload_enabled {
                    start_qso_upload_subscriber(
                        clublog_cfg.clone(),
                        qrz_cfg.clone(),
                        upload_our_callsign.clone(),
                        qso_manager.subscribe(),
                        shutdown.clone(),
                    );
                }

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

                // item-2-auto-73: map of recently-completed MANUAL QSOs, shared
                // between the QsoCompleted handler (in the event-forwarding task
                // below, which populates it) and the decode-processing loop
                // (which consumes it when a directed RR73/RRR arrives). See the
                // type alias / constants at the top of this module.
                let recent_manual_completions: RecentManualCompletions =
                    Arc::new(Mutex::new(HashMap::new()));

                // Spawn a task to forward QSO auto-sequence TX requests to the transmitter
                // and update AP decoding state for the FT8 decoder thread.
                let mut qso_events = qso_manager.subscribe();
                let tx_bus = message_bus.clone();
                let tx_shutdown = shutdown.clone();
                let tx_callsign = our_callsign.clone();
                let ap_state = active_qso_ap;
                let qso_freq_state = active_qso_freq_hz;
                let active_tx_qsos = active_tx_qsos.clone();
                let snapshot_qso_manager = qso_manager.clone();
                let snapshot_bus = tx_bus.clone();
                let completions_for_events = recent_manual_completions.clone();
                tokio::spawn(async move {
                    while !tx_shutdown.load(Ordering::Acquire) {
                        match qso_events.recv().await {
                            Ok(pancetta_qso::QsoEvent::StateChanged {
                                qso_id,
                                old_state,
                                new_state,
                                ..
                            }) => {
                                // Keep the TX-active set in sync (drop-stale-TX
                                // gate). A QSO entering a non-terminal active
                                // state is now allowed to TX; a QSO entering a
                                // terminal Failed state (covers Superseded /
                                // UserCancelled / Timeout / SignalLost / …) must
                                // STOP transmitting at once, so we remove it
                                // immediately. (Completion is handled in the
                                // QsoCompleted arm with a grace window so the
                                // final 73 still goes out.)
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    if new_state.is_active() {
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.insert(key);
                                        }
                                    } else if matches!(
                                        new_state,
                                        pancetta_qso::QsoState::Failed { .. }
                                    ) {
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.remove(&key);
                                        }
                                        info!(
                                            target: "tx.policy",
                                            "QSO {} went terminal-Failed — purging its TX from the active set",
                                            qso_id
                                        );
                                    }
                                }

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

                                // Batch 2 #3: a QSO that just went terminal-Failed
                                // is otherwise silently dropped from the snapshot.
                                // Surface a one-line status so the operator learns
                                // WHY (watchdog timeout, cancelled, …) instead of
                                // the QSO just vanishing. We only fire on the
                                // transition INTO Failed (old_state was not already
                                // terminal).
                                //
                                // FIX 2: a `Superseded` end is an INTENTIONAL
                                // replace, not a failure — the operator (or the
                                // engine on a genuine re-call after the old QSO
                                // went terminal) deliberately swapped one QSO for
                                // another. Surfacing it as "QSO … failed:
                                // superseded" alarmed the operator into thinking
                                // the rig was broken. So we phrase Superseded
                                // neutrally ("replaced earlier call to X") and keep
                                // the scary "failed" wording only for REAL failures
                                // (Timeout / SignalLost / StationQrt / …). With FIX
                                // 1, supersede is rare anyway.
                                if let pancetta_qso::QsoState::Failed { reason, .. } = &new_state {
                                    if !old_state.is_terminal() {
                                        let who = new_state
                                            .their_callsign()
                                            .or_else(|| old_state.their_callsign())
                                            .unwrap_or("?")
                                            .to_string();
                                        let text = if matches!(
                                            reason,
                                            pancetta_qso::QsoFailureReason::Superseded
                                        ) {
                                            format!("Replaced earlier call to {who}")
                                        } else {
                                            format!(
                                                "QSO with {} failed: {}",
                                                who,
                                                failure_reason_text(reason)
                                            )
                                        };
                                        emit_status(&snapshot_bus, text).await;
                                    }
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
                            Ok(pancetta_qso::QsoEvent::QsoCompleted {
                                qso_id, metadata, ..
                            }) => {
                                // Drop-stale-TX grace window. A normally
                                // completing QSO emits its FINAL 73 right at
                                // completion, so we must NOT purge it from the
                                // active set immediately — that would race the
                                // 73 out of existence. Instead we keep it live
                                // for one slot (~16s) via a spawned delayed
                                // task: the final 73 keys this slot, and any
                                // leftover backlog for this QSO is dropped on
                                // the next slot. 16s comfortably covers the
                                // worst-case schedule (≤16s slot wait selects
                                // THIS or the next same-parity slot for the 73)
                                // plus the 12.64s on-air burst.
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    // Ensure the key is present for the grace
                                    // window's duration. Normally a prior
                                    // active StateChanged already inserted it
                                    // (idempotent here), but a QSO that OPENS
                                    // directly at the close (respond_to_caller
                                    // SeventyThree → Completed) never passed
                                    // through an active state, so without this
                                    // insert its single final-73 TransmitRequest
                                    // would be dropped by the Step 4b gate and
                                    // never key PTT.
                                    if let Ok(mut s) = active_tx_qsos.write() {
                                        s.insert(key.clone());
                                    }
                                    let set = active_tx_qsos.clone();
                                    let qid = qso_id;
                                    tokio::spawn(async move {
                                        tokio::time::sleep(Duration::from_secs(16)).await;
                                        if let Ok(mut s) = set.write() {
                                            s.remove(&key);
                                        }
                                        info!(
                                            target: "tx.policy",
                                            "QSO {} completed — grace elapsed, purging its TX from the active set",
                                            qid
                                        );
                                    });
                                }
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

                                    // Batch 2 #4: completed QSOs are filtered out
                                    // of the active snapshot, so the operator never
                                    // saw success. Surface a one-line confirmation
                                    // with the reports exchanged (RST sent/received).
                                    let rst = |r: Option<i8>| {
                                        r.map(|v| format!("{v:+}"))
                                            .unwrap_or_else(|| "--".to_string())
                                    };
                                    emit_status(
                                        &snapshot_bus,
                                        format!(
                                            "QSO with {} logged (RST {}/{})",
                                            their_call,
                                            rst(metadata.reports.sent),
                                            rst(metadata.reports.received),
                                        ),
                                    )
                                    .await;

                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);

                                    // item-2-auto-73: stash MANUAL completions so
                                    // that if this DX keeps re-sending RR73/RRR (they
                                    // didn't copy our 73) we can auto-re-send our 73,
                                    // bounded, from the decode-processing loop below.
                                    // Autonomous completions are deliberately NOT
                                    // stashed — that path has its own dx-busy /
                                    // duplicate gates and shouldn't keep TXing 73s.
                                    if metadata.initiated_by == pancetta_qso::CallInitiation::Manual
                                    {
                                        let now = chrono::Utc::now();
                                        let entry = RecentManualCompletion {
                                            completed_at: now,
                                            frequency_hz: metadata.frequency,
                                            dx_parity: metadata.tx_parity.map(|p| p.opposite()),
                                            resends: 0,
                                            last_resend_at: None,
                                        };
                                        let mut map = completions_for_events.lock().await;
                                        // Prune stale entries while we hold the lock so
                                        // the map never grows unbounded.
                                        map.retain(|_, e| {
                                            now.signed_duration_since(e.completed_at)
                                                < AUTO_73_WINDOW
                                        });
                                        map.insert(their_call.to_uppercase(), entry);
                                    }

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
                            Ok(pancetta_qso::QsoEvent::QsoFailed {
                                qso_id, metadata, ..
                            }) => {
                                // Drop-stale-TX gate: a failed QSO must stop
                                // transmitting immediately. (StateChanged-into-
                                // Failed already purges, but a QsoFailed not
                                // preceded by such a transition would otherwise
                                // be missed — purge here too, idempotently.)
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    if let Ok(mut set) = active_tx_qsos.write() {
                                        set.remove(&key);
                                    }
                                }
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
                                            // item-2-auto-73: a directed RR73/RRR from
                                            // a station we just MANUALLY completed with
                                            // means they didn't copy our 73 — bounded
                                            // auto-re-send. Detect before process_message
                                            // moves the parsed type. The map/window/cap
                                            // gating lives in the helper.
                                            maybe_auto_resend_73(
                                                &msg_type,
                                                &our_callsign,
                                                frequency,
                                                decoded_msg.slot_parity,
                                                &qso_manager,
                                                &recent_manual_completions,
                                                &tx_policy,
                                                &message_bus,
                                            )
                                            .await;

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
                                        crate::message_bus::QsoMessage::RespondToCaller {
                                            callsign,
                                            frequency,
                                            dx_parity,
                                            step,
                                            snr,
                                        } => {
                                            info!(
                                                "Responding to caller {} on {} Hz at step {:?} \
                                                 (manual)",
                                                callsign, frequency, step
                                            );
                                            // Operator picked a station calling US from the
                                            // Callers panel and chose (or accepted the smart
                                            // default for) which sequence step to open at.
                                            // Manual call: bypasses the duplicate gate and
                                            // keep-calls under the watchdog, exactly like
                                            // StartQso — but starts at the correct rung
                                            // (their report → our R-report, etc.) instead of
                                            // always sending our grid. `their_report` is left
                                            // None; the engine defaults it.
                                            match qso_manager
                                                .respond_to_caller(
                                                    callsign.clone(),
                                                    frequency as f64,
                                                    dx_parity,
                                                    step,
                                                    snr,
                                                    None,
                                                )
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Caller-response QSO started with {}: \
                                                         {} (step {:?})",
                                                        callsign, qso_id, step
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Replying to {} — TX queued ({} Hz)",
                                                            callsign, frequency
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to respond to caller {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Reply to {} failed: {}",
                                                            callsign, e
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::LogQso { qso_data } => {
                                            debug!("Manual log QSO: {}", qso_data);
                                        }
                                        // Abort / End both cancel the QSO
                                        // (→ Failed{UserCancelled}, mapping cleared).
                                        crate::message_bus::QsoMessage::AbortQso { qso_id }
                                        | crate::message_bus::QsoMessage::EndQso { qso_id } => {
                                            match qso_id.parse::<pancetta_qso::QsoId>() {
                                                Ok(id) => {
                                                    if let Err(e) = qso_manager.cancel_qso(id).await
                                                    {
                                                        warn!(
                                                            "Failed to abort QSO {}: {}",
                                                            qso_id, e
                                                        );
                                                    } else {
                                                        info!("Aborted QSO {}", qso_id);
                                                    }
                                                }
                                                Err(e) => warn!(
                                                    "AbortQso: bad QSO id '{}': {}",
                                                    qso_id, e
                                                ),
                                            }
                                        }
                                        crate::message_bus::QsoMessage::ResendQso { qso_id } => {
                                            match qso_id.parse::<pancetta_qso::QsoId>() {
                                                Ok(id) => {
                                                    if let Err(e) =
                                                        qso_manager.resend_last_tx(id).await
                                                    {
                                                        warn!(
                                                            "Failed to re-send QSO {}: {}",
                                                            qso_id, e
                                                        );
                                                    } else {
                                                        info!("Re-sent last TX for QSO {}", qso_id);
                                                    }
                                                }
                                                Err(e) => warn!(
                                                    "ResendQso: bad QSO id '{}': {}",
                                                    qso_id, e
                                                ),
                                            }
                                        }
                                        // Cancel EVERY active QSO. This is the
                                        // loop-breaker: manual QSOs keep-call
                                        // every slot via rearm_manual_calls_at,
                                        // and per-callsign `k`/AbortQso only
                                        // clears one — duplicate QSO objects or
                                        // an unseen QSO can keep re-emitting TX
                                        // forever. The emergency stop sends this
                                        // so a single Shift+Q clears the source
                                        // (not just mutes via TX policy).
                                        crate::message_bus::QsoMessage::CancelAllQsos => {
                                            let active = qso_manager.get_active_qsos().await;
                                            let n = active.len();
                                            for (id, _) in active {
                                                if let Err(e) = qso_manager.cancel_qso(id).await {
                                                    warn!("CancelAllQsos: {} failed: {}", id, e);
                                                }
                                            }
                                            info!(
                                                target: "operator.override",
                                                "CancelAllQsos: cancelled {} active QSO(s)",
                                                n
                                            );
                                        }
                                        // Operator pressed `c`: start a manual
                                        // CQ as a tracked CallingCq QSO. The QSO
                                        // owns the CQ transmission (emits the
                                        // first CQ + keeps calling every slot
                                        // via rearm_manual_calls_at); the old
                                        // tui_relay text-only CQ loop no longer
                                        // transmits, so there is exactly one CQ
                                        // TX source per slot (no double-TX).
                                        // When a station answers, the
                                        // CallingCq → WaitingForReport arm fires
                                        // and the Manual-gated auto-reply emitter
                                        // sequences the exchange to Completed +
                                        // QsoCompleted (ADIF log).
                                        crate::message_bus::QsoMessage::StartCq {
                                            frequency,
                                            tx_parity,
                                        } => {
                                            match qso_manager
                                                .start_cq_manual(frequency as f64, tx_parity)
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Manual CQ started: {} ({} Hz, \
                                                         keep-calling under watchdog)",
                                                        qso_id, frequency
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Calling CQ — TX queued ({} Hz)",
                                                            frequency
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!("Failed to start manual CQ: {}", e);
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Start CQ failed: {}", e),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        // Operator pressed `s`: stop calling CQ.
                                        // Cancel any active QSO still in
                                        // CallingCq (un-answered). A CallingCq
                                        // QSO that already advanced past CallingCq
                                        // (a caller answered) is left running so
                                        // the in-progress exchange completes.
                                        crate::message_bus::QsoMessage::StopCq => {
                                            let active = qso_manager.get_active_qsos().await;
                                            let mut cancelled = 0usize;
                                            for (id, progress) in active {
                                                if matches!(
                                                    progress.state,
                                                    pancetta_qso::QsoState::CallingCq { .. }
                                                ) {
                                                    if let Err(e) = qso_manager.cancel_qso(id).await
                                                    {
                                                        warn!("StopCq: {} failed: {}", id, e);
                                                    } else {
                                                        cancelled += 1;
                                                    }
                                                }
                                            }
                                            info!(
                                                "StopCq: cancelled {} un-answered CQ QSO(s)",
                                                cancelled
                                            );
                                        }
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
    // Watchdog config for the manual keep-calling countdown (Batch 2 #1).
    let timeouts = &qso_manager.config().timeouts;
    let max_calls = timeouts.manual_call_max_calls;
    let watchdog_minutes = timeouts.manual_call_watchdog_minutes;

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

    // Batch 2 #5: emit in a STABLE order (start_time, then callsign). The
    // HashMap iteration order is non-deterministic, which made multi-QSO row
    // order jump between snapshots — a positional cursor then pointed at a
    // different QSO each frame. The TUI also pins its selection by qso_id, but
    // a stable emit order keeps the visible list from reshuffling.
    let mut progresses: Vec<pancetta_qso::QsoProgress> = latest.into_values().collect();
    progresses.sort_by(|a, b| {
        a.metadata
            .start_time
            .cmp(&b.metadata.start_time)
            .then_with(|| {
                let ca = a.state.their_callsign().unwrap_or("");
                let cb = b.state.their_callsign().unwrap_or("");
                ca.cmp(cb)
            })
    });

    progresses
        .iter()
        .filter_map(|p| snapshot_item_from_progress(p, max_calls, watchdog_minutes))
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
/// `max_calls` / `watchdog_minutes` come from the QSO manager's
/// `TimeoutConfig`; they populate the manual keep-calling countdown
/// fields (`call_count`/`max_calls`/`watchdog_deadline`), which are only
/// meaningful while the QSO is in a manual keep-calling state
/// (RespondingToCq / SendingReport).
fn snapshot_item_from_progress(
    progress: &pancetta_qso::QsoProgress,
    max_calls: u32,
    watchdog_minutes: u64,
) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
    use pancetta_qso::{CallInitiation, MessageDirection, QsoState};
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

    let initiated_by = match progress.metadata.initiated_by {
        pancetta_qso::CallInitiation::Manual => "Manual",
        pancetta_qso::CallInitiation::Auto => "Auto",
    }
    .to_string();

    // Derive the role-aware display ladder + now/next lines. Terminal/Idle/
    // Contest states return None (shouldn't appear in the active set, but we
    // handle it by leaving the ladder empty and now/next blank). The role
    // (CQer vs Caller) is latched on the QSO at creation and disambiguates the
    // shared middle states (Batch 2 #6).
    let ladder = progress.state.ladder_view(progress.metadata.role);
    let (ladder_labels, ladder_ours, ladder_index, now_line, next_line) = match ladder {
        Some(v) => (
            v.labels.iter().map(|s| s.to_string()).collect(),
            v.ours,
            v.index,
            v.now,
            v.next,
        ),
        None => (Vec::new(), Vec::new(), 0, String::new(), String::new()),
    };

    // Manual keep-calling watchdog visibility (Batch 2 #1). Only meaningful
    // while a MANUAL QSO is in a keep-calling state (RespondingToCq /
    // SendingReport); otherwise zero/None so the TUI shows nothing misleading.
    let keep_calling = progress.metadata.initiated_by == CallInitiation::Manual
        && matches!(
            progress.state,
            QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. }
        );
    let (wd_call_count, wd_max_calls, watchdog_deadline) = if keep_calling {
        let deadline = progress
            .metadata
            .first_call_at
            .map(|t| t + chrono::Duration::minutes(watchdog_minutes as i64));
        (progress.metadata.call_count, max_calls, deadline)
    } else {
        (0, 0, None)
    };

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
        qso_id: progress.metadata.qso_id.to_string(),
        initiated_by,
        ladder_labels,
        ladder_ours,
        ladder_index,
        now_line,
        next_line,
        call_count: wd_call_count,
        max_calls: wd_max_calls,
        watchdog_deadline,
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
                role: Default::default(),
                call_count: 0,
                first_call_at: None,
                last_call_at: None,
                progressed_this_cycle: false,
            },
        }
    }

    /// Default watchdog config for snapshot tests (matches TimeoutConfig
    /// defaults: 10 calls / 5 minutes).
    const TEST_MAX_CALLS: u32 = 10;
    const TEST_WATCHDOG_MIN: u64 = 5;

    /// Thin wrapper so the existing tests don't each repeat the watchdog args.
    fn snap(progress: &QsoProgress) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
        snapshot_item_from_progress(progress, TEST_MAX_CALLS, TEST_WATCHDOG_MIN)
    }

    /// All detail-panel fields derive from state the engine already
    /// tracks: last message per direction, measured RX SNR, reports,
    /// exchange count, plus the original banner fields.
    #[test]
    fn snapshot_derives_detail_fields_from_progress() {
        let item = snap(&fixture_progress()).expect("item");
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
        let item = snap(&progress).expect("item");
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
        assert!(snap(&progress).is_none());
    }

    /// A QSO with no messages yet (just started) still produces an item
    /// with empty detail fields — the panel renders placeholders.
    #[test]
    fn snapshot_handles_empty_message_history() {
        let mut progress = fixture_progress();
        progress.messages.clear();
        let item = snap(&progress).expect("item");
        assert!(item.last_tx_text.is_none());
        assert!(item.last_rx_text.is_none());
        assert!(item.snr_rx.is_none());
        assert_eq!(item.exchange_count, 0);
    }

    /// Batch 2 #1: a MANUAL QSO in a keep-calling state surfaces the
    /// watchdog countdown fields (call N/M + deadline).
    #[test]
    fn snapshot_surfaces_watchdog_for_manual_keep_calling() {
        let mut progress = fixture_progress();
        let start = Utc::now() - Duration::seconds(20);
        progress.state = QsoState::RespondingToCq {
            target_callsign: "JA1ABC".to_string(),
            frequency: 1500.0,
            started_at: start,
        };
        progress.metadata.initiated_by = pancetta_qso::CallInitiation::Manual;
        progress.metadata.call_count = 4;
        progress.metadata.first_call_at = Some(start);
        let item = snap(&progress).expect("item");
        assert_eq!(item.call_count, 4);
        assert_eq!(item.max_calls, TEST_MAX_CALLS);
        let deadline = item.watchdog_deadline.expect("deadline");
        assert_eq!(
            deadline,
            start + Duration::minutes(TEST_WATCHDOG_MIN as i64)
        );
    }

    /// An AUTO QSO (or a manual QSO past the keep-calling phase) shows no
    /// watchdog fields — they would be misleading.
    #[test]
    fn snapshot_no_watchdog_for_auto_qso() {
        let mut progress = fixture_progress();
        progress.metadata.initiated_by = pancetta_qso::CallInitiation::Auto;
        progress.metadata.call_count = 3;
        progress.metadata.first_call_at = Some(Utc::now());
        let item = snap(&progress).expect("item");
        assert_eq!(item.call_count, 0);
        assert_eq!(item.max_calls, 0);
        assert!(item.watchdog_deadline.is_none());
    }

    /// Batch 2 #3: every failure reason maps to an operator-readable string.
    #[test]
    fn failure_reason_text_is_human_readable() {
        use pancetta_qso::QsoFailureReason as R;
        assert_eq!(super::failure_reason_text(&R::Timeout), "watchdog timeout");
        assert_eq!(
            super::failure_reason_text(&R::Superseded),
            "superseded by a newer call"
        );
        assert_eq!(
            super::failure_reason_text(&R::UserCancelled),
            "cancelled by operator"
        );
        assert_eq!(
            super::failure_reason_text(&R::ProtocolError("boom".to_string())),
            "protocol error: boom"
        );
    }
}

#[cfg(test)]
mod auto_73_tests {
    use super::{
        maybe_auto_resend_73, RecentManualCompletion, RecentManualCompletions, AUTO_73_MAX_RESENDS,
        AUTO_73_WINDOW,
    };
    use crate::message_bus::MessageBus;
    use pancetta_core::slot::SlotParity;
    use pancetta_core::TxPolicy;
    use pancetta_qso::states::MessageType as Mt;
    use pancetta_qso::{QsoManager, QsoManagerConfig};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU8;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const OUR: &str = "K5ARH";
    const DX: &str = "JA1ABC";

    async fn manager() -> QsoManager {
        let mut m = QsoManager::new(QsoManagerConfig {
            our_callsign: OUR.to_string(),
            our_grid: Some("EM10".to_string()),
            ..Default::default()
        });
        m.start().await.expect("manager start");
        m
    }

    fn bus() -> MessageBus {
        MessageBus::new(1000).expect("bus")
    }

    /// A completions map containing a single fresh manual completion for `DX`.
    fn map_with_dx() -> RecentManualCompletions {
        let mut map = HashMap::new();
        map.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now(),
                frequency_hz: 1500.0,
                dx_parity: Some(SlotParity::Even),
                resends: 0,
                last_resend_at: None,
            },
        );
        Arc::new(Mutex::new(map))
    }

    fn rr73_to_us() -> Mt {
        Mt::FinalConfirmation {
            to_station: OUR.to_string(),
            from_station: DX.to_string(),
        }
    }

    /// Count `MessageToSend` events the manager has emitted by draining a
    /// subscriber that was attached before the action under test.
    fn drain_sends(rx: &mut tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>) -> usize {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, pancetta_qso::QsoEvent::MessageToSend { .. }) {
                n += 1;
            }
        }
        n
    }

    /// A directed RR73 from a stashed manual completion triggers exactly one
    /// auto-73 per slot, and never more than `AUTO_73_MAX_RESENDS` total even
    /// if RR73 arrives every slot.
    #[tokio::test]
    async fn bound_holds_under_repeated_rr73_every_slot() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        // Simulate the DX hammering RR73 across many slots. We bypass the
        // per-slot guard by zeroing last_resend_at between calls — that proves
        // the HARD cap (resends) holds independently of the time guard.
        for _ in 0..10 {
            maybe_auto_resend_73(
                &rr73_to_us(),
                OUR,
                1500.0,
                Some(SlotParity::Even),
                &mgr,
                &map,
                &policy,
                &bus,
            )
            .await;
            if let Some(e) = map.lock().await.get_mut(DX) {
                e.last_resend_at = None; // defeat the per-slot guard for this test
            }
        }

        let sends = drain_sends(&mut rx);
        assert_eq!(
            sends as u8, AUTO_73_MAX_RESENDS,
            "auto-73 must be capped at {AUTO_73_MAX_RESENDS}, got {sends}"
        );
        // After the cap the entry is dropped so it can never fire again.
        assert!(map.lock().await.get(DX).is_none());
    }

    /// Within one slot, two decodes of the same RR73 fire only ONE auto-73.
    #[tokio::test]
    async fn one_per_slot_dedup() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        for _ in 0..3 {
            maybe_auto_resend_73(
                &rr73_to_us(),
                OUR,
                1500.0,
                Some(SlotParity::Even),
                &mgr,
                &map,
                &policy,
                &bus,
            )
            .await;
            // Do NOT reset last_resend_at — same slot.
        }

        assert_eq!(drain_sends(&mut rx), 1, "only one 73 per slot");
        assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(1));
    }

    /// An RR73 outside the 3-minute window never triggers an auto-73 (the
    /// entry is pruned on lookup).
    #[tokio::test]
    async fn outside_window_no_resend() {
        let mgr = manager().await;
        let map = {
            let mut m = HashMap::new();
            m.insert(
                DX.to_string(),
                RecentManualCompletion {
                    completed_at: chrono::Utc::now()
                        - AUTO_73_WINDOW
                        - chrono::Duration::seconds(1),
                    frequency_hz: 1500.0,
                    dx_parity: Some(SlotParity::Even),
                    resends: 0,
                    last_resend_at: None,
                },
            );
            Arc::new(Mutex::new(m))
        };
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
        assert!(map.lock().await.get(DX).is_none(), "stale entry pruned");
    }

    /// While a QSO with the DX is active, no auto-73 (don't fight a live QSO).
    #[tokio::test]
    async fn active_qso_no_resend() {
        let mgr = manager().await;
        // Open a live QSO with DX (RespondingToCq via manual call).
        mgr.respond_to_cq_manual(DX.to_string(), 1500.0, Some(SlotParity::Even))
            .await
            .expect("start qso");
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        // Subscribe AFTER the manual call so its MessageToSend is not counted;
        // we only want to observe whether the auto-73 fires.
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0, "no auto-73 while QSO active");
    }

    /// A station NOT in the map (e.g. an AUTONOMOUS-completed QSO, which the
    /// QsoCompleted handler never stashes) gets no auto-73.
    #[tokio::test]
    async fn not_in_map_no_resend() {
        let mgr = manager().await;
        let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }

    /// TX policy DISABLED blocks the auto-73 entirely (and does not consume
    /// the resend budget).
    #[tokio::test]
    async fn disabled_policy_no_resend() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Disabled.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0, "DISABLED blocks auto-73");
        assert_eq!(
            map.lock().await.get(DX).map(|e| e.resends),
            Some(0),
            "budget untouched under DISABLED"
        );
    }

    /// RESPOND-ONLY allows the auto-73 (it's a response, not an initiation).
    #[tokio::test]
    async fn respond_only_allows_resend() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::RespondOnly.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 1, "RESPOND-ONLY permits the 73");
    }

    /// A non-close message (e.g. a signal report) directed at us never
    /// triggers an auto-73, even from a stashed callsign.
    #[tokio::test]
    async fn non_close_message_ignored() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        let report = Mt::SignalReport {
            to_station: OUR.to_string(),
            from_station: DX.to_string(),
            report: -12,
        };
        maybe_auto_resend_73(
            &report,
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }

    /// An RR73 NOT directed at us (to a third party) is ignored.
    #[tokio::test]
    async fn rr73_to_third_party_ignored() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        let rr73 = Mt::FinalConfirmation {
            to_station: "W1XYZ".to_string(),
            from_station: DX.to_string(),
        };
        maybe_auto_resend_73(
            &rr73,
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }
}

/// Spawn a background task that listens for `QsoEvent::QsoCompleted` and
/// appends one ADIF record to the durable log for each completed QSO.
///
/// ADIF is the source of truth: a failed write is logged at ERROR level because
/// it indicates a real problem (disk full, permissions, etc.) that the operator
/// should investigate. The task handles receiver lag and channel closure
/// gracefully so it never blocks or panics.
/// Spawn a background task that uploads each completed QSO to the operator's
/// online logbooks (ClubLog and/or QRZ Logbook), one ADIF record per QSO.
///
/// The single ADIF record is rendered exactly as the source-of-truth ADIF
/// writer renders it (`AdifProcessor::qso_to_adif` → `generate_record`), so the
/// uploaded record matches `~/.pancetta/qsos.adi`.
///
/// Best-effort by design: uploads are decoupled from the QSO pipeline and never
/// block it. Each per-service upload is spawned in its own task. Successes log
/// at `info!`, duplicates at `info!`, failures at `warn!` (target
/// `"qso.upload"`). Credentials are never logged.
fn start_qso_upload_subscriber(
    clublog_cfg: pancetta_config::network::ClubLogConfig,
    qrz_cfg: pancetta_config::network::QrzLogbookConfig,
    our_callsign: String,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::Arc;

    // Build the enabled clients once and share them across uploads.
    let clublog_client = if clublog_cfg.enabled {
        // Fall back to the QSO's own call when no station call is configured.
        let callsign = if clublog_cfg.callsign.is_empty() {
            our_callsign.clone()
        } else {
            clublog_cfg.callsign.clone()
        };
        Some(Arc::new(pancetta_dx::ClubLogClient::new(
            clublog_cfg.email.clone(),
            clublog_cfg.password.clone(),
            callsign,
            clublog_cfg.api_key.clone(),
        )))
    } else {
        None
    };

    let qrz_client = if qrz_cfg.enabled {
        Some(Arc::new(pancetta_dx::QrzLogbookClient::new(
            qrz_cfg.api_key.clone(),
        )))
    } else {
        None
    };

    if clublog_client.is_some() {
        info!(target: "qso.upload", "ClubLog per-QSO upload enabled");
    }
    if qrz_client.is_some() {
        info!(target: "qso.upload", "QRZ Logbook per-QSO upload enabled");
    }

    tokio::spawn(async move {
        let processor = pancetta_qso::AdifProcessor::new();

        while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
            match events.recv().await {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                    // Render the single ADIF record the same way the
                    // source-of-truth writer does.
                    let adif_qso = processor.qso_to_adif(&metadata, metadata.contest_info.as_ref());
                    let record = match processor.generate_record(&adif_qso) {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                target: "qso.upload",
                                "Skipping upload for QSO {}: ADIF render failed: {}",
                                metadata.qso_id, e
                            );
                            continue;
                        }
                    };

                    let their = metadata
                        .their_callsign
                        .clone()
                        .unwrap_or_else(|| "?".to_string());

                    if let Some(client) = clublog_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(()) => info!(
                                    target: "qso.upload",
                                    "ClubLog: uploaded QSO with {}", their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "ClubLog: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }

                    if let Some(client) = qrz_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(pancetta_dx::QrzInsertOutcome::Inserted { logid }) => info!(
                                    target: "qso.upload",
                                    "QRZ: uploaded QSO with {} (logid={})",
                                    their,
                                    logid.as_deref().unwrap_or("?")
                                ),
                                Ok(pancetta_dx::QrzInsertOutcome::Duplicate { .. }) => info!(
                                    target: "qso.upload",
                                    "QRZ: QSO with {} already logged (duplicate, skipped)",
                                    their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "QRZ: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(target: "qso.upload", "QSO upload subscriber lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

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
