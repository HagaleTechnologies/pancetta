//! QSO state machine and management
//!
//! This module provides the core QSO management functionality including
//! state transitions, timeout handling, and QSO lifecycle management.

use crate::async_database::AsyncQsoDatabase;
use crate::states::*;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration as TokioDuration, Interval};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// QSO management errors
#[derive(Debug, Error)]
pub enum QsoManagerError {
    #[error("QSO not found: {qso_id}")]
    QsoNotFound { qso_id: QsoId },

    #[error("Invalid state transition from {from:?} to {to:?}")]
    InvalidTransition { from: QsoState, to: QsoState },

    #[error("QSO already exists for callsign {callsign} on frequency {frequency}")]
    DuplicateQso { callsign: String, frequency: f64 },

    #[error("Invalid callsign format: {callsign}")]
    InvalidCallsign { callsign: String },

    #[error("QSO timeout: {reason}")]
    Timeout { reason: String },

    #[error("Configuration error: {message}")]
    Configuration { message: String },

    #[error("Database error: {source}")]
    Database { source: anyhow::Error },
}

/// QSO manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoManagerConfig {
    /// Our station callsign
    pub our_callsign: String,

    /// Our grid square
    pub our_grid: Option<GridSquare>,

    /// Timeout settings
    pub timeouts: TimeoutConfig,

    /// Contest mode settings
    pub contest_mode: Option<ContestConfig>,

    /// Automatic sequencing configuration
    pub auto_sequence: AutoSequenceConfig,

    /// Duplicate checking settings
    pub duplicate_checking: DuplicateCheckConfig,
}

/// Timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Timeout for CQ calls (seconds)
    pub cq_timeout: u64,

    /// Timeout for waiting for report (seconds)
    pub report_timeout: u64,

    /// Timeout for waiting for confirmation (seconds)
    pub confirmation_timeout: u64,

    /// Maximum QSO duration (seconds)
    pub max_qso_duration: u64,

    /// Cleanup interval for completed QSOs (seconds)
    pub cleanup_interval: u64,

    /// Manual keep-calling watchdog: stop calling after this many minutes
    /// have elapsed since the first manual call, regardless of call count.
    /// Whichever of this and `manual_call_max_calls` fires first ends the
    /// manual call attempt. Default: 5 minutes.
    pub manual_call_watchdog_minutes: u64,

    /// Manual keep-calling watchdog: stop after transmitting this many
    /// calls to the DX. Whichever of this and `manual_call_watchdog_minutes`
    /// fires first ends the manual call attempt. Default: 10 calls.
    pub manual_call_max_calls: u32,
}

/// Contest configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestConfig {
    /// Contest name
    pub contest_name: String,

    /// Contest category
    pub category: String,

    /// Starting serial number
    pub starting_serial: SerialNumber,

    /// Enable contest mode
    pub enabled: bool,
}

/// Automatic sequencing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSequenceConfig {
    /// Enable automatic sequencing
    pub enabled: bool,

    /// Automatically respond to CQ calls
    pub auto_respond_cq: bool,

    /// Automatically send reports
    pub auto_send_reports: bool,

    /// Automatically send confirmations
    pub auto_send_confirmations: bool,

    /// Delay between automatic actions (milliseconds)
    pub action_delay_ms: u64,
}

/// Duplicate checking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateCheckConfig {
    /// Enable duplicate checking
    pub enabled: bool,

    /// Check duplicates within this time window (hours)
    pub time_window_hours: u32,

    /// Check duplicates on same frequency
    pub check_frequency: bool,

    /// Check duplicates on same band
    pub check_band: bool,
}

/// QSO event notifications
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QsoEvent {
    /// QSO state changed
    StateChanged {
        qso_id: QsoId,
        old_state: QsoState,
        new_state: QsoState,
        timestamp: DateTime<Utc>,
    },

    /// Message received
    MessageReceived { qso_id: QsoId, message: QsoMessage },

    /// Message should be sent
    MessageToSend {
        qso_id: QsoId,
        message: MessageType,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },

    /// QSO completed
    QsoCompleted {
        qso_id: QsoId,
        metadata: QsoMetadata,
    },

    /// QSO failed
    QsoFailed {
        qso_id: QsoId,
        reason: QsoFailureReason,
        metadata: QsoMetadata,
    },

    /// Duplicate QSO detected
    DuplicateDetected {
        qso_id: QsoId,
        original_qso_id: QsoId,
        callsign: String,
    },
}

impl Default for QsoManagerConfig {
    fn default() -> Self {
        Self {
            our_callsign: "NOCALL".to_string(),
            our_grid: None,
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        }
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            cq_timeout: 30,
            report_timeout: 30,
            confirmation_timeout: 30,
            max_qso_duration: 300,
            cleanup_interval: 60,
            manual_call_watchdog_minutes: 5,
            manual_call_max_calls: 10,
        }
    }
}

impl Default for AutoSequenceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_respond_cq: false,
            auto_send_reports: false,
            auto_send_confirmations: false,
            action_delay_ms: 1000,
        }
    }
}

impl Default for DuplicateCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time_window_hours: 24,
            check_frequency: true,
            check_band: false,
        }
    }
}

/// QSO manager implementation
pub struct QsoManager {
    /// Configuration
    config: QsoManagerConfig,

    /// Active QSOs by ID
    qsos: Arc<RwLock<HashMap<QsoId, QsoProgress>>>,

    /// QSOs by callsign for duplicate checking
    qsos_by_callsign: Arc<RwLock<HashMap<String, Vec<QsoId>>>>,

    /// Event broadcaster
    event_sender: broadcast::Sender<QsoEvent>,

    /// Next contest serial number
    next_serial: Arc<RwLock<SerialNumber>>,

    /// Cleanup interval timer
    cleanup_interval: Arc<RwLock<Option<Interval>>>,

    /// Optional database for persistent duplicate checking
    database: Option<Arc<AsyncQsoDatabase>>,

    /// Rig dial frequency in Hz, shared from the coordinator's hamlib poll
    /// (0 if unknown / no rig). `metadata.frequency` holds the *audio offset*;
    /// the logged RF frequency of a completed QSO is `dial + offset` (WSJT-X
    /// convention). Used only when stamping completed-QSO metadata so the ADIF
    /// records a real FREQ/BAND instead of the bare offset.
    dial_frequency_hz: Arc<AtomicU64>,
}

impl QsoManager {
    /// Create a new QSO manager
    pub fn new(config: QsoManagerConfig) -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        let next_serial = config
            .contest_mode
            .as_ref()
            .map(|c| c.starting_serial)
            .unwrap_or(1);

        Self {
            config,
            qsos: Arc::new(RwLock::new(HashMap::new())),
            qsos_by_callsign: Arc::new(RwLock::new(HashMap::new())),
            event_sender,
            next_serial: Arc::new(RwLock::new(next_serial)),
            cleanup_interval: Arc::new(RwLock::new(None)),
            database: None,
            dial_frequency_hz: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Share the coordinator's rig dial-frequency source so completed QSOs log
    /// the true RF frequency (dial + audio offset) instead of the bare offset.
    /// Pass the same `Arc<AtomicU64>` the hamlib poll loop updates; if never
    /// called, completed metadata keeps the offset value (e.g. unit tests).
    pub fn set_dial_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.dial_frequency_hz = source;
    }

    /// Create a new QSO manager with a database for persistent duplicate checking
    pub fn with_database(config: QsoManagerConfig, database: Arc<AsyncQsoDatabase>) -> Self {
        let mut manager = Self::new(config);
        manager.database = Some(database);
        manager
    }

    /// Get the configuration
    pub fn config(&self) -> &QsoManagerConfig {
        &self.config
    }

    /// Start the QSO manager
    pub async fn start(&self) -> Result<(), QsoManagerError> {
        info!("Starting QSO manager for {}", self.config.our_callsign);

        // Start cleanup timer
        let cleanup_duration = TokioDuration::from_secs(self.config.timeouts.cleanup_interval);
        let interval_timer = interval(cleanup_duration);
        *self.cleanup_interval.write().await = Some(interval_timer);

        // Start background tasks
        let manager = self.clone();
        tokio::spawn(async move {
            manager.cleanup_loop().await;
        });

        let manager = self.clone();
        tokio::spawn(async move {
            manager.timeout_check_loop().await;
        });

        Ok(())
    }

    /// Subscribe to QSO events
    pub fn subscribe(&self) -> broadcast::Receiver<QsoEvent> {
        self.event_sender.subscribe()
    }

    /// Start a new CQ call
    /// Start a CQ call.
    ///
    /// `tx_parity` is the parity we want our CQ to land on. `None`
    /// lets the TX scheduler pick (using the configured self-parity
    /// fallback). Callers driving auto-CQ from the autonomous operator
    /// will typically supply a fixed parity to keep cycles consistent.
    pub async fn start_cq(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let qso_id = Uuid::new_v4();
        let now = Utc::now();

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            // Calling CQ is not a manual keep-calling QSO; it has its own
            // CallingCq timeout and call_count in the state itself.
            initiated_by: CallInitiation::Auto,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit CQ message
        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;

        info!("Started CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Start a **manual** (operator-initiated) CQ call as a real QSO.
    ///
    /// This is the engine half of the TUI `c` (StartCq) key. Unlike
    /// [`Self::start_cq`] (which marks the QSO [`CallInitiation::Auto`] for
    /// autonomous CQ), this marks it [`CallInitiation::Manual`] so that:
    ///
    /// 1. **We keep calling CQ every slot** until a station answers or the
    ///    CQ watchdog fires — [`Self::rearm_manual_calls_at`] re-emits a
    ///    `Cq` `MessageToSend` for a manual `CallingCq` QSO once per FT8
    ///    slot, bounded by `manual_call_max_calls` /
    ///    `manual_call_watchdog_minutes` (see [`Self::check_timeouts_at`]).
    /// 2. **When a caller answers, the exchange auto-sequences to
    ///    Completed + logs** — the auto-reply emitter in
    ///    [`Self::process_message`] is gated on `CallInitiation::Manual`, so
    ///    a manual CQer (us) automatically replies with our report → RR73 as
    ///    the caller's CqResponse → ReportAck arrive, exactly like the
    ///    operator-driven Callers path.
    ///
    /// We emit a `StateChanged` (Idle → CallingCq) so the coordinator's
    /// drop-stale-TX gate keys this QSO into `active_tx_qsos` (otherwise the
    /// TX worker would refuse to key PTT for it), and emit the first `Cq`
    /// `MessageToSend` immediately. `last_call_at` is stamped `now` so the
    /// per-slot rearm does not double-send the first CQ within the opening
    /// slot.
    ///
    /// `tx_parity` is the parity we want our CQ to land on; `None` lets the
    /// TX scheduler pick using the configured self-parity fallback. (Calling
    /// CQ, we choose our own slot parity — there is no DX parity to oppose.)
    pub async fn start_cq_manual(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let qso_id = Uuid::new_v4();
        let now = Utc::now();

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        let raw_text = self.render_sent_text(&message);
        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            // Operator pressed `c`: this is a MANUAL CQ. The manual
            // keep-calling watchdog re-arms our CQ every slot, and the
            // CallInitiation::Manual gate turns on the auto-reply emitter so
            // an answering station drives the exchange to completion.
            initiated_by: CallInitiation::Manual,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            // Record the opening CQ as a Sent message so the TUI last-TX line
            // and `resend_last_tx` see it.
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit a state change (Idle → CallingCq) so the coordinator's
        // drop-stale-TX gate keys this QSO into `active_tx_qsos`; without it
        // the TX worker would drop our CQ as "stale TX for an ended QSO".
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        // Emit the first CQ. Subsequent slots are owned by the per-slot
        // manual keep-call rearm (`rearm_manual_calls_at`).
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;

        info!("Started manual CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Respond to a CQ call (autonomous/internal path).
    ///
    /// `dx_parity` is the slot parity of the DX station's CQ, used to
    /// derive our `tx_parity` (opposite of theirs). May be `None` if
    /// the CQ came from a DX cluster spot rather than an on-air decode.
    ///
    /// This is the autonomous path: the self-duplicate gate applies and
    /// there is no manual keep-calling. For operator-initiated manual
    /// calls use [`Self::respond_to_cq_manual`] (or
    /// [`Self::respond_to_cq_with`] with [`CallInitiation::Manual`]).
    pub async fn respond_to_cq(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(target_callsign, frequency, dx_parity, CallInitiation::Auto)
            .await
    }

    /// Respond to a CQ call as an operator-initiated **manual** call.
    ///
    /// Bypasses the self-duplicate gate (the operator explicitly chose to
    /// call this station, e.g. to re-work it) and marks the QSO so the
    /// manual keep-calling watchdog re-arms a call every TX slot until the
    /// DX answers or the watchdog fires.
    pub async fn respond_to_cq_manual(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Manual,
        )
        .await
    }

    /// Respond to a CQ call, explicitly choosing the initiation mode.
    ///
    /// [`CallInitiation::Auto`] preserves the historical behavior
    /// (duplicate gate enforced, no keep-calling). [`CallInitiation::Manual`]
    /// bypasses the duplicate gate and enables manual keep-calling.
    pub async fn respond_to_cq_with(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        initiated_by: CallInitiation,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        // Check for duplicate — but only for autonomous calls. A manual
        // call is an explicit operator decision to work (or re-work) this
        // station, so the self-duplicate gate must not block it.
        if initiated_by == CallInitiation::Auto
            && self.check_duplicate(&target_callsign, frequency).await?
        {
            return Err(QsoManagerError::DuplicateQso {
                callsign: target_callsign,
                frequency,
            });
        }

        // FIX 1: re-calling a station we are ALREADY actively working CONTINUES
        // that QSO instead of superseding it / spawning a duplicate. The
        // on-air failure mode this guards against: an operator mashing Space on
        // one DX previously created a brand-new QSO each press (each
        // superseding the last from the grid step), flooding the single TX
        // worker with stale frames and surfacing the intentional supersede as a
        // scary "QSO … failed: superseded". Now a re-call of an active manual
        // QSO on the same band is an idempotent keep-call: re-send the existing
        // QSO's CURRENT outbound message and return its id. State is untouched
        // (we do NOT reset to RespondingToCq/grid). Only when there is NO active
        // QSO do we fall through to create one (and the genuine
        // re-call-after-terminal case still supersedes any leftover).
        if initiated_by == CallInitiation::Manual {
            if let Some(existing_id) = self
                .find_active_manual_qso_for(&target_callsign, frequency)
                .await
            {
                info!(
                    "Re-call of {} on {:.1} Hz — continuing existing QSO {} (idempotent keep-call, no new QSO)",
                    target_callsign, frequency, existing_id
                );
                // Re-emit the QSO's most-recent outbound as a keep-call. This
                // is a benign no-op if it somehow has no prior Sent message.
                let _ = self.resend_last_tx(existing_id).await;
                return Ok(existing_id);
            }
        }

        // FIX 3: supersede any existing active QSO with this callsign on the
        // same band before creating the new one. With FIX 1 above this should
        // now only ever fire for the genuine case (the older QSO already went
        // terminal but its mapping/record lingered before cleanup). Operator
        // policy: "if there are two exchanges on the same band from the same
        // callsign, use the state of whichever is more recent." We retire the
        // older one (→ Failed{Superseded}, mapping removed) so exactly one QSO
        // per (callsign, band) remains active.
        self.supersede_active_qsos_for(&target_callsign, frequency)
            .await;

        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let tx_parity = dx_parity.map(|p| p.opposite());

        let state = QsoState::RespondingToCq {
            target_callsign: target_callsign.clone(),
            frequency,
            started_at: now,
        };

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target_callsign.clone()),
            frequency,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by,
            // We answered the DX's CQ → Caller role.
            role: QsoRole::Caller,
            // The first call is emitted immediately below (the CqResponse
            // MessageToSend), so the count starts at 1.
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
        };

        // Send response message
        let message = MessageType::CqResponse {
            calling_station: target_callsign.clone(),
            responding_station: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        // Record the initial outbound call as a Sent message so it can be
        // re-sent later (see `resend_last_tx`) and surfaced to the UI. The
        // raw_text is the rendered FT8 text so the TUI "TX:" line shows what
        // we sent (UX audit Batch 2 — was String::new() → blank line).
        let raw_text = self.render_sent_text(&message);
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target_callsign, qso_id).await;

        // Announce the QSO's birth into its initial active state BEFORE the
        // first MessageToSend. The coordinator's `active_tx_qsos` populater
        // inserts a qso_id only on `StateChanged` into an active state; the TX
        // worker's drop-stale-TX gate (Step 4b) then drops any TransmitRequest
        // whose qso_id is absent. Both the StateChanged and the MessageToSend
        // are consumed by the SAME serial event loop in the coordinator, so
        // emitting StateChanged first guarantees the insert is ordered ahead of
        // the TransmitRequest the MessageToSend produces — otherwise the very
        // first scheduled call is silently dropped and PTT never keys (the
        // operator-reported "scheduled QSO never keys PTT, but manual/tune
        // do" bug).
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;

        info!(
            "Responding to CQ from {} on {:.1} Hz: {}",
            target_callsign, frequency, qso_id
        );

        Ok(qso_id)
    }

    /// Respond to a station **calling us**, opening the exchange at an
    /// operator-chosen [`ResponseStep`] instead of always sending our grid.
    ///
    /// This is the engine half of the TUI "Callers" panel. The operator picks
    /// a caller and pancetta classifies what they sent (their CQ/grid →
    /// `Grid`, their report → `ReportAck`, etc.), with a manual override. We
    /// reuse all of [`respond_to_cq_with`](Self::respond_to_cq_with)'s manual
    /// machinery — self-duplicate-gate bypass, superseding a same-call QSO,
    /// latching `tx_parity = dx_parity.opposite()`, and per-slot keep-calling
    /// under the manual watchdog — but set the *initial* [`QsoState`] and emit
    /// the *first* [`MessageType`] according to `step`:
    ///
    /// | step         | initial state          | first message      |
    /// |--------------|------------------------|--------------------|
    /// | `Grid`       | `RespondingToCq`       | `CqResponse` (grid)|
    /// | `Report`     | `SendingReport`        | `SignalReport`     |
    /// | `ReportAck`  | `SendingReport`        | `ReportAck`        |
    /// | `Rr73`       | `WaitingForConfirmation` | `FinalConfirmation` |
    /// | `SeventyThree` | `Completed`          | `SeventyThree` (+ QsoCompleted) |
    ///
    /// `our_snr_of_them` is our measurement of the caller's signal; it
    /// produces the report we send (rounded, clamped to −30..50, defaulting to
    /// −15 if absent). `their_report` is the report they sent us, if known —
    /// used to populate the `their_report` field of the `SendingReport` /
    /// `WaitingForConfirmation` state for `ReportAck`/`Rr73` opens.
    ///
    /// The `Grid` step is exactly equivalent to `respond_to_cq_manual`, so the
    /// DX-Hunter path (which still uses `StartQso` → `respond_to_cq_manual`) is
    /// unaffected.
    pub async fn respond_to_caller(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
    ) -> Result<QsoId, QsoManagerError> {
        use pancetta_core::ResponseStep;

        // Grid is exactly the historical manual-call behavior; route through
        // the existing path so there is a single source of truth for it.
        if step == ResponseStep::Grid {
            return self
                .respond_to_cq_with(target, frequency, dx_parity, CallInitiation::Manual)
                .await;
        }

        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }

        // Our report of their signal (the report WE send), same formula the
        // auto-sequencer uses in `MessageExchange::generate_response`, but
        // defaulting to -15 when we have no measurement.
        let our_report: SignalReport = our_snr_of_them
            .map(|s| (s.round() as i8).clamp(-30, 50))
            .unwrap_or(-15);
        // Their report of us (only meaningful for ReportAck/Rr73 opens).
        let their_report_val: SignalReport = their_report.unwrap_or(-15);

        // FIX 1: if we already have an ACTIVE manual QSO with this caller on
        // this band, CONTINUE it instead of superseding/duplicating. Mashing a
        // context reply on a station already in progress must keep ONE QSO per
        // (callsign, band).
        //   - If the requested step is AHEAD of the existing QSO's current
        //     ladder stage (e.g. the DX now sent RR73 → SeventyThree while we
        //     were in SendingReport), advance the EXISTING QSO to emit that
        //     step.
        //   - If it matches (or is behind) the current stage, just re-send the
        //     existing QSO's current outbound (idempotent keep-call).
        // Either way we return the existing id and never create a second QSO.
        if let Some(existing_id) = self.find_active_manual_qso_for(&target, frequency).await {
            let existing_rank = {
                let qsos = self.qsos.read().await;
                qsos.get(&existing_id)
                    .and_then(|p| Self::ladder_rank(&p.state))
            };
            let requested_rank = Self::step_ladder_rank(step);
            match (existing_rank, requested_rank) {
                (Some(cur), Some(req)) if req > cur => {
                    info!(
                        "Context reply to {} at step {:?} — advancing existing QSO {} \
                         (ahead of its current stage)",
                        target, step, existing_id
                    );
                    self.advance_existing_qso_to_step(
                        existing_id,
                        &target,
                        frequency,
                        step,
                        our_report,
                        their_report_val,
                    )
                    .await?;
                    return Ok(existing_id);
                }
                _ => {
                    info!(
                        "Context reply to {} at step {:?} — re-sending existing QSO {} \
                         current outbound (idempotent keep-call)",
                        target, step, existing_id
                    );
                    let _ = self.resend_last_tx(existing_id).await;
                    return Ok(existing_id);
                }
            }
        }

        // Manual: supersede any same-call QSO on this band, then build the new
        // one (no duplicate gate — the operator explicitly chose this caller).
        // With FIX 1 above this only fires when no ACTIVE QSO remains (e.g. a
        // lingering terminal record), so it should rarely trigger now.
        self.supersede_active_qsos_for(&target, frequency).await;

        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let tx_parity = dx_parity.map(|p| p.opposite());

        // Build (initial_state, first_message) for the chosen step.
        let (state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => unreachable!("Grid handled above"),
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target.clone()),
            frequency,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by: CallInitiation::Manual,
            // Replying to a station calling us → Caller role.
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
        };

        // If we are opening at the close (SeventyThree → Completed), stamp the
        // completion reports/end-time so the logged record is well-formed.
        let is_completed_open = matches!(state, QsoState::Completed { .. });
        if is_completed_open {
            metadata.reports = SignalReports {
                sent: Some(our_report),
                received: Some(their_report_val),
            };
            metadata.end_time = Some(now);
        }

        let raw_text = self.render_sent_text(&message);
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata: metadata.clone(),
        };

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target, qso_id).await;

        // See the matching comment in `respond_to_cq_with`: emit the initial
        // StateChanged BEFORE the first MessageToSend so the coordinator's
        // `active_tx_qsos` set has this qso_id inserted before the first
        // scheduled TransmitRequest reaches the Step 4b PTT gate. A
        // `Completed` open (SeventyThree) is not an active state, but emitting
        // the StateChanged is still correct/harmless — the gate's post-
        // completion grace window keeps the final-73 TransmitRequest live (and
        // the QsoCompleted event below drives the grace insert+removal anyway).
        self.emit_state_change(qso_id, QsoState::Idle, state.clone())
            .await;

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;

        info!(
            "Responding to caller {} on {:.1} Hz at step {:?}: {}",
            target, frequency, step, qso_id
        );

        // If we opened directly at the close, emit QsoCompleted so the logger
        // records the QSO. Mirror the completion metadata path in
        // `process_message_for_qso`, including the dial-frequency stamp so the
        // ADIF carries the real on-air RF frequency, not the bare audio offset.
        if is_completed_open {
            let dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            if dial > 0 {
                metadata.frequency += dial as f64;
            }
            self.emit_event(QsoEvent::QsoCompleted { qso_id, metadata })
                .await;
        }

        Ok(qso_id)
    }

    /// Process an incoming message
    pub async fn process_message(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
    ) -> Result<(), QsoManagerError> {
        let timestamp = Utc::now();

        // Find relevant QSO(s)
        let qso_ids = self.find_qsos_for_message(&message_type, frequency).await;

        for qso_id in qso_ids {
            let message = QsoMessage {
                timestamp,
                direction: MessageDirection::Received,
                message_type: message_type.clone(),
                raw_text: raw_text.clone(),
                signal_strength,
                frequency,
            };

            self.process_message_for_qso(qso_id, message).await?;
        }

        Ok(())
    }

    /// Get QSO status
    pub async fn get_qso(&self, qso_id: QsoId) -> Result<QsoProgress, QsoManagerError> {
        let qsos = self.qsos.read().await;
        qsos.get(&qso_id)
            .cloned()
            .ok_or(QsoManagerError::QsoNotFound { qso_id })
    }

    /// Get all active QSOs
    pub async fn get_active_qsos(&self) -> Vec<(QsoId, QsoProgress)> {
        let qsos = self.qsos.read().await;
        qsos.iter()
            .filter(|(_, progress)| progress.state.is_active())
            .map(|(id, progress)| (*id, progress.clone()))
            .collect()
    }

    /// Cancel a QSO
    pub async fn cancel_qso(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        if let Some(mut progress) = qsos.remove(&qso_id) {
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: QsoFailureReason::UserCancelled,
                failed_at: Utc::now(),
                last_state: Box::new(old_state.clone()),
            };

            self.emit_state_change(qso_id, old_state, progress.state.clone())
                .await;

            // Remove from callsign mapping
            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }

            info!("Cancelled QSO: {}", qso_id);
        }

        Ok(())
    }

    /// Emit a MessageToSend event for a QSO.
    ///
    /// Reads `tx_parity` from the QSO metadata so that every emission
    /// carries the value latched at QSO start, regardless of when this
    /// method is called.  Used by the auto_sequencer internally and
    /// exposed as `pub` so integration tests can drive additional
    /// MessageToSend events without going through the auto_sequencer.
    pub async fn send_message(&self, qso_id: QsoId, message: MessageType, frequency: f64) {
        let tx_parity = self
            .qsos
            .read()
            .await
            .get(&qso_id)
            .map(|p| p.metadata.tx_parity)
            .unwrap_or(None);
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;
    }

    /// Re-send the most recent outbound message for a QSO.
    ///
    /// Looks up the QSO, finds the most-recent `Sent` message in its message
    /// log, and re-emits it via the same `MessageToSend` path `send_message`
    /// uses (carrying the QSO's frequency and latched `tx_parity`). Returns
    /// `QsoNotFound` for an unknown id; returns `Ok(())` (a benign no-op) when
    /// the QSO has no prior outbound message to resend.
    pub async fn resend_last_tx(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let (message, frequency) = {
            let qsos = self.qsos.read().await;
            let progress = qsos
                .get(&qso_id)
                .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
            match progress
                .messages
                .iter()
                .rev()
                .find(|m| m.direction == MessageDirection::Sent)
            {
                Some(m) => (m.message_type.clone(), progress.metadata.frequency),
                None => {
                    info!("resend_last_tx: no prior Sent message for QSO {}", qso_id);
                    return Ok(());
                }
            }
        };
        self.send_message(qso_id, message, frequency).await;
        Ok(())
    }

    /// Get next contest serial number
    pub async fn get_next_serial(&self) -> SerialNumber {
        let mut next_serial = self.next_serial.write().await;
        let serial = *next_serial;
        *next_serial += 1;
        serial
    }

    // Internal helper methods

    /// Render a `MessageType` to the FT8 text we would transmit, so the
    /// recorded `Sent` `QsoMessage.raw_text` matches what goes on the air.
    /// Without this, engine-emitted Sent records carried an empty string and
    /// the TUI "TX:" line was blank (UX audit Batch 2). Falls back to an
    /// empty string on the (unexpected) render error rather than failing the
    /// QSO; the message still transmits via the separate encode path.
    fn render_sent_text(&self, message: &MessageType) -> String {
        crate::exchange::MessageExchange::new(self.config.our_callsign.clone())
            .generate_message(message)
            .unwrap_or_default()
    }

    async fn process_message_for_qso(
        &self,
        qso_id: QsoId,
        message: QsoMessage,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;

        let old_state = progress.state.clone();
        // Capture per-QSO routing data while we hold the write lock so the
        // reply emission below does not need to re-acquire it (which would
        // deadlock against this guard).
        let qso_frequency = progress.metadata.frequency;
        let qso_tx_parity = progress.metadata.tx_parity;
        let qso_initiated_by = progress.metadata.initiated_by;
        progress.messages.push(message.clone());

        // Compound-callsign equivalence (catalog C18 / peer D4): if this frame
        // came from our latched partner under a MORE-COMPLETE displayed call
        // (same station per `callsigns_match`, but a longer compound form — e.g.
        // we latched bare `G8BCG` and the DX now signs `EA8/G8BCG`), upgrade the
        // logged `their_callsign` to the fuller form. The compound carries DX /
        // portable info worth preserving in the ADIF. We only upgrade to a
        // STRICTLY LONGER matching form, so a later bare-call frame never
        // downgrades an already-latched compound, and a genuinely different
        // station (which would fail `callsigns_match`) never overwrites it.
        if let (Some(sender), Some(latched)) = (
            message.message_type.sender_callsign(),
            progress.metadata.their_callsign.as_deref(),
        ) {
            if crate::exchange::callsigns_match(sender, latched) && sender.len() > latched.len() {
                let upgraded = sender.to_string();
                info!(
                    target: "qso.compound",
                    from = %latched,
                    to = %upgraded,
                    "upgrading logged partner callsign to more-complete compound form (C18)"
                );
                progress.metadata.their_callsign = Some(upgraded);
            }
        }

        // Determine state transition based on current state and message.
        // `initiated_by` is threaded through so the manual-only state-regression
        // arms ("back up to where the DX thinks we are") never fire for
        // autonomous QSOs.
        let new_state = self
            .determine_state_transition(
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
            )
            .await?;

        // Auto-sequence the outbound reply for MANUAL-initiated QSOs only.
        // The reply is generated from the SAME (pre-transition state,
        // received message) pair that drove the transition, so the two never
        // disagree. Autonomous-initiated QSOs are deliberately left UNCHANGED
        // (no auto-reply) — that remains gated for Phase 5.
        //
        // `reply_to_emit` is captured here (under the lock) and emitted after
        // the write guard is released, since `emit_event` only needs the
        // broadcast channel, not the QSO map.
        // Detect a manual state regression: the DX sent an EARLIER-stage
        // message and `determine_state_transition` either backed us up the
        // ladder (rank decreased) or kept us in SendingReport on a repeated
        // report. Used to (a) count the re-send against the manual watchdog cap
        // and (b) gate the per-slot rearm so it does not double-send in the
        // same slot.
        let is_manual_regression = qso_initiated_by == CallInitiation::Manual
            && (Self::ladder_rank(&new_state) < Self::ladder_rank(&old_state)
                || (matches!(old_state, QsoState::SendingReport { .. })
                    && matches!(new_state, QsoState::SendingReport { .. })
                    && matches!(message.message_type, MessageType::SignalReport { .. })));

        let mut reply_to_emit: Option<MessageType> = None;
        if (new_state != old_state || is_manual_regression)
            && qso_initiated_by == CallInitiation::Manual
        {
            let exchange = crate::exchange::MessageExchange::new(self.config.our_callsign.clone());
            match exchange.generate_response(
                &old_state,
                &message.message_type,
                message.signal_strength,
            ) {
                Ok(Some(reply)) => reply_to_emit = Some(reply),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        qso_id = %qso_id,
                        "failed to generate auto-sequence reply: {}",
                        e
                    );
                }
            }
        }

        // UX audit Batch 2 #8: latch the DX's grid the moment it arrives in a
        // CqResponse (the opening "<us> <them> <grid>" / "CQ <them> <grid>"
        // exchange). The common close arm (SendingReport → Completed) hard-codes
        // grid_square: None, so without latching here the decoded grid never
        // reaches the logged ADIF GRIDSQUARE. We only overwrite when the
        // incoming grid is present so a later grid-less message can't clear it.
        if let MessageType::CqResponse {
            grid: Some(grid), ..
        } = &message.message_type
        {
            if !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        if new_state != old_state {
            // CQer flow: the QSO was created by start_cq with their_callsign
            // None (we didn't know who would answer). The moment a state
            // advance reveals the contra callsign (caller answered), latch it
            // so the logged ADIF/worked-station record carries the right call,
            // and register the callsign mapping for relevance/supersede.
            if progress.metadata.their_callsign.is_none() {
                if let Some(call) = new_state.their_callsign() {
                    progress.metadata.their_callsign = Some(call.to_string());
                }
            }

            // If transitioning to Completed, update metadata with signal reports and end time
            if let QsoState::Completed {
                their_report,
                our_report,
                grid_square,
                ..
            } = &new_state
            {
                progress.metadata.reports = SignalReports {
                    sent: Some(*our_report),
                    received: Some(*their_report),
                };
                progress.metadata.end_time = Some(Utc::now());
                // Prefer the grid carried in the Completed state (CQer path
                // threads it through WaitingForConfirmation); otherwise keep
                // whatever was latched from the opening CqResponse above.
                if let Some(grid) = grid_square {
                    progress.metadata.grids.theirs = Some(grid.clone());
                }
            }

            let completed_metadata = if matches!(&new_state, QsoState::Completed { .. }) {
                let mut m = progress.metadata.clone();
                // `m.frequency` is the audio offset within the slot. The logged
                // RF frequency is the rig dial plus that offset (WSJT-X logs the
                // actual on-air frequency, not the dial). Without this the ADIF
                // recorded BAND 0MHZ / FREQ ~0.001 from the bare offset.
                let dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                if dial > 0 {
                    m.frequency = dial as f64 + m.frequency;
                }
                Some(m)
            } else {
                None
            };

            // C3 fix (watchdog-vs-just-in-time-answer race): mark that this QSO
            // made a FORWARD state advance in the current watchdog cycle. The
            // manual keep-calling watchdog (`check_timeouts_at`) grants a
            // one-pass reprieve to any QSO that advanced since its previous
            // pass, so a just-in-time DX answer arriving in the very slot the
            // call cap trips is NOT thrown away as Failed{Timeout} in the same
            // tick it advanced. This is deliberately NOT a `call_count` reset —
            // the per-QSO cap still bounds total calls across the whole QSO
            // (C12: per-QSO, not per-step), and a QSO that advances once then
            // goes silent still retires at the cap on the NEXT pass (the flag
            // is cleared every watchdog pass). We set it only on a genuine
            // forward advance (not a manual regression — a DX repeating an
            // earlier message must keep counting against the cap so a stuck DX
            // cannot drive an unbounded ping-pong). Auto QSOs do not use the
            // manual watchdog and are unaffected.
            if qso_initiated_by == CallInitiation::Manual && !is_manual_regression {
                progress.metadata.progressed_this_cycle = true;
            }

            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: message.timestamp,
                reason: TransitionReason::MessageReceived(message.message_type.clone()),
            });

            self.emit_state_change(qso_id, old_state, new_state).await;

            // Emit QsoCompleted event so loggers can auto-log the QSO
            if let Some(metadata) = completed_metadata {
                self.emit_event(QsoEvent::QsoCompleted { qso_id, metadata })
                    .await;
            }
        }

        // Count a manual regression re-send against the keep-calling watchdog
        // so a DX that keeps repeating an earlier message cannot drive an
        // unbounded ping-pong. We bump `call_count` and stamp `last_call_at`
        // to `message.timestamp`; the latter also gates `rearm_manual_calls_at`
        // (which only re-emits when ≥1 slot has elapsed since `last_call_at`),
        // so the in-slot transition re-send and the per-slot rearm never both
        // fire in the same slot. `first_call_at` is left untouched — a
        // regression must not reset the watchdog clock.
        if is_manual_regression {
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(message.timestamp);
                // A regression is the opposite of forward progress (the DX
                // repeated an earlier message), so it cancels any pending C3
                // reprieve from an earlier advance — a stuck DX repeating must
                // still retire at the cap, never earn an extra watchdog pass.
                progress.metadata.progressed_this_cycle = false;
            }
        }

        self.emit_event(QsoEvent::MessageReceived { qso_id, message })
            .await;

        // Record the auto-sequenced reply as a Sent message (under the lock)
        // so it is available to `resend_last_tx` and the UI snapshot. Render
        // the FT8 text so the TUI "TX:" line shows what we sent (UX audit
        // Batch 2 — was String::new()).
        if let Some(reply) = reply_to_emit.as_ref() {
            let reply_text = self.render_sent_text(reply);
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.messages.push(QsoMessage {
                    timestamp: Utc::now(),
                    direction: MessageDirection::Sent,
                    message_type: reply.clone(),
                    raw_text: reply_text,
                    signal_strength: None,
                    frequency: qso_frequency,
                });
            }
        }

        // Release the QSO map write lock before emitting the reply so the
        // emission path holds no locks (and a future change to send_message-
        // style routing cannot deadlock).
        drop(qsos);

        // Emit the auto-sequenced reply for manual QSOs. We transmit on the
        // QSO's own frequency and reuse the tx_parity latched at QSO start,
        // exactly as the initial-call MessageToSend does.
        if let Some(reply) = reply_to_emit {
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message: reply,
                frequency: qso_frequency,
                tx_parity: qso_tx_parity,
            })
            .await;
        }

        Ok(())
    }

    /// Forward position of a state on the responder's FT8 QSO ladder:
    /// RespondingToCq → SendingReport → WaitingForConfirmation → Completed.
    /// Higher means later in the conversation. Used only to detect a manual
    /// state *regression* (a transition whose rank decreased). States off this
    /// ladder (CallingCq, Idle, Failed, Contest, …) return `None` so they never
    /// register as a regression.
    /// Does `call` refer to *our* station, allowing a compound form of our own
    /// callsign (e.g. we operate as `K5ARH/P`)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`] against `our_callsign`. Used in the
    /// `to == us` / `calling_station == us` halves of sender verification so a
    /// message directed at our compound call is not rejected as "not for us".
    fn is_us(&self, call: &str) -> bool {
        crate::exchange::callsigns_match(call, &self.config.our_callsign)
    }

    /// Is `from` the same station as our latched QSO partner `partner`, allowing
    /// a compound↔base change mid-QSO (catalog C18 / peer D4)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`]. Used in the `from == DX` half of
    /// sender verification so an established QSO does not stall when the DX's
    /// displayed call gains or loses a portable prefix/suffix between frames.
    /// Deliberately conservative: genuinely different calls (`K5ARH`/`K5ARG`)
    /// still mismatch — see `callsigns_match` docs.
    fn is_partner(from: &str, partner: &str) -> bool {
        crate::exchange::callsigns_match(from, partner)
    }

    fn ladder_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::RespondingToCq { .. } => Some(0),
            QsoState::SendingReport { .. } => Some(1),
            QsoState::WaitingForConfirmation { .. } => Some(2),
            QsoState::SendingConfirmation { .. } => Some(2),
            QsoState::Completed { .. } => Some(3),
            _ => None,
        }
    }

    async fn determine_state_transition(
        &self,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
    ) -> Result<QsoState, QsoManagerError> {
        match (current_state, message_type) {
            // CQ call received response (CQer flow). A station answered our CQ
            // with "<us> <them> <grid>". Verify the response is addressed to us
            // (calling_station == our callsign) before advancing — a spurious
            // CqResponse to another station must not hijack our CQ QSO. We latch
            // their grid here (UX audit Batch 2 #8) so the eventual ADIF carries
            // GRIDSQUARE; the relevance filter already directs only
            // addressed-to-us responses here, but we re-verify for defence.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    grid,
                },
            ) => {
                if !self.is_us(calling_station) {
                    warn!(
                        target: "qso.security",
                        got_to = %calling_station,
                        got_from = %responding_station,
                        "CqResponse not addressed to us ignored — no CQ advance"
                    );
                    return Ok(current_state.clone());
                }
                Ok(QsoState::WaitingForReport {
                    their_callsign: responding_station.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                    their_grid: grid.clone(),
                })
            }

            // A4 (CQer flow — caller skips the grid): a station answers our CQ
            // with a bare signal report ("<us> <them> -NN") instead of the
            // usual grid frame. On-air this means "I copied you, here's your
            // report" — the caller already has our copy. The protocol-correct
            // next move for us (the CQer) is to send THEM our report, exactly
            // as we would after a grid-bearing CqResponse — so we advance to
            // WaitingForReport (same rung as the CqResponse path) and the reply
            // emitter sends our SignalReport. Without this arm CallingCq had no
            // SignalReport transition and we kept re-CQing forever.
            //
            // Sender-verified like every other arm: the report must be TO us
            // (we don't yet know who will answer our CQ, so any from_station is
            // accepted as the contra). We latch their callsign (from_station)
            // so the QSO carries the right contra call; no grid is available.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        got_to = %to_station,
                        got_from = %from_station,
                        "SignalReport not addressed to us ignored — no CQ advance (A4)"
                    );
                    return Ok(current_state.clone());
                }
                Ok(QsoState::WaitingForReport {
                    their_callsign: from_station.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                    their_grid: None,
                })
            }

            // A5 (CQer flow — caller closes early): after we (the CQer) sent our
            // report (now WaitingForReport) the caller fires RR73 / a plain 73
            // instead of acking with their R-report. The caller is done — accept
            // the early close, complete, and log. This is the CQer-side mirror
            // of the FIX-2 early-close arm the Caller flow already has
            // (SendingReport → Completed on RR73/73). Without it a
            // WaitingForReport CQer ignored the close and the QSO never
            // completed.
            //
            // Sender-verified (from == caller && to == us). We never received a
            // numeric report-ack from them, so log with our computed report and
            // a defaulted their_report.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in WaitingForReport ignored (CQer, A5)"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                let duration = (Utc::now() - *started_at).num_seconds() as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // CQer flow: we sent our SignalReport (on the CallingCq→
            // WaitingForReport transition) and the caller rogered it with their
            // R-report (ReportAck). Advance to WaitingForConfirmation; the reply
            // emitter answers our FinalConfirmation (RR73). Carry the latched
            // grid into the confirmation state so it reaches Completed/ADIF.
            // Sender-verified (from == DX && to == us) like every other arm.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    their_grid,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in WaitingForReport ignored (CQer)"
                    );
                    return Ok(current_state.clone());
                }
                // The caller's R-report is their report OF US. Our report (of
                // them) was computed when we sent it; recover it from SNR or
                // fall back to the report they just acked.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: their_grid.clone(),
                    started_at: Utc::now(),
                })
            }

            // STUCK-AT-GRID FIX: we answered a CQer with our grid (now in
            // RespondingToCq) and the DX returns OUR call — either a bare
            // "<us> <DX>" or a "<us> <DX> <grid>" (a CqResponse directed at
            // us, carrying no report). On-air this means "I copied you, here
            // I am" — the DX heard our grid. The protocol-correct next move is
            // for us (the answering station) to send the DX a signal report,
            // advancing the contact. Without this arm we re-sent our grid every
            // slot until the manual watchdog timed out — the single
            // highest-frequency stall in the on-air log (N8ME, F5NNN, N9FME,
            // IQ0VT, KB5YNF, KA0NC, first-K9HJZ).
            //
            // A bare-call or grid answer to us parses as a CqResponse with
            // calling_station = us, responding_station = DX. Verify both
            // directions (from DX, to us) before advancing, exactly as on every
            // other arm. We carry no report from the DX yet (their_report:
            // None) and compute OUR report from the SNR.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if !Self::is_partner(responding_station, target_callsign)
                    || !self.is_us(calling_station)
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in RespondingToCq ignored — sender/target mismatch"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                info!(
                    target: "qso.advance",
                    their_callsign = %target_callsign,
                    "DX returned our call without a report — advancing grid -> signal report"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: None,
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // Response to CQ, waiting for report
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if !Self::is_partner(from_station, target_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                // Use received signal strength (SNR) as our report, default to received report
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // Received report acknowledgment
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck ignored"
                    );
                    return Ok(current_state.clone());
                }
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: Utc::now(),
                })
            }

            // FIX 2: the DX rogered our R-report directly with RR73 (or a
            // plain 73). Real FT8 is a 4-message QSO and RR73 is the close,
            // so we must complete (and the reply emitter answers our 73).
            // Without this arm the QSO stalled one message short — the DX's
            // RR73 was ignored and the contact was never logged. We accept
            // both FinalConfirmation (RR73/RRR-class close) and a bare
            // SeventyThree (73) here.
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in SendingReport ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (Utc::now() - *started_at).num_seconds() as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // Received final confirmation
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    grid_square,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious FinalConfirmation ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (Utc::now() - *started_at).num_seconds() as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
            }

            // === STATE REGRESSION (manual-initiated QSOs only) ===========
            // Operator principle: "if a DX station re-sends something EARLIER
            // in the conversation, they obviously didn't receive our response —
            // back ourselves up to where THEY think we are."
            //
            // These arms are gated on CallInitiation::Manual so autonomous
            // QSOs are unaffected. Sender verification (from == DX && to == us)
            // is preserved on every regression exactly as on forward arms.

            // REGRESSION 1: we sent RR73 (WaitingForConfirmation) but the DX is
            // still sending us their SignalReport — they never copied our R.
            // Back up two steps to SendingReport and re-send our R-report (the
            // reply emitter answers a ReportAck for this (state, msg) pair).
            // Latch the newest report value the DX sent.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) if initiated_by == CallInitiation::Manual => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Recompute our report from the freshest SNR (fall back to the
                // already-latched value), and latch the DX's newest report.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*our_report);
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → SendingReport \
                     (DX repeated their report; they never copied our R)"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // REGRESSION 2: we sent our R (SendingReport) and the DX re-sends
            // their SignalReport — they didn't copy our R. STAY in
            // SendingReport (do not advance); the per-slot rearm
            // (`rearm_manual_calls_at`, FIX 4) keeps re-sending our R-report.
            // We update the latched reports to the newest values the DX sent so
            // the eventual log carries the most recent exchange. Returning a
            // (possibly value-changed) SendingReport here drives a report
            // update without the reply emitter double-sending: exchange.rs has
            // no (SendingReport, SignalReport) response arm, so the in-slot
            // emit path is a no-op and the rearm owns the re-send.
            (
                QsoState::SendingReport {
                    their_callsign,
                    our_report,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) if initiated_by == CallInitiation::Manual => {
                if !Self::is_partner(from_station, their_callsign) || !self.is_us(to_station) {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in SendingReport ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*our_report);
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    // Preserve started_at so the manual watchdog keeps measuring
                    // from the original QSO start — a regression must not reset
                    // the keep-calling clock.
                    started_at: *started_at,
                })
            }

            // REGRESSION 3: we sent RR73 (WaitingForConfirmation) but the DX
            // re-sends their original grid/call (CqResponse) — they restarted
            // the whole exchange. Back up to RespondingToCq and re-send our
            // grid/call. Only observable when the repeated message parses as a
            // CqResponse directed appropriately for this QSO.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) if initiated_by == CallInitiation::Manual => {
                // A "DX K5ARH GRID" repeat parses with calling_station = us,
                // responding_station = DX. Verify both directions before
                // regressing so a spurious station cannot reset our QSO.
                if !Self::is_partner(responding_station, their_callsign)
                    || !self.is_us(calling_station)
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → RespondingToCq \
                     (DX restarted the exchange)"
                );
                Ok(QsoState::RespondingToCq {
                    target_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
            }

            // No state change
            _ => Ok(current_state.clone()),
        }
    }

    async fn find_qsos_for_message(
        &self,
        message_type: &MessageType,
        frequency: f64,
    ) -> Vec<QsoId> {
        let qsos = self.qsos.read().await;
        let mut matching_qsos = Vec::new();

        for (&qso_id, progress) in qsos.iter() {
            if !progress.state.is_active() {
                continue;
            }

            // Check if message is relevant to this QSO
            if self.is_message_relevant(&progress.state, message_type, frequency) {
                matching_qsos.push(qso_id);
            }
        }

        matching_qsos
    }

    fn is_message_relevant(
        &self,
        state: &QsoState,
        message_type: &MessageType,
        frequency: f64,
    ) -> bool {
        // Frequency tolerance tightened from 50 Hz → 15 Hz to reduce
        // cross-QSO message bleed-through in multi-QSO mode. FT8 frame-to-
        // frame drift is typically < 6 Hz on a stable transceiver, so 15 Hz
        // covers normal operation while shrinking the window an attacker
        // can exploit. (Security review 2026-04-29 C-1.)
        const FREQ_TOLERANCE_HZ: f64 = 15.0;
        // B15 fix: once a QSO is ESTABLISHED (we know the contra callsign and
        // are past CallingCq/Idle), allow a wider drift so an actively-
        // answering DX that has drifted beyond the tight window is NOT dropped.
        // The match arms below already require from == DX && to == us && the
        // state-appropriate message, which unambiguously identifies our partner
        // — at that point callsign+state continuity wins over the freq window
        // (catalog B15). We WIDEN the gate (to 100 Hz) rather than re-latch the
        // QSO's stored frequency here: `is_message_relevant` takes `&self` and
        // holds only a read lock, so it cannot mutate state; 100 Hz comfortably
        // covers realistic transceiver drift / micro-QSY within a contact while
        // still bounding how far a stray station can be from our partner's
        // latched offset. The tight 15 Hz gate is kept for INITIAL / ambiguous
        // matching (CallingCq, Idle, and any non-matching message) so two
        // different stations are never merged into one QSO.
        const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;

        let matched = match (state, message_type) {
            // We're calling CQ. The responder's callsign is whoever is in the
            // `responding_station` field; the message must be addressed to us.
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    calling_station, ..
                },
            ) => self.is_us(calling_station),

            // A4 (routing half): a caller answered our CQ with a bare signal
            // report (grid skipped) — "<us> <them> -NN". Route it to this
            // CallingCq QSO so the transition arm can step CQ → report. Only
            // addressed-to-us reports qualify (any from_station, since we don't
            // yet know who will answer).
            (QsoState::CallingCq { .. }, MessageType::SignalReport { to_station, .. }) => {
                self.is_us(to_station)
            }

            // CQer flow: we called CQ, the caller answered, and we sent our
            // report (now WaitingForReport). The caller's R-report (ReportAck)
            // is the next message — route it to this QSO so it can close.
            // Verify both directions: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // A5 (routing half): the caller closed early with RR73 / 73 from
            // WaitingForReport (before sending their R-report). Route the close
            // to this QSO so the transition arm can complete it. Both directions
            // verified: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // We responded to a CQ from `target_callsign` and are waiting for
            // their report. Verify both directions: from THEM, to US.
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, target_callsign) && self.is_us(to_station),

            // STUCK-AT-GRID FIX (routing half): the DX answered our grid by
            // returning our call (bare "<us> <DX>" or "<us> <DX> <grid>") — a
            // CqResponse directed at us. Route it to this QSO so the transition
            // arm can step grid -> report. Verify both directions: from THEM
            // (responding_station), to US (calling_station).
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                Self::is_partner(responding_station, target_callsign) && self.is_us(calling_station)
            }

            // We sent the report and are waiting for the report-ack. Same check.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // FIX 2: the DX may close directly from our R-report with RR73
            // (or a plain 73) instead of acking first — accept it here so it
            // routes to this QSO. Both directions verified.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // Awaiting RR73 — verify both directions. Accept a plain 73 too
            // (DX skipped RR73).
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            _ => {
                // Anything else: only relevant if addressed to us.
                message_type.is_addressed_to(&self.config.our_callsign)
            }
        };

        if !matched {
            return false;
        }

        // Apply the frequency gate AFTER the callsign/to/state match (B15). A
        // matched message from an ESTABLISHED QSO's partner is allowed the
        // wider drift bound; everything else uses the tight default. An
        // established QSO is one where we already know the contra callsign
        // (i.e. not CallingCq/Idle) — `their_callsign()` is Some.
        if let Some(qso_freq) = state.frequency() {
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            if (qso_freq - frequency).abs() > tolerance {
                return false;
            }
        }

        true
    }

    async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
    ) -> Result<bool, QsoManagerError> {
        if !self.config.duplicate_checking.enabled {
            return Ok(false);
        }

        // Check in-memory active/recent QSOs first (case-insensitive key,
        // Batch 2 #7, matching add/remove_callsign_mapping).
        let key = callsign.to_uppercase();
        let qsos_by_callsign = self.qsos_by_callsign.read().await;
        if let Some(qso_ids) = qsos_by_callsign.get(&key) {
            let qsos = self.qsos.read().await;
            let time_window =
                Duration::hours(self.config.duplicate_checking.time_window_hours as i64);
            let cutoff_time = Utc::now() - time_window;

            for &qso_id in qso_ids {
                if let Some(progress) = qsos.get(&qso_id) {
                    if progress.metadata.start_time > cutoff_time {
                        // Check frequency if required
                        if self.config.duplicate_checking.check_frequency
                            && (progress.metadata.frequency - frequency).abs() > 50.0
                        {
                            continue;
                        }

                        return Ok(true);
                    }
                }
            }
        }
        drop(qsos_by_callsign);

        // Also check the persistent database (catches duplicates after restart
        // or after cleanup_completed_qsos has removed them from memory)
        if let Some(ref db) = self.database {
            let now = Utc::now();
            match db
                .check_duplicate(
                    callsign,
                    frequency,
                    now,
                    self.config.duplicate_checking.time_window_hours,
                )
                .await
            {
                Ok(Some(_qso_id)) => {
                    debug!(
                        "Duplicate QSO for {} found in database (not in memory)",
                        callsign
                    );
                    return Ok(true);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        "Database duplicate check failed, relying on in-memory only: {}",
                        e
                    );
                }
            }
        }

        Ok(false)
    }

    async fn add_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // UX audit Batch 2 #7: key the callsign map case-insensitively
        // (uppercase). A case/format mismatch between a DX-Hunter call and a
        // Callers reply for the same station would otherwise defeat supersede
        // and re-spawn a duplicate QSO. Callsigns are conventionally uppercase;
        // normalising here (and at every lookup) makes supersede robust.
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        qsos_by_callsign
            .entry(key)
            .or_insert_with(Vec::new)
            .push(qso_id);
    }

    /// FIX 1: return the id of an ACTIVE (non-terminal), MANUAL-initiated QSO
    /// with `callsign` on the same band as `frequency`, if one exists. Used by
    /// the operator re-call paths so mashing Call/Space on a station already in
    /// progress CONTINUES the one QSO rather than superseding it / spawning a
    /// duplicate. Case-insensitive callsign match (matching the callsign-map
    /// keying); "same band" derived via [`crate::utils::frequency_to_band`]
    /// exactly like [`Self::supersede_active_qsos_for`]. When several match
    /// (shouldn't happen post-FIX-1, but be robust), the most-recently-started
    /// one wins.
    async fn find_active_manual_qso_for(&self, callsign: &str, frequency: f64) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| {
                    let matches = p.state.is_active()
                        && p.metadata.initiated_by == CallInitiation::Manual
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band;
                    matches.then_some((id, p.metadata.start_time))
                })
            })
            .max_by_key(|(_, started)| *started)
            .map(|(id, _)| id)
    }

    /// FIX 1: forward position of a [`ResponseStep`] on the responder's FT8 QSO
    /// ladder, aligned with [`Self::ladder_rank`] so the two are directly
    /// comparable. `Grid` → 0 (RespondingToCq), `Report`/`ReportAck` → 1
    /// (SendingReport), `Rr73` → 2 (WaitingForConfirmation), `SeventyThree` → 3
    /// (Completed). Used to decide whether a context reply is AHEAD of an
    /// existing QSO's current stage (→ advance) or at/behind it (→ re-send).
    fn step_ladder_rank(step: pancetta_core::ResponseStep) -> Option<u8> {
        use pancetta_core::ResponseStep;
        Some(match step {
            ResponseStep::Grid => 0,
            ResponseStep::Report | ResponseStep::ReportAck => 1,
            ResponseStep::Rr73 => 2,
            ResponseStep::SeventyThree => 3,
        })
    }

    /// FIX 1: advance an EXISTING manual QSO to the state/outbound implied by
    /// `step`, instead of creating a new QSO. Mirrors the (state, message)
    /// mapping in [`Self::respond_to_caller`] but mutates the existing QSO in
    /// place: sets its state, records the outbound as a `Sent` message, emits
    /// the `MessageToSend`, and — when advancing to `SeventyThree` — stamps the
    /// completion metadata and emits `QsoCompleted` so the contact is logged.
    /// The QSO's latched `tx_parity` and `initiated_by` are preserved (we reuse
    /// what was latched at QSO start). Used when an operator context-replies at
    /// a step ahead of where the QSO currently is.
    async fn advance_existing_qso_to_step(
        &self,
        qso_id: QsoId,
        target: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
        our_report: SignalReport,
        their_report_val: SignalReport,
    ) -> Result<(), QsoManagerError> {
        use pancetta_core::ResponseStep;
        let now = Utc::now();

        let (new_state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => {
                // Grid never ranks ahead of an active QSO, so the caller never
                // routes here; re-send current as a safe fallback.
                return self.resend_last_tx(qso_id).await;
            }
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let is_completed = matches!(new_state, QsoState::Completed { .. });
        let raw_text = self.render_sent_text(&message);

        // Mutate the existing QSO under the write lock, capturing what we need
        // for the emits after the lock is released.
        let emit = {
            let mut qsos = self.qsos.write().await;
            let Some(progress) = qsos.get_mut(&qso_id) else {
                return Err(QsoManagerError::QsoNotFound { qso_id });
            };
            let old_state = progress.state.clone();
            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: now,
                reason: TransitionReason::UserAction,
            });
            progress.messages.push(QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            });
            let tx_parity = progress.metadata.tx_parity;

            // On completion, stamp reports/end-time and prepare the completed
            // metadata (with the real RF frequency = dial + offset) to log.
            let completed_metadata = if is_completed {
                progress.metadata.reports = SignalReports {
                    sent: Some(our_report),
                    received: Some(their_report_val),
                };
                progress.metadata.end_time = Some(now);
                let mut m = progress.metadata.clone();
                let dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                if dial > 0 {
                    m.frequency += dial as f64;
                }
                Some(m)
            } else {
                None
            };
            (old_state, tx_parity, completed_metadata)
        };
        let (old_state, tx_parity, completed_metadata) = emit;

        self.emit_state_change(qso_id, old_state, new_state).await;
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;
        if let Some(metadata) = completed_metadata {
            self.emit_event(QsoEvent::QsoCompleted { qso_id, metadata })
                .await;
        }
        Ok(())
    }

    /// FIX 3: retire every active (non-terminal) QSO with `callsign` on the
    /// same band as `frequency`, marking each `Failed{Superseded}` and
    /// clearing its callsign mapping. Emits a `StateChanged` per superseded
    /// QSO (terminal Failed → AP/snapshot clears in the coordinator). Called
    /// just before a new manual QSO is created so only the most-recent one
    /// remains active.
    ///
    /// "Same band" is derived from the QSO frequency via
    /// [`crate::utils::frequency_to_band`]. Within a single operating session
    /// every active QSO shares the RF band, so in practice this collapses to
    /// "same callsign"; deriving the band keeps the rule correct should
    /// per-QSO RF frequencies ever be threaded through.
    async fn supersede_active_qsos_for(&self, callsign: &str, frequency: f64) {
        let new_band = crate::utils::frequency_to_band(frequency);
        // Look up case-insensitively (Batch 2 #7) so a case/format mismatch
        // can't leak a duplicate active QSO past supersede.
        let key = callsign.to_uppercase();

        // Collect the QSO IDs to supersede under the read lock, then mutate.
        let to_supersede: Vec<QsoId> = {
            let qsos = self.qsos.read().await;
            let ids = match self.qsos_by_callsign.read().await.get(&key) {
                Some(ids) => ids.clone(),
                None => Vec::new(),
            };
            ids.into_iter()
                .filter(|id| {
                    qsos.get(id).is_some_and(|p| {
                        p.state.is_active()
                            && crate::utils::frequency_to_band(p.metadata.frequency) == new_band
                    })
                })
                .collect()
        };

        for qso_id in to_supersede {
            let old_state = {
                let mut qsos = self.qsos.write().await;
                match qsos.get_mut(&qso_id) {
                    Some(progress) => {
                        let old_state = progress.state.clone();
                        progress.state = QsoState::Failed {
                            reason: QsoFailureReason::Superseded,
                            failed_at: Utc::now(),
                            last_state: Box::new(old_state.clone()),
                        };
                        Some(old_state)
                    }
                    None => None,
                }
            };
            if let Some(old_state) = old_state {
                let new_state = self.qsos.read().await.get(&qso_id).map(|p| p.state.clone());
                if let Some(new_state) = new_state {
                    self.emit_state_change(qso_id, old_state, new_state).await;
                }
                self.remove_callsign_mapping(callsign, qso_id).await;
                info!(
                    "Superseded older active QSO {} with {} on band {} (re-call)",
                    qso_id, callsign, new_band
                );
            }
        }
    }

    async fn remove_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // Match the uppercase keying of `add_callsign_mapping` (Batch 2 #7).
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        if let Some(qso_ids) = qsos_by_callsign.get_mut(&key) {
            qso_ids.retain(|&id| id != qso_id);
            if qso_ids.is_empty() {
                qsos_by_callsign.remove(&key);
            }
        }
    }

    async fn emit_event(&self, event: QsoEvent) {
        if let Err(e) = self.event_sender.send(event) {
            warn!("Failed to emit QSO event: {}", e);
        }
    }

    async fn emit_state_change(&self, qso_id: QsoId, old_state: QsoState, new_state: QsoState) {
        self.emit_event(QsoEvent::StateChanged {
            qso_id,
            old_state,
            new_state,
            timestamp: Utc::now(),
        })
        .await;
    }

    async fn cleanup_loop(&self) {
        loop {
            // Check if we should continue
            {
                let interval_guard = self.cleanup_interval.read().await;
                if interval_guard.is_none() {
                    break;
                }
            }

            // Wait for next tick
            {
                let mut interval_guard = self.cleanup_interval.write().await;
                if let Some(ref mut interval_timer) = *interval_guard {
                    interval_timer.tick().await;
                } else {
                    break;
                }
            }

            // Perform cleanup
            self.cleanup_completed_qsos().await;
        }
    }

    async fn timeout_check_loop(&self) {
        let mut interval_timer = interval(TokioDuration::from_secs(5)); // Check every 5 seconds

        loop {
            interval_timer.tick().await;
            // Re-arm manual keep-calling BEFORE the watchdog so a re-call
            // that pushes the count to the cap is still counted, then the
            // watchdog can retire it on the same or next tick.
            self.rearm_manual_calls().await;
            self.check_timeouts().await;
        }
    }

    /// Re-arm manual keep-calling at the current time. See
    /// [`Self::rearm_manual_calls_at`].
    async fn rearm_manual_calls(&self) {
        self.rearm_manual_calls_at(Utc::now()).await;
    }

    /// For every manual-initiated QSO still in `RespondingToCq` (waiting
    /// for the DX to come back), re-emit the call (a `CqResponse`
    /// `MessageToSend`) at most once per FT8 slot so the operator keeps
    /// calling the DX every slot until they answer or the manual watchdog
    /// fires. The TX scheduler downstream resolves slot parity from the
    /// `tx_parity` latched on the QSO, so re-emitting more often than a
    /// slot is harmless, but we gate to ~one per slot to avoid flooding
    /// the bus.
    ///
    /// Re-arming increments `call_count` and updates `last_call_at`; the
    /// watchdog ([`Self::check_timeouts_at`]) reads `call_count` and
    /// `first_call_at` to decide when to stop.
    pub async fn rearm_manual_calls_at(&self, now: DateTime<Utc>) {
        // One FT8 slot is 15s; re-arm only when at least a slot has
        // elapsed since the last call to keep ~one call per slot.
        const SLOT_SECONDS: i64 = 15;

        // Each entry carries the exact MessageType to re-emit so a
        // RespondingToCq QSO re-sends the call (CqResponse) while a
        // SendingReport QSO re-sends our R-report (ReportAck) — FIX 4.
        let mut to_recall: Vec<(
            QsoId,
            MessageType,
            f64,
            Option<pancetta_core::slot::SlotParity>,
        )> = Vec::new();

        {
            let mut qsos = self.qsos.write().await;
            for (&qso_id, progress) in qsos.iter_mut() {
                if progress.metadata.initiated_by != CallInitiation::Manual {
                    continue;
                }
                let message = match &progress.state {
                    // Manual CQ (operator `c`): keep calling CQ every slot
                    // until a station answers (→ WaitingForReport, handled by
                    // the normal sequence) or the watchdog retires us.
                    QsoState::CallingCq { .. } => MessageType::Cq {
                        callsign: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    QsoState::RespondingToCq {
                        target_callsign, ..
                    } => MessageType::CqResponse {
                        calling_station: target_callsign.clone(),
                        responding_station: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // FIX 4: we sent R and the DX re-sent their report (they
                    // did not copy our R) — re-send our R-report each slot,
                    // under the SAME watchdog, until the DX advances (RR73)
                    // or the watchdog retires us. Without this we went silent
                    // and stalled. Reconstruct the R-report from the report
                    // latched in the state.
                    QsoState::SendingReport {
                        their_callsign,
                        our_report,
                        ..
                    } => MessageType::ReportAck {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // Any later state: the normal sequence drives the rest.
                    _ => continue,
                };

                // Stop re-arming once the watchdog bound is reached; the
                // watchdog itself will retire the QSO on its own pass.
                let max_calls = self.config.timeouts.manual_call_max_calls;
                if progress.metadata.call_count >= max_calls {
                    continue;
                }

                let elapsed_since_last = progress
                    .metadata
                    .last_call_at
                    .map(|t| (now - t).num_seconds())
                    .unwrap_or(i64::MAX);
                if elapsed_since_last < SLOT_SECONDS {
                    continue;
                }

                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);

                // Record the re-emitted call as a Sent message so the TUI's
                // last-TX line and activity counter advance during keep-calling
                // (UX audit Batch 2 — the panel previously froze because rearm
                // appended nothing, making keep-calling look like a hang). The
                // raw_text is the rendered FT8 text we put on the air.
                let raw_text = self.render_sent_text(&message);
                progress.messages.push(QsoMessage {
                    timestamp: now,
                    direction: MessageDirection::Sent,
                    message_type: message.clone(),
                    raw_text,
                    signal_strength: None,
                    frequency: progress.metadata.frequency,
                });

                to_recall.push((
                    qso_id,
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                ));
            }
        }

        for (qso_id, message, frequency, tx_parity) in to_recall {
            debug!(
                "Manual keep-calling: re-emitting {:?} on {:.1} Hz (qso={})",
                message, frequency, qso_id
            );
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
            })
            .await;
        }
    }

    async fn cleanup_completed_qsos(&self) {
        let mut qsos = self.qsos.write().await;
        let cutoff_time = Utc::now() - Duration::hours(1); // Keep completed QSOs for 1 hour

        let to_remove: Vec<QsoId> = qsos
            .iter()
            .filter(|(_, progress)| match &progress.state {
                QsoState::Completed { completed_at, .. } => *completed_at < cutoff_time,
                QsoState::Failed { failed_at, .. } => *failed_at < cutoff_time,
                _ => false,
            })
            .map(|(&qso_id, _)| qso_id)
            .collect();

        for qso_id in to_remove {
            if let Some(progress) = qsos.remove(&qso_id) {
                if let Some(callsign) = &progress.metadata.their_callsign {
                    drop(qsos); // Release lock before acquiring another
                    self.remove_callsign_mapping(callsign, qso_id).await;
                    qsos = self.qsos.write().await; // Re-acquire lock
                }
                debug!("Cleaned up QSO: {}", qso_id);
            }
        }
    }

    async fn check_timeouts(&self) {
        self.check_timeouts_at(Utc::now()).await;
    }

    /// Watchdog pass at an explicit time (for testability).
    ///
    /// In addition to the standard per-state timeouts, this enforces the
    /// **manual keep-calling watchdog**: a manual-initiated QSO that is
    /// still in `RespondingToCq` is retired (→ `Failed`/idle, callsign
    /// mapping cleared) once it has either transmitted
    /// `manual_call_max_calls` calls OR `manual_call_watchdog_minutes`
    /// have elapsed since the first call — whichever comes first.
    pub async fn check_timeouts_at(&self, now: DateTime<Utc>) {
        let mut qsos = self.qsos.write().await;
        let mut timeouts = Vec::new();

        for (&qso_id, progress) in qsos.iter_mut() {
            // Manual keep-calling watchdog. Covers CallingCq (operator `c`:
            // re-calling CQ until someone answers), RespondingToCq
            // (re-calling the DX) and SendingReport (FIX 4: re-sending our
            // R-report when the DX repeats their report). In all phases the
            // operator is actively keep-calling, and `call_count` /
            // `first_call_at` span the whole QSO, so the 10-calls / 5-min
            // bound applies to the QSO as a whole. Once the DX advances past
            // these states (a caller answers / ReportAck / RR73 received),
            // the normal state timeouts take over.
            if progress.metadata.initiated_by == CallInitiation::Manual
                && matches!(
                    progress.state,
                    QsoState::CallingCq { .. }
                        | QsoState::RespondingToCq { .. }
                        | QsoState::SendingReport { .. }
                )
            {
                let max_calls = self.config.timeouts.manual_call_max_calls;
                let watchdog =
                    Duration::minutes(self.config.timeouts.manual_call_watchdog_minutes as i64);
                let elapsed = progress
                    .metadata
                    .first_call_at
                    .map(|t| now - t)
                    .unwrap_or_else(Duration::zero);

                // C3 race guard: if this QSO made a forward state advance in the
                // current watchdog cycle (the DX just answered), grant a
                // one-pass reprieve — do NOT retire it in the same tick it
                // advanced. The flag is consumed (cleared) here, so a QSO that
                // advanced once then goes silent is retired on the NEXT pass
                // once it re-hits the cap (per-QSO bound preserved; see C12).
                let progressed = progress.metadata.progressed_this_cycle;
                progress.metadata.progressed_this_cycle = false;

                if !progressed && (progress.metadata.call_count >= max_calls || elapsed >= watchdog)
                {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
                // Manual calls do not use the (much shorter) per-state
                // timeout while keep-calling; the watchdog above governs.
                continue;
            }

            if let Some(duration) = progress.state.state_duration(now) {
                let timeout_seconds = match &progress.state {
                    QsoState::CallingCq { .. } => self.config.timeouts.cq_timeout,
                    QsoState::WaitingForReport { .. } => self.config.timeouts.report_timeout,
                    QsoState::WaitingForConfirmation { .. } => {
                        self.config.timeouts.confirmation_timeout
                    }
                    _ => continue,
                };

                if duration.num_seconds() as u64 > timeout_seconds {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
            }
        }

        for (qso_id, reason) in timeouts {
            if let Some(mut progress) = qsos.remove(&qso_id) {
                let old_state = progress.state.clone();
                progress.state = QsoState::Failed {
                    reason,
                    failed_at: now,
                    last_state: Box::new(old_state.clone()),
                };

                drop(qsos); // Release lock before emitting events
                self.emit_state_change(qso_id, old_state, progress.state.clone())
                    .await;

                if let Some(callsign) = &progress.metadata.their_callsign {
                    self.remove_callsign_mapping(callsign, qso_id).await;
                }

                warn!("QSO timeout: {}", qso_id);
                qsos = self.qsos.write().await; // Re-acquire lock
            }
        }
    }
}

impl Clone for QsoManager {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            qsos: Arc::clone(&self.qsos),
            qsos_by_callsign: Arc::clone(&self.qsos_by_callsign),
            event_sender: self.event_sender.clone(),
            next_serial: Arc::clone(&self.next_serial),
            cleanup_interval: Arc::clone(&self.cleanup_interval),
            database: self.database.clone(),
            dial_frequency_hz: Arc::clone(&self.dial_frequency_hz),
        }
    }
}

// Default implementations removed - using the ones at lines 191-226

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test;

    fn test_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_start_cq() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager.start_cq(14074000.0, None).await.unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::CallingCq { .. }));
        assert_eq!(progress.metadata.frequency, 14074000.0);
    }

    #[tokio::test]
    async fn test_respond_to_cq() {
        let manager = QsoManager::new(test_config());
        let qso_id = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
        assert_eq!(progress.metadata.their_callsign, Some("K1DEF".to_string()));
    }

    // --- Manual-vs-auto calling semantics (operator policy) --------------

    /// An auto response to a callsign we already have an active QSO with is
    /// rejected by the self-duplicate gate (unchanged behavior).
    #[tokio::test]
    async fn auto_recall_to_same_dx_is_rejected_as_duplicate() {
        let manager = QsoManager::new(test_config());
        let _first = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let second = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(
            matches!(second, Err(QsoManagerError::DuplicateQso { .. })),
            "auto re-call should be a duplicate, got {:?}",
            second
        );
    }

    /// A MANUAL call bypasses the self-duplicate gate even when an active
    /// QSO with that callsign already exists.
    #[tokio::test]
    async fn manual_call_bypasses_duplicate_gate() {
        let manager = QsoManager::new(test_config());
        let _first = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let manual = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(
            manual.is_ok(),
            "manual call must not be blocked by duplicate gate, got {:?}",
            manual
        );
        let progress = manager.get_qso(manual.unwrap()).await.unwrap();
        assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
        assert_eq!(progress.metadata.call_count, 1);
    }

    /// Two consecutive manual calls to the same DX are both allowed (the
    /// operator hit the duplicate-QSO bug doing exactly this).
    #[tokio::test]
    async fn manual_recall_to_same_dx_is_allowed() {
        let manager = QsoManager::new(test_config());
        let a = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        let b = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await;
        assert!(a.is_ok() && b.is_ok(), "both manual calls allowed");
    }

    /// FIX 1: re-calling a station with an ACTIVE manual QSO CONTINUES that QSO
    /// — it returns the SAME qso_id, does NOT create a second QSO, and does NOT
    /// supersede (the old QSO is NOT marked Failed{Superseded}). This is the
    /// core of the "mashing Space spawns duplicate/superseding QSOs" fix.
    #[tokio::test]
    async fn manual_recall_of_active_qso_continues_same_qso() {
        let manager = QsoManager::new(test_config());
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let second = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "re-call of an active QSO must continue it (same id), not spawn a new one"
        );

        // Exactly one active QSO for this callsign — the original.
        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            active_for_dx,
            vec![first],
            "exactly one active QSO (the original) should remain"
        );

        // It is NOT superseded — still in its original RespondingToCq state.
        let progress = manager.get_qso(first).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "continued QSO must keep its state, got {:?}",
            progress.state
        );

        // The callsign mapping holds only the one QSO.
        let mapping = manager.qsos_by_callsign.read().await;
        assert_eq!(
            mapping.get("K1DEF").map(|v| v.as_slice()),
            Some([first].as_slice()),
            "mapping must point only to the single continued QSO"
        );
    }

    /// FIX 1 / FIX 3 boundary: a re-call AFTER the prior QSO already went
    /// terminal still works — it creates a FRESH QSO (and supersedes any
    /// lingering terminal record). This is the genuine "work them again" case.
    #[tokio::test]
    async fn manual_recall_after_terminal_creates_fresh_qso() {
        let manager = QsoManager::new(test_config());
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        // Drive the first QSO terminal (operator cancel).
        manager.cancel_qso(first).await.unwrap();

        let second = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_ne!(
            first, second,
            "re-call after the prior QSO went terminal must create a fresh QSO"
        );

        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(active_for_dx, vec![second], "only the fresh QSO is active");
    }

    /// FIX 1: a context-Space reply at a step AHEAD of the existing active
    /// QSO's stage ADVANCES the SAME QSO (no new QSO, no supersede). We open a
    /// manual QSO (RespondingToCq → step rank 0), then context-reply at Rr73
    /// (rank 2): the existing QSO must advance to WaitingForConfirmation and
    /// keep its id.
    #[tokio::test]
    async fn context_reply_ahead_advances_existing_qso() {
        use pancetta_core::ResponseStep;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();

        // DX is now ahead (they sent us an R-report); operator context-replies
        // at Rr73. Must advance THIS QSO, not create a new one.
        let advanced = manager
            .respond_to_caller(
                "K1DEF".to_string(),
                freq,
                None,
                ResponseStep::Rr73,
                Some(-10.0),
                Some(-12),
            )
            .await
            .unwrap();
        assert_eq!(
            first, advanced,
            "ahead context reply must continue same QSO"
        );

        let progress = manager.get_qso(first).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "QSO should have advanced to WaitingForConfirmation, got {:?}",
            progress.state
        );

        // Exactly one active QSO for this callsign.
        let active = manager.get_active_qsos().await;
        let n = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .count();
        assert_eq!(n, 1, "exactly one active QSO after the advance");
    }

    /// The manual watchdog retires a RespondingToCq QSO once the call count
    /// reaches `manual_call_max_calls`.
    #[tokio::test]
    async fn manual_watchdog_fires_on_max_calls() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 3;
        config.timeouts.manual_call_watchdog_minutes = 60; // not the binding bound here
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Simulate keep-calling: re-arm enough times (one slot apart) to
        // hit the cap. call_count starts at 1; re-arm to 3.
        let mut t = Utc::now();
        for _ in 0..5 {
            t += Duration::seconds(15);
            manager.rearm_manual_calls_at(t).await;
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            progress.metadata.call_count >= 3,
            "expected call_count to reach cap, got {}",
            progress.metadata.call_count
        );

        // Watchdog must now retire it.
        manager.check_timeouts_at(t).await;
        let after = manager.get_qso(qso_id).await;
        assert!(
            matches!(after, Err(QsoManagerError::QsoNotFound { .. })),
            "watchdog should have removed the QSO, got {:?}",
            after.map(|p| p.state)
        );
    }

    /// The manual watchdog retires a RespondingToCq QSO once the elapsed
    /// time exceeds `manual_call_watchdog_minutes`, even below the call cap.
    #[tokio::test]
    async fn manual_watchdog_fires_on_elapsed_time() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000; // not binding
        config.timeouts.manual_call_watchdog_minutes = 5;
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Just under 5 minutes: still alive.
        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
        manager
            .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
            .await;
        assert!(manager.get_qso(qso_id).await.is_ok());

        // Past 5 minutes: retired.
        manager
            .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));
    }

    /// rearm_manual_calls re-emits a CqResponse MessageToSend and increments
    /// the call count — but only once per slot, and not for auto QSOs.
    #[tokio::test]
    async fn rearm_emits_call_once_per_slot_for_manual_only() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let manual_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let _auto_id = manager
            .respond_to_cq("K9ZZ".to_string(), 14076000.0, None)
            .await
            .unwrap();

        // Drain the two initial MessageToSend events from the responses.
        let mut initial = 0;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, QsoEvent::MessageToSend { .. }) {
                initial += 1;
            }
        }
        assert_eq!(initial, 2, "two initial calls (one manual, one auto)");

        // Re-arm too soon (same instant as start): no new call.
        let start = manager
            .get_qso(manual_id)
            .await
            .unwrap()
            .metadata
            .start_time;
        manager.rearm_manual_calls_at(start).await;
        let mut too_soon = 0;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, QsoEvent::MessageToSend { .. }) {
                too_soon += 1;
            }
        }
        assert_eq!(too_soon, 0, "re-arm within a slot must not re-call");

        // Re-arm a slot later: exactly one new call, for the manual QSO.
        manager
            .rearm_manual_calls_at(start + Duration::seconds(15))
            .await;
        let mut recalls = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { qso_id, .. } = ev {
                recalls.push(qso_id);
            }
        }
        assert_eq!(recalls.len(), 1, "exactly one re-call");
        assert_eq!(recalls[0], manual_id, "only the manual QSO is re-called");
        assert_eq!(
            manager
                .get_qso(manual_id)
                .await
                .unwrap()
                .metadata
                .call_count,
            2
        );
    }

    /// resend_last_tx on a QSO with a prior Sent message re-emits a
    /// MessageToSend carrying that message.
    #[tokio::test]
    async fn resend_last_tx_reemits_last_sent() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();

        // Drain the initial call event.
        while events.try_recv().is_ok() {}

        manager.resend_last_tx(qso_id).await.unwrap();

        // Expect exactly one MessageToSend, re-emitting the initial CqResponse.
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend {
                qso_id, message, ..
            } = ev
            {
                resends.push((qso_id, message));
            }
        }
        assert_eq!(resends.len(), 1, "exactly one resend event");
        assert_eq!(resends[0].0, qso_id);
        assert!(matches!(resends[0].1, MessageType::CqResponse { .. }));
    }

    /// resend_last_tx on an unknown QSO id returns QsoNotFound.
    #[tokio::test]
    async fn resend_last_tx_unknown_id_not_found() {
        let manager = QsoManager::new(test_config());
        let bogus = QsoId::new_v4();
        let err = manager.resend_last_tx(bogus).await.unwrap_err();
        assert!(matches!(err, QsoManagerError::QsoNotFound { .. }));
    }

    /// FIX 4: a manual QSO in SendingReport (we sent R, DX has not advanced)
    /// re-emits our R-report (ReportAck) each slot when re-armed.
    #[tokio::test]
    async fn rearm_resends_r_report_in_sending_report() {
        let manager = QsoManager::new(test_config());
        let mut events = manager.subscribe();

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        // Advance to SendingReport: DX sends us a report; we send R-report.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -7,
                },
                "W1ABC K1DEF -07".to_string(),
                14074000.0,
                Some(-12.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        // Drain the initial call + the auto-sequenced R-report.
        while events.try_recv().is_ok() {}

        // A slot later, with the DX still not advancing, re-arm re-sends our
        // R-report (ReportAck), not a fresh call.
        let last = progress.metadata.last_call_at.unwrap();
        manager
            .rearm_manual_calls_at(last + Duration::seconds(15))
            .await;
        let mut resends = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let QsoEvent::MessageToSend { message, .. } = ev {
                resends.push(message);
            }
        }
        assert_eq!(resends.len(), 1, "exactly one re-send, got {:?}", resends);
        match &resends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, "K1DEF");
                assert_eq!(from_station, "W1ABC");
                assert_eq!(*report, -12, "re-sends our latched R-report");
            }
            other => panic!("expected ReportAck re-send, got {:?}", other),
        }
    }

    /// FIX 4: the watchdog still retires a SendingReport manual QSO that
    /// never advances — re-sending our R-report cannot loop forever.
    #[tokio::test]
    async fn watchdog_retires_stalled_sending_report() {
        let mut config = test_config();
        config.timeouts.manual_call_max_calls = 1000; // not the binding bound
        config.timeouts.manual_call_watchdog_minutes = 5;
        let manager = QsoManager::new(config);

        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -7,
                },
                "W1ABC K1DEF -07".to_string(),
                14074000.0,
                Some(-12.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));

        let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
        // Just under the watchdog: still alive.
        manager
            .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
            .await;
        assert!(manager.get_qso(qso_id).await.is_ok());
        // Past the watchdog: retired.
        manager
            .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
            .await;
        assert!(matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ));
    }

    // --- Batch 2 #6: CQer (we-CQed) completion path -----------------------

    /// A full we-CQed exchange must advance all the way to Completed and emit
    /// QsoCompleted (it previously stalled in WaitingForReport — no arm out).
    /// We drive it via `process_message`, feeding the caller's messages, and
    /// assert the QSO completes and logs the caller's grid (Batch 2 #8).
    #[tokio::test]
    async fn cqer_full_sequence_completes_and_logs_grid() {
        // our_callsign = W1ABC (from test_config).
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq(freq, None).await.unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::CallingCq { .. }
        ));
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.role,
            QsoRole::Cqer
        );

        // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::WaitingForReport { .. }),
            "CallingCq + CqResponse → WaitingForReport, got {:?}",
            p.state
        );

        // Caller rogers our report with their R-report: "W1ABC K1DEF R-12".
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::WaitingForConfirmation { .. }),
            "WaitingForReport + ReportAck → WaitingForConfirmation, got {:?}",
            p.state
        );

        // Caller closes with 73: "W1ABC K1DEF 73" → Completed.
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "WaitingForConfirmation + 73 → Completed, got {:?}",
            p.state
        );
        // Batch 2 #8: the caller's grid latched from the opening CqResponse is
        // carried into the logged metadata.
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
        assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
        assert!(p.metadata.end_time.is_some());
    }

    /// The manual CQer also EMITS the right reply at each step (the auto-reply
    /// path is Manual-gated). Drive a manual CQ QSO and verify the reply
    /// sequence SignalReport → FinalConfirmation reaches the event bus.
    #[tokio::test]
    async fn manual_cqer_emits_report_then_rr73() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        // Build a manual CallingCq QSO directly (start_cq is Auto-only; the
        // operator-CQ-as-QSO wiring is a separate deferred item — see report).
        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let progress = QsoProgress {
            state: QsoState::CallingCq {
                frequency: freq,
                started_at: now,
                call_count: 1,
            },
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id,
                our_callsign: "W1ABC".to_string(),
                their_callsign: None,
                frequency: freq,
                mode: "FT8".to_string(),
                start_time: now,
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
                contest_info: None,
                tags: HashMap::new(),
                notes: None,
                tx_parity: None,
                initiated_by: CallInitiation::Manual,
                role: QsoRole::Cqer,
                call_count: 1,
                first_call_at: Some(now),
                last_call_at: Some(now),
                progressed_this_cycle: false,
            },
        };
        manager.qsos.write().await.insert(qso_id, progress);

        let mut events = manager.subscribe();

        // Caller answers → we should emit a SignalReport.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        let mut saw_report = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::SignalReport { .. }) {
                        saw_report = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(saw_report, "manual CQer should emit a SignalReport reply");

        // Caller R-reports → we should emit a FinalConfirmation (RR73).
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();
        let mut saw_rr73 = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::FinalConfirmation { .. }) {
                        saw_rr73 = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(
            saw_rr73,
            "manual CQer should emit a FinalConfirmation (RR73)"
        );
    }

    // --- Manual `c` (CQ) → CallingCq QSO ---------------------------------

    /// Pressing `c` (`start_cq_manual`) creates an ACTIVE, manual CallingCq
    /// QSO that emits a StateChanged (so the coordinator keys it into
    /// `active_tx_qsos`) and an opening Cq MessageToSend.
    #[tokio::test]
    async fn start_cq_manual_creates_active_calling_cq_qso() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let mut events = manager.subscribe();

        let qso_id = manager.start_cq_manual(freq, None).await.unwrap();

        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::CallingCq { .. }),
            "expected CallingCq, got {:?}",
            p.state
        );
        assert_eq!(p.metadata.initiated_by, CallInitiation::Manual);
        assert_eq!(p.metadata.role, QsoRole::Cqer);
        // It is an active QSO (so it shows in the active set).
        assert_eq!(manager.get_active_qsos().await.len(), 1);

        // It emitted a StateChanged into CallingCq (keys active_tx_qsos in the
        // coordinator) and an opening Cq MessageToSend.
        let mut saw_state_change = false;
        let mut saw_cq = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::StateChanged { new_state, .. }) => {
                    if matches!(new_state, QsoState::CallingCq { .. }) {
                        saw_state_change = true;
                    }
                }
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::Cq { .. }) {
                        saw_cq = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(
            saw_state_change,
            "start_cq_manual should emit a StateChanged into CallingCq"
        );
        assert!(saw_cq, "start_cq_manual should emit an opening Cq message");
    }

    /// The full operator-CQ exchange: a caller answers our manual CQ and the
    /// exchange auto-sequences (Manual-gated auto-reply) all the way to
    /// Completed + QsoCompleted (ADIF log), latching the caller's grid.
    #[tokio::test]
    async fn start_cq_manual_caller_answer_completes_and_logs() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config()); // our call = W1ABC
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None).await.unwrap();
        let mut events = manager.subscribe();

        // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager.get_qso(qso_id).await.unwrap().state,
                QsoState::WaitingForReport { .. }
            ),
            "CallingCq + CqResponse → WaitingForReport"
        );

        // Caller rogers our report: "W1ABC K1DEF R-12".
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();

        // Caller closes with 73 → Completed.
        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .unwrap();

        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            p.state
        );
        assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
        assert!(p.metadata.end_time.is_some());

        // A QsoCompleted event fired (ADIF logger subscribes to this).
        let mut saw_completed = false;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::QsoCompleted { qso_id: id, .. }) if id == qso_id => {
                    saw_completed = true;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert!(saw_completed, "completed CQ QSO should emit QsoCompleted");
    }

    /// While calling CQ (un-answered), the per-slot rearm keeps re-emitting our
    /// CQ — exactly ONE keep-call per slot (no double-TX), bounded by the
    /// manual watchdog.
    #[tokio::test]
    async fn manual_cq_rearm_re_emits_one_cq_per_slot() {
        use tokio::sync::broadcast::error::TryRecvError;
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None).await.unwrap();
        let mut events = manager.subscribe();

        let start = Utc::now();
        // Same slot as creation (< 15s elapsed): rearm must NOT re-emit (the
        // opening CQ already went out — re-emitting now would double-TX).
        manager
            .rearm_manual_calls_at(start + Duration::seconds(5))
            .await;
        // One slot later: rearm re-emits exactly one Cq.
        manager
            .rearm_manual_calls_at(start + Duration::seconds(16))
            .await;

        let mut cq_count = 0;
        loop {
            match events.try_recv() {
                Ok(QsoEvent::MessageToSend { message, .. }) => {
                    if matches!(message, MessageType::Cq { .. }) {
                        cq_count += 1;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        assert_eq!(
            cq_count, 1,
            "rearm should emit exactly one CQ keep-call across one elapsed slot"
        );

        // Sanity: the QSO is still CallingCq and call_count advanced.
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(p.state, QsoState::CallingCq { .. }));
        assert_eq!(p.metadata.call_count, 2, "1 opening + 1 rearm");
    }

    /// The manual CQ watchdog retires an un-answered CallingCq QSO once it
    /// hits the max-calls bound (so we never CQ forever).
    #[tokio::test]
    async fn manual_cq_watchdog_retires_after_max_calls() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None).await.unwrap();

        // Drive enough slots to exceed manual_call_max_calls (default 10).
        let mut t = Utc::now();
        for _ in 0..15 {
            t += Duration::seconds(16);
            manager.rearm_manual_calls_at(t).await;
        }
        manager.check_timeouts_at(t).await;

        // QSO retired (Failed / mapping cleared → not found via get_qso after
        // cleanup, or in a terminal state).
        match manager.get_qso(qso_id).await {
            Ok(p) => assert!(
                p.state.is_terminal(),
                "watchdog should retire the CQ QSO, got {:?}",
                p.state
            ),
            Err(QsoManagerError::QsoNotFound { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// `cancel_qso` (StopCq path) cancels an un-answered CallingCq QSO.
    #[tokio::test]
    async fn manual_cq_cancel_stops_calling() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager.start_cq_manual(freq, None).await.unwrap();
        assert_eq!(manager.get_active_qsos().await.len(), 1);

        manager.cancel_qso(qso_id).await.unwrap();

        // No longer active; a subsequent rearm emits nothing for it.
        assert!(manager
            .get_active_qsos()
            .await
            .iter()
            .all(|(_, p)| !matches!(p.state, QsoState::CallingCq { .. })));
    }

    // --- Batch 2 #7 / FIX 1: case-insensitive continue --------------------

    /// A manual call to "k1def" must CONTINUE the existing active QSO with
    /// "K1DEF" (case-insensitive match), not spawn a duplicate and not
    /// supersede. The case-insensitive callsign keying is what makes the
    /// FIX-1 active-QSO lookup robust to case/format mismatches between a
    /// DX-Hunter call and a Callers reply for the same station.
    #[tokio::test]
    async fn recall_continue_is_case_insensitive() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();
        // Re-call with different case — should CONTINUE `first` (same id).
        let second = manager
            .respond_to_cq_manual("k1def".to_string(), freq, None)
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "case-different re-call must continue the same QSO"
        );
        // `first` is NOT superseded — still active.
        let first_state = manager.get_qso(first).await.unwrap().state;
        assert!(
            matches!(first_state, QsoState::RespondingToCq { .. }),
            "first QSO must remain active (not superseded), got {:?}",
            first_state
        );
        let active = manager.get_active_qsos().await;
        assert_eq!(
            active.len(),
            1,
            "exactly one active QSO after case-different re-call"
        );
        assert_eq!(active[0].0, first);
    }

    // --- Batch 2 #8: grid latched into Completed (Caller path) ------------

    /// In the Caller flow the DX's grid arrives in the opening CqResponse and
    /// must reach the logged metadata even though the close arm hard-codes
    /// grid_square: None. Drive a manual caller QSO and complete it.
    #[tokio::test]
    async fn caller_grid_latched_into_completed_metadata() {
        let manager = QsoManager::new(test_config());
        let freq = 14074000.0;
        let qso_id = manager
            .respond_to_cq_manual("K1DEF".to_string(), freq, None)
            .await
            .unwrap();

        // The DX re-sends "W1ABC K1DEF FN31" (their grid) — latch it.
        manager
            .process_message(
                MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .unwrap();

        // DX sends our report → SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -9,
                },
                "W1ABC K1DEF -09".to_string(),
                freq,
                Some(-9.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));

        // DX closes from our R directly with RR73 → Completed (grid_square None
        // in the state, but metadata.grids.theirs already latched FN31).
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF RR73".to_string(),
                freq,
                Some(-9.0),
            )
            .await
            .unwrap();
        let p = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(p.state, QsoState::Completed { .. }),
            "got {:?}",
            p.state
        );
        assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
    }
}

#[cfg(test)]
mod sender_verification_tests {
    use super::*;
    use chrono::Utc;

    fn manager_with_call(our: &str) -> QsoManager {
        let config = QsoManagerConfig {
            our_callsign: our.into(),
            our_grid: Some("FN42".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        };
        QsoManager::new(config)
    }

    #[tokio::test]
    async fn spoofed_signal_report_does_not_advance_state() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        // Attacker sends a properly-addressed report from a DIFFERENT call.
        let spoof = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -12,
        };
        let new_state = manager
            .determine_state_transition(&state, &spoof, None, CallInitiation::Auto)
            .await
            .unwrap();
        // State must NOT advance.
        assert!(matches!(new_state, QsoState::RespondingToCq { .. }));
    }

    #[tokio::test]
    async fn legitimate_signal_report_advances_state() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        let new_state = manager
            .determine_state_transition(&state, &legit, None, CallInitiation::Auto)
            .await
            .unwrap();
        assert!(
            matches!(new_state, QsoState::SendingReport { .. }),
            "expected SendingReport, got {:?}",
            new_state
        );
    }

    #[test]
    fn is_message_relevant_rejects_spoofed_sender() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let spoof = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -12,
        };
        assert!(!manager.is_message_relevant(&state, &spoof, 1500.0));
    }

    #[test]
    fn is_message_relevant_accepts_legitimate_sender() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        assert!(manager.is_message_relevant(&state, &legit, 1500.0));
    }

    #[test]
    fn is_message_relevant_tight_gate_for_initial_ambiguous_match() {
        // The tight 15 Hz gate still governs INITIAL / ambiguous matching —
        // a state with no known contra callsign (CallingCq). This preserves
        // the security-review C-1 tightening for the case where we have not
        // yet locked onto a partner. (B15 only widens the gate once a QSO is
        // ESTABLISHED; see is_message_relevant_established_qso_allows_drift.)
        let manager = manager_with_call("K5ARH");
        let state = QsoState::CallingCq {
            frequency: 1500.0,
            started_at: Utc::now(),
            call_count: 1,
        };
        // A bare report answering our CQ (A4 routing shape), addressed to us.
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // 16 Hz off, no partner latched yet → rejected (tight gate).
        assert!(!manager.is_message_relevant(&state, &legit, 1516.0));
        // 14 Hz off → accepted.
        assert!(manager.is_message_relevant(&state, &legit, 1514.0));
    }

    #[test]
    fn is_message_relevant_established_qso_allows_drift() {
        // B15: once a QSO is ESTABLISHED (contra callsign known, here a
        // RespondingToCq partner answering us with from+to+state all matching),
        // callsign+state continuity wins over the tight 15 Hz window — a DX that
        // drifted beyond 15 Hz is still routed (up to the 100 Hz established
        // bound) instead of being dropped, so an actively-answering partner can
        // complete the contact. The old 50 Hz tolerance is gone for the initial
        // case, but the established case now intentionally accepts up to 100 Hz.
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // 16 Hz, 45 Hz, 100 Hz drift from an established partner → accepted.
        assert!(manager.is_message_relevant(&state, &legit, 1516.0));
        assert!(manager.is_message_relevant(&state, &legit, 1545.0));
        assert!(manager.is_message_relevant(&state, &legit, 1600.0));
        // Beyond the 100 Hz established bound → rejected (still bounded).
        assert!(!manager.is_message_relevant(&state, &legit, 1601.0));
    }

    #[tokio::test]
    async fn spoofed_report_ack_does_not_advance_to_completion() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::SendingReport {
            their_callsign: "K9ZZ".into(),
            their_report: Some(-15),
            our_report: -10,
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let spoof = MessageType::ReportAck {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
            report: -10,
        };
        let new_state = manager
            .determine_state_transition(&state, &spoof, None, CallInitiation::Auto)
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::SendingReport { .. }));
    }

    #[tokio::test]
    async fn spoofed_final_confirmation_does_not_complete_qso() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::WaitingForConfirmation {
            their_callsign: "K9ZZ".into(),
            their_report: -15,
            our_report: -10,
            frequency: 1500.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let spoof = MessageType::FinalConfirmation {
            to_station: "K5ARH".into(),
            from_station: "NF4KE".into(),
        };
        let new_state = manager
            .determine_state_transition(&state, &spoof, None, CallInitiation::Auto)
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::WaitingForConfirmation { .. }));
    }

    #[tokio::test]
    async fn legitimate_final_confirmation_completes_qso() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::WaitingForConfirmation {
            their_callsign: "K9ZZ".into(),
            their_report: -15,
            our_report: -10,
            frequency: 1500.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let legit = MessageType::FinalConfirmation {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
        };
        let new_state = manager
            .determine_state_transition(&state, &legit, None, CallInitiation::Auto)
            .await
            .unwrap();
        assert!(matches!(new_state, QsoState::Completed { .. }));
    }
}

#[cfg(test)]
mod reply_emitter_tests {
    //! Auto-sequence reply emitter for MANUAL QSOs.
    //!
    //! Drives a manual QSO through the full inbound exchange and asserts the
    //! outbound `MessageToSend` replies (R-report → RR73 → 73) are emitted,
    //! that the QSO completes + logs, and that autonomous QSOs do NOT
    //! auto-reply.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn manager() -> QsoManager {
        let config = QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        };
        QsoManager::new(config)
    }

    /// Drain currently-buffered events into a Vec.
    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn messages_to_send(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// On-air text the coordinator would generate for an emitted reply.
    fn on_air(msg: &MessageType) -> String {
        crate::utils::generate_ft8_message(msg, OUR).unwrap()
    }

    /// 1. Manual QSO in RespondingToCq + SignalReport → emits ReportAck
    ///    (R+report) and state advances to SendingReport.
    #[tokio::test]
    async fn manual_signal_report_emits_report_ack() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx); // discard the initial CqResponse call

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(
            sends.len(),
            1,
            "expected exactly one reply, got {:?}",
            sends
        );
        match &sends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                // snr -15 → our report -15 (matches SendingReport.our_report).
                assert_eq!(*report, -15);
            }
            other => panic!("expected ReportAck, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH R-15");

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "expected SendingReport, got {:?}",
            progress.state
        );
    }

    /// 2. Manual QSO in SendingReport + ReportAck → emits FinalConfirmation
    ///    (RR73) and state advances to WaitingForConfirmation.
    #[tokio::test]
    async fn manual_report_ack_emits_final_confirmation() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1, "expected one reply, got {:?}", sends);
        match &sends[0] {
            MessageType::FinalConfirmation {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
            }
            other => panic!("expected FinalConfirmation, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH RR73");
    }

    /// 3. Manual QSO in WaitingForConfirmation + FinalConfirmation → emits
    ///    SeventyThree (73), QSO → Completed, and a QsoCompleted event fires
    ///    (so the ADIF logger logs it).
    #[tokio::test]
    async fn manual_final_confirmation_emits_73_and_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        match &sends[0] {
            MessageType::SeventyThree {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
            }
            other => panic!("expected SeventyThree, got {:?}", other),
        }
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

        // QSO completed.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        // QsoCompleted event fired (drives ADIF logging).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event"
        );
    }

    /// FIX 2: Manual QSO in SendingReport + RR73 (FinalConfirmation) → the
    /// DX rogered our R-report directly. We emit our 73, the QSO completes,
    /// and a QsoCompleted event fires (drives ADIF logging). This is the
    /// "never sent 73 / QSO stalled one message short" bug.
    #[tokio::test]
    async fn manual_sending_report_plus_rr73_emits_73_and_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport (DX sent their report; we send R-report).
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::SendingReport { .. }));
        let _ = drain(&mut rx);

        // DX closes directly with RR73 (skips a separate RRR/report-ack).
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
    }

    /// A completed QSO logs the RF frequency (dial + audio offset), not the
    /// bare offset, when a dial-frequency source is shared. Regression for the
    /// ADIF FREQ ~0.001 / BAND 0MHZ bug.
    #[tokio::test]
    async fn completed_metadata_logs_dial_plus_offset() {
        let mut manager = manager();
        manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
        let mut rx = manager.subscribe();
        let _qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let completed = events.iter().find_map(|e| match e {
            QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
            _ => None,
        });
        let metadata = completed.expect("expected a QsoCompleted event");
        // dial 14_074_000 + offset 1500 = 14_075_500 Hz (20m).
        assert_eq!(metadata.frequency, 14_074_000.0 + FREQ);
    }

    /// Without a dial source (e.g. unit tests / no rig), completed metadata
    /// keeps the value it was created with — no spurious offset added.
    #[tokio::test]
    async fn completed_metadata_unchanged_without_dial_source() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let events = drain(&mut rx);
        let metadata = events
            .iter()
            .find_map(|e| match e {
                QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
                _ => None,
            })
            .expect("expected a QsoCompleted event");
        assert_eq!(metadata.frequency, FREQ);
    }

    // --- respond_to_caller: open the exchange at a chosen ResponseStep ----

    use pancetta_core::ResponseStep;

    /// `Grid` opens exactly like the historical manual call: state
    /// `RespondingToCq` and a first message of `CqResponse` carrying our grid.
    #[tokio::test]
    async fn respond_to_caller_grid_matches_legacy_manual() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(DX.into(), FREQ, None, ResponseStep::Grid, Some(-12.0), None)
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::CqResponse {
                calling_station,
                responding_station,
                grid,
            } => {
                assert_eq!(calling_station, DX);
                assert_eq!(responding_station, OUR);
                assert_eq!(grid.as_deref(), Some("EM12"));
            }
            other => panic!("expected CqResponse, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
        assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
    }

    /// `Report` opens at state `SendingReport` (their_report None) and a first
    /// message of `SignalReport` carrying the report derived from our SNR.
    #[tokio::test]
    async fn respond_to_caller_report_emits_signal_report() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Report,
                Some(-9.0),
                None,
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::SignalReport {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -9);
            }
            other => panic!("expected SignalReport, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        match progress.state {
            QsoState::SendingReport {
                their_report,
                our_report,
                ..
            } => {
                assert_eq!(their_report, None);
                assert_eq!(our_report, -9);
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// `ReportAck` opens at state `SendingReport` (their_report Some) and a
    /// first message of `ReportAck` (R-report).
    #[tokio::test]
    async fn respond_to_caller_report_ack_emits_report_ack() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::ReportAck,
                Some(-10.0),
                Some(-3),
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        match &sends[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -10);
            }
            other => panic!("expected ReportAck, got {other:?}"),
        }
        let progress = manager.get_qso(qso_id).await.unwrap();
        match progress.state {
            QsoState::SendingReport { their_report, .. } => {
                assert_eq!(their_report, Some(-3));
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// `Rr73` opens at state `WaitingForConfirmation` and a first message of
    /// `FinalConfirmation` (RR73).
    #[tokio::test]
    async fn respond_to_caller_rr73_emits_final_confirmation() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::Rr73,
                Some(-5.0),
                Some(-7),
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        assert!(
            matches!(sends[0], MessageType::FinalConfirmation { .. }),
            "expected FinalConfirmation, got {:?}",
            sends[0]
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(
            progress.state,
            QsoState::WaitingForConfirmation { .. }
        ));
    }

    /// `SeventyThree` opens directly at `Completed`, emits a `SeventyThree`
    /// first message AND a `QsoCompleted` event so the QSO is logged.
    #[tokio::test]
    async fn respond_to_caller_seventy_three_completes_and_logs() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
            )
            .await
            .unwrap();
        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::Completed { .. }));
    }

    /// Item 2 (primary): a station keeps sending us RR73 (never copied our 73).
    /// The operator presses Space again — a repeat
    /// `respond_to_caller(SeventyThree)` for the SAME callsign must re-emit
    /// another 73 rather than getting deduped into a no-op. Because the first
    /// 73 leaves the QSO `Completed` (terminal, not active), `supersede_*` skips
    /// it and we build a fresh completed QSO that emits its own SeventyThree.
    #[tokio::test]
    async fn repeat_respond_to_caller_seventy_three_resends_73() {
        let manager = manager();
        let mut rx = manager.subscribe();

        // First 73.
        let first = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
            )
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        assert_eq!(sends.len(), 1);
        assert!(matches!(sends[0], MessageType::SeventyThree { .. }));

        // They send us RR73 again; operator presses Space again → second 73.
        let second = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::SeventyThree,
                Some(-8.0),
                Some(-4),
            )
            .await
            .unwrap();
        assert_ne!(first, second, "repeat press should build a fresh QSO");
        let sends2 = messages_to_send(&drain(&mut rx));
        assert_eq!(
            sends2.len(),
            1,
            "repeat Space must re-send a 73, not no-op: {sends2:?}"
        );
        assert!(
            matches!(sends2[0], MessageType::SeventyThree { .. }),
            "expected a second SeventyThree, got {:?}",
            sends2[0]
        );
    }

    /// `our_snr_of_them = None` falls back to a sane default report (-15).
    #[tokio::test]
    async fn respond_to_caller_defaults_report_when_no_snr() {
        let manager = manager();
        let mut rx = manager.subscribe();
        manager
            .respond_to_caller(DX.into(), FREQ, None, ResponseStep::Report, None, None)
            .await
            .unwrap();
        let sends = messages_to_send(&drain(&mut rx));
        match &sends[0] {
            MessageType::SignalReport { report, .. } => assert_eq!(*report, -15),
            other => panic!("expected SignalReport, got {other:?}"),
        }
    }

    /// A full sequence opened at `ReportAck`: we send R-report, the DX answers
    /// RR73, and the QSO completes (and logs) via the normal state machine.
    #[tokio::test]
    async fn respond_to_caller_report_ack_through_rr73_completes() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                None,
                ResponseStep::ReportAck,
                Some(-10.0),
                Some(-3),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        // DX rogers our R-report with RR73 → we close with 73 and complete.
        manager
            .process_message(
                MessageType::FinalConfirmation {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} RR73", OUR, DX),
                FREQ,
                Some(-12.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
        assert!(
            matches!(sends[0], MessageType::SeventyThree { .. }),
            "expected SeventyThree, got {:?}",
            sends[0]
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event"
        );
    }

    /// FIX 2: Manual QSO in SendingReport + a bare "73" (DX skipped RR73) →
    /// QSO completes and logs, and we do NOT re-send a 73 (they are done).
    #[tokio::test]
    async fn manual_sending_report_plus_bare_73_completes_without_resend() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let _ = drain(&mut rx);

        manager
            .process_message(
                MessageType::SeventyThree {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                },
                format!("{} {} 73", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert!(
            sends.is_empty(),
            "bare 73 close must not re-send a 73, got {:?}",
            sends
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::Completed { .. }),
            "expected Completed, got {:?}",
            progress.state
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
            "expected a QsoCompleted event (ADIF log)"
        );
    }

    /// 4. AUTONOMOUS QSO in RespondingToCq + SignalReport → state advances
    ///    but NO reply is emitted (manual-only gate).
    #[tokio::test]
    async fn auto_qso_advances_but_emits_no_reply() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq(DX.into(), FREQ, None) // CallInitiation::Auto
            .await
            .unwrap();
        let _ = drain(&mut rx); // discard initial CqResponse call

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let events = drain(&mut rx);
        let sends = messages_to_send(&events);
        assert!(
            sends.is_empty(),
            "autonomous QSO must NOT auto-reply, got {:?}",
            sends
        );
        // State still advanced (machine unchanged).
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "auto QSO state must still advance, got {:?}",
            progress.state
        );
    }

    /// 5. Spurious sender (wrong from/to) is still ignored: no state advance
    ///    and no reply emitted, even for a manual QSO.
    #[tokio::test]
    async fn spurious_sender_ignored_no_reply() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        let _ = drain(&mut rx);

        // Properly-addressed report but from a DIFFERENT callsign.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: "NF4KE".into(),
                    report: -7,
                },
                format!("{} NF4KE -07", OUR),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let sends = messages_to_send(&drain(&mut rx));
        assert!(
            sends.is_empty(),
            "spurious sender must not trigger a reply, got {:?}",
            sends
        );
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "spurious report must not advance state, got {:?}",
            progress.state
        );
    }
}

#[cfg(test)]
mod state_regression_tests {
    //! State-regression intelligence ("back up to where the DX thinks we are").
    //!
    //! When a MANUAL QSO's DX re-sends an EARLIER-stage message — meaning they
    //! never copied our most-recent transmission — the QSO machine regresses to
    //! match the DX and re-sends the appropriate response instead of stalling.
    //!
    //! Re-send duty split:
    //! - REGRESSION 1 (WaitingForConfirmation + repeated report → SendingReport):
    //!   `process_message_for_qso` emits the R-report IMMEDIATELY this slot (via
    //!   the reply emitter's new (WaitingForConfirmation, SignalReport) arm); the
    //!   per-slot `rearm_manual_calls_at` owns subsequent slots.
    //! - REGRESSION 2 (SendingReport + repeated report → stays SendingReport):
    //!   the transition does NOT emit (exchange has no (SendingReport,
    //!   SignalReport) arm); `rearm_manual_calls_at` (FIX 4) owns the R re-send.
    //!   The transition only updates the latched reports. Stamping `last_call_at`
    //!   on the regression gates rearm so the two never double-send in one slot.
    use super::*;

    const OUR: &str = "K5ARH";
    const DX: &str = "K9ZZ";
    const FREQ: f64 = 1500.0;

    fn manager_with(max_calls: u32, watchdog_min: u64) -> QsoManager {
        let mut config = QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM12".into()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        };
        config.timeouts.manual_call_max_calls = max_calls;
        config.timeouts.manual_call_watchdog_minutes = watchdog_min;
        QsoManager::new(config)
    }

    fn manager() -> QsoManager {
        manager_with(10, 5)
    }

    fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
        events
            .iter()
            .filter_map(|e| match e {
                QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Drive a manual QSO to WaitingForConfirmation (CqResponse → R → RR73 to
    /// the DX), returning the qso_id.
    async fn manual_to_waiting_confirmation(manager: &QsoManager) -> QsoId {
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::WaitingForConfirmation { .. }
        ));
        qso_id
    }

    /// REGRESSION 1: WaitingForConfirmation + repeated report → SendingReport,
    /// an R-report is re-emitted, and reports are updated to the newest value.
    #[tokio::test]
    async fn manual_waiting_confirmation_plus_repeated_report_regresses_to_sending_report() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        let _ = drain(&mut rx);

        // DX re-sends their report — with a NEW value — having never copied
        // our RR73. snr -9 → our report -9.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -3,
                },
                format!("{} {} -03", OUR, DX),
                FREQ,
                Some(-9.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        // Regressed two steps back.
        match &progress.state {
            QsoState::SendingReport {
                their_report,
                our_report,
                ..
            } => {
                assert_eq!(*their_report, Some(-3), "their report updated to newest");
                assert_eq!(*our_report, -9, "our report recomputed from newest SNR");
            }
            other => panic!("expected SendingReport, got {:?}", other),
        }

        // R-report re-emitted this slot.
        let emitted = sends(&drain(&mut rx));
        assert_eq!(
            emitted.len(),
            1,
            "expected one R re-send, got {:?}",
            emitted
        );
        match &emitted[0] {
            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => {
                assert_eq!(to_station, DX);
                assert_eq!(from_station, OUR);
                assert_eq!(*report, -9);
            }
            other => panic!("expected ReportAck, got {:?}", other),
        }
    }

    /// REGRESSION 2: SendingReport + repeated report → stays SendingReport (no
    /// spurious double-advance); rearm re-sends R (transition itself does not,
    /// avoiding a same-slot double-send).
    #[tokio::test]
    async fn manual_sending_report_repeated_report_stays_and_resends() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manager
            .respond_to_cq_manual(DX.into(), FREQ, None)
            .await
            .unwrap();
        // Advance to SendingReport.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ));
        let _ = drain(&mut rx);

        // DX re-sends their report (didn't copy our R).
        let result = manager
            .determine_state_transition(
                &manager.get_qso(qso_id).await.unwrap().state,
                &MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                Some(-15.0),
                CallInitiation::Manual,
            )
            .await
            .unwrap();
        assert!(
            matches!(result, QsoState::SendingReport { .. }),
            "must stay in SendingReport, got {:?}",
            result
        );

        // Now exercise the full path: it must NOT emit from the transition (no
        // exchange arm); the per-slot rearm owns the R re-send.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        let from_transition = sends(&drain(&mut rx));
        assert!(
            from_transition.is_empty(),
            "transition must not re-send (rearm owns it), got {:?}",
            from_transition
        );

        // A slot later, rearm re-sends our R-report (and not before — the
        // regression stamped last_call_at, so no double-send in this slot).
        let last = manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .last_call_at
            .unwrap();
        manager
            .rearm_manual_calls_at(last + Duration::seconds(15))
            .await;
        let rearmed = sends(&drain(&mut rx));
        assert_eq!(rearmed.len(), 1, "rearm re-sends R, got {:?}", rearmed);
        assert!(
            matches!(rearmed[0], MessageType::ReportAck { .. }),
            "rearm must re-send ReportAck, got {:?}",
            rearmed[0]
        );
    }

    /// Regression re-sends count against the watchdog cap: a DX that keeps
    /// repeating an earlier report cannot drive an unbounded ping-pong — the
    /// QSO retires once the cap is exceeded.
    #[tokio::test]
    async fn regression_respects_watchdog_cap() {
        // Small cap, large time window so the call cap is the binding bound.
        let manager = manager_with(3, 60);
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        // call_count is 1 at QSO start; each regression bumps it.

        // DX repeats their report several times — each is a regression re-send.
        for _ in 0..5 {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: OUR.into(),
                        from_station: DX.into(),
                        report: -7,
                    },
                    format!("{} {} -07", OUR, DX),
                    FREQ,
                    Some(-15.0),
                )
                .await
                .unwrap();
            // After the first regression we are in SendingReport; subsequent
            // repeats are REGRESSION 2 (stay) and still count.
        }

        let count = manager.get_qso(qso_id).await.unwrap().metadata.call_count;
        assert!(
            count >= 3,
            "regressions must count against cap, got {}",
            count
        );

        // The watchdog now retires the QSO rather than looping forever.
        manager.check_timeouts_at(Utc::now()).await;
        assert!(
            matches!(
                manager.get_qso(qso_id).await,
                Err(QsoManagerError::QsoNotFound { .. })
            ),
            "watchdog should retire the QSO once the cap is exceeded"
        );
    }

    /// A spurious sender (correct to:, wrong from:) does NOT trigger regression.
    #[tokio::test]
    async fn regression_requires_matching_sender() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let qso_id = manual_to_waiting_confirmation(&manager).await;
        let before = manager.get_qso(qso_id).await.unwrap().metadata.call_count;
        let _ = drain(&mut rx);

        // Properly-addressed report but from a DIFFERENT callsign.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: "NF4KE".into(),
                    report: -7,
                },
                format!("{} NF4KE -07", OUR),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        // No regression: still WaitingForConfirmation, no re-send, no count bump.
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "spurious sender must not regress, got {:?}",
            progress.state
        );
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "spurious sender must not re-send"
        );
        assert_eq!(
            progress.metadata.call_count, before,
            "spurious sender must not count against cap"
        );
    }

    /// An AUTO-initiated QSO with a repeated earlier-stage message does NOT
    /// regress or auto-resend (manual-only gate).
    #[tokio::test]
    async fn auto_qso_does_not_regress() {
        let manager = manager();
        let mut rx = manager.subscribe();
        // Build an AUTO QSO and drive it forward to WaitingForConfirmation. The
        // auto path does not auto-reply, so we drive the state directly via
        // process_message (state machine advances regardless of mode).
        let qso_id = manager.respond_to_cq(DX.into(), FREQ, None).await.unwrap();
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        manager
            .process_message(
                MessageType::ReportAck {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} R-07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::WaitingForConfirmation { .. }
        ));
        let _ = drain(&mut rx);

        // DX repeats their report. Auto QSO must NOT regress and must NOT
        // auto-resend.
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: OUR.into(),
                    from_station: DX.into(),
                    report: -7,
                },
                format!("{} {} -07", OUR, DX),
                FREQ,
                Some(-15.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
            "auto QSO must NOT regress, got {:?}",
            progress.state
        );
        assert!(
            sends(&drain(&mut rx)).is_empty(),
            "auto QSO must not auto-resend on regression"
        );
    }
}
