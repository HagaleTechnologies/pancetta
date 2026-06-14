//! QSO state machine and management
//!
//! This module provides the core QSO management functionality including
//! state transitions, timeout handling, and QSO lifecycle management.

use crate::async_database::AsyncQsoDatabase;
use crate::states::*;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
        }
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
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
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

        // FIX 3: supersede any existing active QSO with this callsign on the
        // same band before creating the new one. Operator policy: "if there
        // are two exchanges on the same band from the same callsign, use the
        // state of whichever is more recent." Re-calling a station already in
        // an active QSO previously spawned a SECOND concurrent QSO; both
        // transmitted to the same DX on the same freq. We retire the older
        // one (→ Failed{Superseded}, mapping removed) so exactly one QSO per
        // (callsign, band) remains active.
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
            // The first call is emitted immediately below (the CqResponse
            // MessageToSend), so the count starts at 1.
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target_callsign, qso_id).await;

        // Send response message
        let message = MessageType::CqResponse {
            calling_station: target_callsign.clone(),
            responding_station: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

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

    /// Get next contest serial number
    pub async fn get_next_serial(&self) -> SerialNumber {
        let mut next_serial = self.next_serial.write().await;
        let serial = *next_serial;
        *next_serial += 1;
        serial
    }

    // Internal helper methods

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

        if new_state != old_state {
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
                if let Some(grid) = grid_square {
                    progress.metadata.grids.theirs = Some(grid.clone());
                }
            }

            let completed_metadata = if matches!(&new_state, QsoState::Completed { .. }) {
                Some(progress.metadata.clone())
            } else {
                None
            };

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
            }
        }

        self.emit_event(QsoEvent::MessageReceived { qso_id, message })
            .await;

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
            // CQ call received response
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::CqResponse {
                    responding_station, ..
                },
            ) => Ok(QsoState::WaitingForReport {
                their_callsign: responding_station.clone(),
                frequency: *frequency,
                started_at: Utc::now(),
            }),

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
                if from_station != target_callsign || to_station != &self.config.our_callsign {
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
                if from_station != their_callsign || to_station != &self.config.our_callsign {
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
                if from_station != their_callsign || to_station != &self.config.our_callsign {
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
                if from_station != their_callsign || to_station != &self.config.our_callsign {
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
                if from_station != their_callsign || to_station != &self.config.our_callsign {
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
                if from_station != their_callsign || to_station != &self.config.our_callsign {
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
                if responding_station != their_callsign
                    || calling_station != &self.config.our_callsign
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
        if let Some(qso_freq) = state.frequency() {
            if (qso_freq - frequency).abs() > FREQ_TOLERANCE_HZ {
                return false;
            }
        }

        match (state, message_type) {
            // We're calling CQ. The responder's callsign is whoever is in the
            // `responding_station` field; the message must be addressed to us.
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    calling_station, ..
                },
            ) => calling_station == &self.config.our_callsign,

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
            ) => from_station == target_callsign && to_station == &self.config.our_callsign,

            // We sent the report and are waiting for the report-ack. Same check.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => from_station == their_callsign && to_station == &self.config.our_callsign,

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
            ) => from_station == their_callsign && to_station == &self.config.our_callsign,

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
            ) => from_station == their_callsign && to_station == &self.config.our_callsign,

            _ => {
                // Anything else: only relevant if addressed to us.
                message_type.is_addressed_to(&self.config.our_callsign)
            }
        }
    }

    async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
    ) -> Result<bool, QsoManagerError> {
        if !self.config.duplicate_checking.enabled {
            return Ok(false);
        }

        // Check in-memory active/recent QSOs first
        let qsos_by_callsign = self.qsos_by_callsign.read().await;
        if let Some(qso_ids) = qsos_by_callsign.get(callsign) {
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
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        qsos_by_callsign
            .entry(callsign.to_string())
            .or_insert_with(Vec::new)
            .push(qso_id);
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

        // Collect the QSO IDs to supersede under the read lock, then mutate.
        let to_supersede: Vec<QsoId> = {
            let qsos = self.qsos.read().await;
            let ids = match self.qsos_by_callsign.read().await.get(callsign) {
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
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        if let Some(qso_ids) = qsos_by_callsign.get_mut(callsign) {
            qso_ids.retain(|&id| id != qso_id);
            if qso_ids.is_empty() {
                qsos_by_callsign.remove(callsign);
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

        for (&qso_id, progress) in qsos.iter() {
            // Manual keep-calling watchdog. Covers both RespondingToCq
            // (re-calling the DX) and SendingReport (FIX 4: re-sending our
            // R-report when the DX repeats their report). In both phases the
            // operator is actively keep-calling, and `call_count` /
            // `first_call_at` span the whole QSO, so the 10-calls / 5-min
            // bound applies to the QSO as a whole. Once the DX advances past
            // these states (ReportAck/RR73 received), the normal state
            // timeouts take over.
            if progress.metadata.initiated_by == CallInitiation::Manual
                && matches!(
                    progress.state,
                    QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. }
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

                if progress.metadata.call_count >= max_calls || elapsed >= watchdog {
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

    /// FIX 3: two manual calls to the same callsign on the same band leave
    /// exactly ONE active QSO (the newer); the older is superseded and
    /// removed from the active set and its callsign mapping.
    #[tokio::test]
    async fn manual_recall_supersedes_older_active_qso() {
        let manager = QsoManager::new(test_config());
        let first = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let second = manager
            .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        assert_ne!(first, second);

        // Exactly one active QSO for this callsign, and it is the newer one.
        let active = manager.get_active_qsos().await;
        let active_for_dx: Vec<_> = active
            .iter()
            .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            active_for_dx,
            vec![second],
            "only the newer QSO should remain active"
        );

        // The older QSO is Failed{Superseded} (still retrievable until cleanup).
        let old = manager.get_qso(first).await.unwrap();
        assert!(
            matches!(
                old.state,
                QsoState::Failed {
                    reason: QsoFailureReason::Superseded,
                    ..
                }
            ),
            "older QSO must be Superseded, got {:?}",
            old.state
        );

        // The callsign mapping points only to the newer QSO so subsequent
        // routing / re-arm operates on exactly one QSO.
        let mapping = manager.qsos_by_callsign.read().await;
        assert_eq!(
            mapping.get("K1DEF").map(|v| v.as_slice()),
            Some([second].as_slice()),
            "mapping must point only to the newer QSO"
        );
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
    fn is_message_relevant_rejects_offset_15hz_or_more() {
        // Tightened from 50 Hz to 15 Hz.
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
        // 16 Hz off → should be rejected.
        assert!(!manager.is_message_relevant(&state, &legit, 1516.0));
        // 14 Hz off → should be accepted.
        assert!(manager.is_message_relevant(&state, &legit, 1514.0));
    }

    #[test]
    fn is_message_relevant_rejects_50hz_offset_now() {
        // Regression guard: the old 50 Hz tolerance must be gone.
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
        assert!(!manager.is_message_relevant(&state, &legit, 1545.0));
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
