//! Automatic QSO progression and sequencing
//!
//! This module provides automated QSO sequencing capabilities that can
//! automatically progress through standard FT8 QSO flows with minimal
//! user intervention.

use crate::exchange::{ExchangeError, MessageExchange};
use crate::qso_manager::{QsoEvent, QsoManager};
use crate::states::*;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

/// Auto sequencer errors
#[derive(Debug, Error)]
pub enum AutoSequencerError {
    #[error("QSO manager error: {source}")]
    QsoManager {
        source: crate::qso_manager::QsoManagerError,
    },

    #[error("Message exchange error: {source}")]
    Exchange { source: ExchangeError },

    #[error("Sequencer not enabled")]
    NotEnabled,

    #[error("Invalid configuration: {message}")]
    Configuration { message: String },

    #[error("Sequence timeout: {qso_id}")]
    SequenceTimeout { qso_id: QsoId },

    #[error("Unexpected state transition: {details}")]
    UnexpectedTransition { details: String },
}

/// Auto sequencer configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AutoSequencerConfig {
    /// Enable automatic sequencing
    pub enabled: bool,

    /// CQ behavior configuration
    pub cq_behavior: CqBehaviorConfig,

    /// Response behavior configuration
    pub response_behavior: ResponseBehaviorConfig,

    /// Contest mode configuration
    pub contest_behavior: Option<ContestBehaviorConfig>,

    /// Timing configuration
    pub timing: SequenceTiming,

    /// Signal strength thresholds
    pub signal_thresholds: SignalThresholds,
}

/// CQ calling behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CqBehaviorConfig {
    /// Automatically call CQ when idle
    pub auto_cq: bool,

    /// Frequencies to use for CQ calls
    pub cq_frequencies: Vec<f64>,

    /// Maximum number of unanswered CQ calls
    pub max_cq_calls: u32,

    /// Interval between CQ calls (seconds)
    pub cq_interval: u64,

    /// Only call CQ during certain time windows
    pub time_restrictions: Option<TimeRestrictions>,
}

/// Response behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseBehaviorConfig {
    /// Automatically respond to CQ calls
    pub auto_respond: bool,

    /// Only respond to certain types of stations
    pub response_filters: ResponseFilters,

    /// Maximum concurrent QSOs
    pub max_concurrent_qsos: u32,

    /// Automatically send signal reports
    pub auto_send_reports: bool,

    /// Automatically send confirmations
    pub auto_send_confirmations: bool,
}

/// Contest behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestBehaviorConfig {
    /// Contest name
    pub contest_name: String,

    /// Automatically exchange contest information
    pub auto_exchange: bool,

    /// Skip stations already worked
    pub skip_duplicates: bool,

    /// Prioritize multipliers
    pub prioritize_multipliers: bool,

    /// Contest specific settings
    pub contest_settings: HashMap<String, String>,
}

/// Timing configuration for sequencing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceTiming {
    /// Delay between automatic actions (milliseconds)
    pub action_delay: u64,

    /// Maximum time to wait for response (seconds)
    pub response_timeout: u64,

    /// Time to wait before retrying (seconds)
    pub retry_delay: u64,

    /// Maximum number of retries
    pub max_retries: u32,
}

/// Signal strength thresholds for automatic decisions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalThresholds {
    /// Minimum SNR to respond to CQ (-30 to +50 dB)
    pub min_cq_response_snr: i8,

    /// Minimum SNR to continue QSO
    pub min_qso_continue_snr: i8,

    /// SNR threshold for weak signal handling
    pub weak_signal_threshold: i8,
}

/// Time restrictions for operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRestrictions {
    /// Start time (UTC hour 0-23)
    pub start_hour: u8,

    /// End time (UTC hour 0-23)
    pub end_hour: u8,

    /// Days of week (0=Sunday, 1=Monday, etc.)
    pub allowed_days: Vec<u8>,
}

/// Response filters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFilters {
    /// Only respond to these callsigns (empty = respond to all)
    pub allowed_callsigns: Vec<String>,

    /// Never respond to these callsigns
    pub blocked_callsigns: Vec<String>,

    /// Only respond to these countries
    pub allowed_countries: Vec<String>,

    /// Minimum signal strength to respond
    pub min_signal_strength: f32,

    /// Only respond to stations in these grid squares
    pub allowed_grid_squares: Vec<String>,
}

/// Sequence decision result
#[derive(Debug, Clone, PartialEq)]
pub enum SequenceDecision {
    /// Take the specified action
    Action(SequenceAction),

    /// Wait for the specified duration
    Wait(Duration),

    /// No action needed
    NoAction,

    /// Cancel the current sequence
    Cancel(String),
}

/// Actions the sequencer can take
#[derive(Debug, Clone, PartialEq)]
pub enum SequenceAction {
    /// Start calling CQ on frequency
    StartCq { frequency: f64 },

    /// Respond to CQ call
    RespondToCq { callsign: String, frequency: f64 },

    /// Send signal report
    SendReport { qso_id: QsoId, report: SignalReport },

    /// Send report acknowledgment
    SendReportAck { qso_id: QsoId, report: SignalReport },

    /// Send final confirmation
    SendConfirmation { qso_id: QsoId },

    /// Send 73 message
    Send73 { qso_id: QsoId },

    /// Send contest exchange
    SendContestExchange { qso_id: QsoId, serial: SerialNumber },

    /// Cancel QSO
    CancelQso { qso_id: QsoId, reason: String },
}

/// Auto sequencer implementation
pub struct AutoSequencer {
    /// Configuration
    config: AutoSequencerConfig,

    /// QSO manager reference
    qso_manager: QsoManager,

    /// Message exchange handler
    message_exchange: MessageExchange,

    /// Active sequences by QSO ID
    active_sequences: RwLock<HashMap<QsoId, SequenceState>>,

    /// Event subscription
    event_receiver: RwLock<Option<broadcast::Receiver<QsoEvent>>>,

    /// CQ call state
    cq_state: RwLock<CqState>,
}

/// State of an active sequence
#[derive(Debug, Clone)]
struct SequenceState {
    /// Last action taken
    last_action: Option<SequenceAction>,

    /// Number of retries
    retry_count: u32,

    /// Sequence start time
    started_at: DateTime<Utc>,

    /// Last activity time
    last_activity: DateTime<Utc>,

    /// Expected next state
    expected_state: Option<QsoState>,
}

/// CQ calling state
#[derive(Debug, Clone)]
struct CqState {
    /// Currently calling CQ
    calling: bool,

    /// Current CQ frequency
    frequency: Option<f64>,

    /// Number of CQ calls made
    call_count: u32,

    /// Last CQ time
    last_cq_time: Option<DateTime<Utc>>,

    /// Active CQ QSO ID
    cq_qso_id: Option<QsoId>,
}

impl AutoSequencer {
    /// Create a new auto sequencer
    pub fn new(config: AutoSequencerConfig, qso_manager: QsoManager, our_callsign: String) -> Self {
        let message_exchange = MessageExchange::new(our_callsign);

        Self {
            config,
            qso_manager,
            message_exchange,
            active_sequences: RwLock::new(HashMap::new()),
            event_receiver: RwLock::new(None),
            cq_state: RwLock::new(CqState {
                calling: false,
                frequency: None,
                call_count: 0,
                last_cq_time: None,
                cq_qso_id: None,
            }),
        }
    }

    /// Start the auto sequencer
    ///
    /// Takes an `Arc<Self>` so that all background tasks share the same state.
    /// Previously, `self.clone()` created independent state maps per task.
    pub async fn start(self: &Arc<Self>) -> Result<(), AutoSequencerError> {
        if !self.config.enabled {
            return Err(AutoSequencerError::NotEnabled);
        }

        info!("Starting auto sequencer");

        // Subscribe to QSO events
        let receiver = self.qso_manager.subscribe();
        *self.event_receiver.write().await = Some(receiver);

        // Start background tasks — all share the same Arc'd state
        let sequencer = Arc::clone(self);
        tokio::spawn(async move {
            sequencer.event_loop().await;
        });

        let sequencer = Arc::clone(self);
        tokio::spawn(async move {
            sequencer.cq_loop().await;
        });

        let sequencer = Arc::clone(self);
        tokio::spawn(async move {
            sequencer.timeout_check_loop().await;
        });

        Ok(())
    }

    /// Process incoming QSO event
    pub async fn process_event(&self, event: QsoEvent) -> Result<(), AutoSequencerError> {
        match event {
            QsoEvent::StateChanged {
                qso_id,
                old_state,
                new_state,
                ..
            } => {
                self.handle_state_change(qso_id, old_state, new_state)
                    .await?;
            }

            QsoEvent::MessageReceived { qso_id, message } => {
                self.handle_message_received(qso_id, message).await?;
            }

            QsoEvent::QsoCompleted { qso_id, .. } => {
                self.handle_qso_completed(qso_id).await?;
            }

            QsoEvent::QsoFailed { qso_id, .. } => {
                self.handle_qso_failed(qso_id).await?;
            }

            _ => {} // Other events don't require action
        }

        Ok(())
    }

    /// Evaluate CQ call from another station
    pub async fn evaluate_cq_call(
        &self,
        callsign: &str,
        frequency: f64,
        grid: Option<&str>,
        signal_strength: f32,
    ) -> Result<SequenceDecision, AutoSequencerError> {
        if !self.config.response_behavior.auto_respond {
            return Ok(SequenceDecision::NoAction);
        }

        // Check if we should respond based on filters
        if !self
            .should_respond_to_station(callsign, signal_strength, grid)
            .await
        {
            debug!("Not responding to {} due to filters", callsign);
            return Ok(SequenceDecision::NoAction);
        }

        // Check concurrent QSO limit
        let active_qsos = self.qso_manager.get_active_qsos().await;
        if active_qsos.len() >= self.config.response_behavior.max_concurrent_qsos as usize {
            debug!("Not responding to {} - too many active QSOs", callsign);
            return Ok(SequenceDecision::NoAction);
        }

        // Check signal strength threshold
        let snr = self.calculate_snr(signal_strength).await;
        if snr < self.config.signal_thresholds.min_cq_response_snr {
            debug!(
                "Not responding to {} - signal too weak ({}dB)",
                callsign, snr
            );
            return Ok(SequenceDecision::NoAction);
        }

        info!(
            "Auto-responding to CQ from {} on {:.1} Hz",
            callsign, frequency
        );

        Ok(SequenceDecision::Action(SequenceAction::RespondToCq {
            callsign: callsign.to_string(),
            frequency,
        }))
    }

    /// Execute a sequence action
    pub async fn execute_action(&self, action: SequenceAction) -> Result<(), AutoSequencerError> {
        match action {
            SequenceAction::StartCq { frequency } => {
                let qso_id = self
                    .qso_manager
                    .start_cq(frequency)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                self.start_sequence(qso_id).await;

                let mut cq_state = self.cq_state.write().await;
                cq_state.calling = true;
                cq_state.frequency = Some(frequency);
                cq_state.call_count += 1;
                cq_state.last_cq_time = Some(Utc::now());
                cq_state.cq_qso_id = Some(qso_id);
            }

            SequenceAction::RespondToCq {
                callsign,
                frequency,
            } => {
                let qso_id = self
                    .qso_manager
                    .respond_to_cq(callsign, frequency)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                self.start_sequence(qso_id).await;
            }

            SequenceAction::SendReport { qso_id, report } => {
                let progress = self
                    .qso_manager
                    .get_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                if let Some(their_callsign) = progress.state.their_callsign() {
                    let our_callsign = progress.metadata.our_callsign.clone();
                    let frequency = progress
                        .state
                        .frequency()
                        .unwrap_or(progress.metadata.frequency);
                    let their_call = their_callsign.to_string();

                    let message = MessageType::SignalReport {
                        to_station: their_call,
                        from_station: our_callsign,
                        report,
                    };

                    self.qso_manager
                        .send_message(qso_id, message, frequency)
                        .await;
                    info!("Sent signal report {:+} for QSO {}", report, qso_id);
                }

                self.update_sequence_activity(qso_id).await;
            }

            SequenceAction::SendReportAck { qso_id, report } => {
                let progress = self
                    .qso_manager
                    .get_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                if let Some(their_callsign) = progress.state.their_callsign() {
                    let our_callsign = progress.metadata.our_callsign.clone();
                    let frequency = progress
                        .state
                        .frequency()
                        .unwrap_or(progress.metadata.frequency);
                    let their_call = their_callsign.to_string();

                    let message = MessageType::ReportAck {
                        to_station: their_call,
                        from_station: our_callsign,
                        report,
                    };

                    self.qso_manager
                        .send_message(qso_id, message, frequency)
                        .await;
                    info!("Sent R{:+} for QSO {}", report, qso_id);
                }

                self.update_sequence_activity(qso_id).await;
            }

            SequenceAction::SendConfirmation { qso_id } => {
                let progress = self
                    .qso_manager
                    .get_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                if let Some(their_callsign) = progress.state.their_callsign() {
                    let our_callsign = progress.metadata.our_callsign.clone();
                    let frequency = progress
                        .state
                        .frequency()
                        .unwrap_or(progress.metadata.frequency);
                    let their_call = their_callsign.to_string();

                    let message = MessageType::FinalConfirmation {
                        to_station: their_call,
                        from_station: our_callsign,
                    };

                    self.qso_manager
                        .send_message(qso_id, message, frequency)
                        .await;
                    info!("Sent confirmation (RR73) for QSO {}", qso_id);
                }

                self.update_sequence_activity(qso_id).await;
            }

            SequenceAction::Send73 { qso_id } => {
                let progress = self
                    .qso_manager
                    .get_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                if let Some(their_callsign) = progress.state.their_callsign() {
                    let our_callsign = progress.metadata.our_callsign.clone();
                    let frequency = progress
                        .state
                        .frequency()
                        .unwrap_or(progress.metadata.frequency);
                    let their_call = their_callsign.to_string();

                    let message = MessageType::SeventyThree {
                        to_station: their_call,
                        from_station: our_callsign,
                    };

                    self.qso_manager
                        .send_message(qso_id, message, frequency)
                        .await;
                    info!("Sent 73 for QSO {}", qso_id);
                }

                self.update_sequence_activity(qso_id).await;
            }

            SequenceAction::SendContestExchange { qso_id, serial } => {
                let progress = self
                    .qso_manager
                    .get_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                if let Some(their_callsign) = progress.state.their_callsign() {
                    let our_callsign = progress.metadata.our_callsign.clone();
                    let frequency = progress
                        .state
                        .frequency()
                        .unwrap_or(progress.metadata.frequency);
                    let their_call = their_callsign.to_string();

                    let report = progress.metadata.reports.received.unwrap_or(-15);

                    let message = MessageType::ContestExchange {
                        to_station: their_call,
                        from_station: our_callsign,
                        report,
                        serial,
                    };

                    self.qso_manager
                        .send_message(qso_id, message, frequency)
                        .await;
                    info!(
                        "Sent contest exchange (serial {}) for QSO {}",
                        serial, qso_id
                    );
                }

                self.update_sequence_activity(qso_id).await;
            }

            SequenceAction::CancelQso { qso_id, reason } => {
                info!("Cancelling QSO {}: {}", qso_id, reason);
                self.qso_manager
                    .cancel_qso(qso_id)
                    .await
                    .map_err(|e| AutoSequencerError::QsoManager { source: e })?;

                self.end_sequence(qso_id).await;
            }
        }

        Ok(())
    }

    // Private helper methods

    async fn event_loop(&self) {
        // Get the receiver once and move it out
        let receiver_opt = {
            let mut guard = self.event_receiver.write().await;
            guard.take()
        };

        if let Some(mut receiver) = receiver_opt {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        if let Err(e) = self.process_event(event).await {
                            error!("Error processing event: {}", e);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("QSO event channel closed");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("Skipped {} QSO events due to lag", skipped);
                    }
                }
            }

            // Put the receiver back when done
            *self.event_receiver.write().await = Some(receiver);
        }
    }

    async fn cq_loop(&self) {
        if !self.config.cq_behavior.auto_cq {
            return;
        }

        let interval = Duration::from_secs(self.config.cq_behavior.cq_interval);

        loop {
            sleep(interval).await;

            if let Err(e) = self.check_auto_cq().await {
                error!("Error in auto CQ check: {}", e);
            }
        }
    }

    async fn timeout_check_loop(&self) {
        let check_interval = Duration::from_secs(5);

        loop {
            sleep(check_interval).await;

            if let Err(e) = self.check_sequence_timeouts().await {
                error!("Error checking sequence timeouts: {}", e);
            }
        }
    }

    async fn handle_state_change(
        &self,
        qso_id: QsoId,
        old_state: QsoState,
        new_state: QsoState,
    ) -> Result<(), AutoSequencerError> {
        debug!(
            "QSO {} state changed: {:?} -> {:?}",
            qso_id, old_state, new_state
        );

        // Update sequence state
        self.update_sequence_activity(qso_id).await;

        // Determine if automatic action is needed
        if let Some(action) = self.determine_auto_action(&new_state, qso_id).await {
            // Add delay before taking action
            sleep(Duration::from_millis(self.config.timing.action_delay)).await;

            if let Err(e) = self.execute_action(action).await {
                error!("Error executing auto action for QSO {}: {}", qso_id, e);
            }
        }

        Ok(())
    }

    async fn handle_message_received(
        &self,
        qso_id: QsoId,
        message: QsoMessage,
    ) -> Result<(), AutoSequencerError> {
        debug!(
            "Message received for QSO {}: {:?}",
            qso_id, message.message_type
        );

        self.update_sequence_activity(qso_id).await;

        Ok(())
    }

    async fn handle_qso_completed(&self, qso_id: QsoId) -> Result<(), AutoSequencerError> {
        info!("QSO {} completed", qso_id);
        self.end_sequence(qso_id).await;

        // Update CQ state if this was a CQ QSO
        let mut cq_state = self.cq_state.write().await;
        if cq_state.cq_qso_id == Some(qso_id) {
            cq_state.calling = false;
            cq_state.cq_qso_id = None;
        }

        Ok(())
    }

    async fn handle_qso_failed(&self, qso_id: QsoId) -> Result<(), AutoSequencerError> {
        warn!("QSO {} failed", qso_id);
        self.end_sequence(qso_id).await;

        // Update CQ state if this was a CQ QSO
        let mut cq_state = self.cq_state.write().await;
        if cq_state.cq_qso_id == Some(qso_id) {
            cq_state.calling = false;
            cq_state.cq_qso_id = None;
        }

        Ok(())
    }

    async fn determine_auto_action(
        &self,
        state: &QsoState,
        qso_id: QsoId,
    ) -> Option<SequenceAction> {
        match state {
            QsoState::WaitingForReport { .. }
                if self.config.response_behavior.auto_send_reports =>
            {
                // Look up the actual SNR from the most recent received message
                let report = self.get_received_snr(qso_id).await.unwrap_or(-15);
                Some(SequenceAction::SendReport { qso_id, report })
            }

            // SendingReport means we sent our report (e.g., "K1DEF W1ABC -05").
            // The correct next step is to WAIT for their R-report acknowledgment,
            // NOT to send RR73 immediately. The old code sent RR73 here, skipping
            // the R-report exchange entirely and completing QSOs prematurely.
            QsoState::SendingReport { .. } => {
                // No action — wait for the other station's R-report reply.
                // When we receive it, the state will transition to
                // WaitingForConfirmation, which is handled below.
                None
            }

            QsoState::WaitingForConfirmation { .. }
                if self.config.response_behavior.auto_send_confirmations =>
            {
                Some(SequenceAction::SendConfirmation { qso_id })
            }

            QsoState::SendingConfirmation { .. } => Some(SequenceAction::Send73 { qso_id }),

            _ => None,
        }
    }

    /// Extract the SNR from the most recent received message for a QSO
    async fn get_received_snr(&self, qso_id: QsoId) -> Option<SignalReport> {
        let progress = self.qso_manager.get_qso(qso_id).await.ok()?;

        // First check the metadata reports
        if let Some(received) = progress.metadata.reports.received {
            return Some(received);
        }

        // Fall back to computing from the last received message's signal_strength
        let last_received = progress
            .messages
            .iter()
            .rev()
            .find(|m| m.direction == MessageDirection::Received)?;

        let signal_strength = last_received.signal_strength?;
        Some(self.calculate_snr(signal_strength).await)
    }

    async fn check_auto_cq(&self) -> Result<(), AutoSequencerError> {
        let cq_state = self.cq_state.read().await;

        // Don't start new CQ if already calling
        if cq_state.calling {
            return Ok(());
        }

        // Check if we've exceeded max CQ calls
        if cq_state.call_count >= self.config.cq_behavior.max_cq_calls {
            return Ok(());
        }

        // Check time restrictions
        if !self.is_time_allowed().await {
            return Ok(());
        }

        // Check if we have too many active QSOs
        let active_qsos = self.qso_manager.get_active_qsos().await;
        if active_qsos.len() >= self.config.response_behavior.max_concurrent_qsos as usize {
            return Ok(());
        }

        drop(cq_state);

        // Select frequency for CQ
        if let Some(frequency) = self.select_cq_frequency().await {
            info!("Starting auto CQ on {:.1} Hz", frequency);

            let action = SequenceAction::StartCq { frequency };
            self.execute_action(action).await?;
        }

        Ok(())
    }

    async fn select_cq_frequency(&self) -> Option<f64> {
        // Simple frequency selection - could be enhanced with band plan awareness
        if let Some(frequency) = self.config.cq_behavior.cq_frequencies.first() {
            Some(*frequency)
        } else {
            Some(14074000.0) // Default FT8 frequency
        }
    }

    async fn is_time_allowed(&self) -> bool {
        if let Some(restrictions) = &self.config.cq_behavior.time_restrictions {
            let now = Utc::now();
            let hour = now.time().hour() as u8;
            let weekday = now.weekday().num_days_from_sunday() as u8;

            // Check hour restrictions
            if restrictions.start_hour <= restrictions.end_hour {
                if hour < restrictions.start_hour || hour > restrictions.end_hour {
                    return false;
                }
            } else {
                // Overnight allowed window (e.g., start=22, end=6 means allowed 22-06 UTC).
                // The disallowed hours are those BOTH before start AND after end,
                // i.e., the gap in the middle of the day (e.g., 07-21 for start=22, end=6).
                if hour < restrictions.start_hour && hour > restrictions.end_hour {
                    return false;
                }
            }

            // Check day restrictions
            if !restrictions.allowed_days.is_empty()
                && !restrictions.allowed_days.contains(&weekday)
            {
                return false;
            }
        }

        true
    }

    async fn should_respond_to_station(
        &self,
        callsign: &str,
        signal_strength: f32,
        grid: Option<&str>,
    ) -> bool {
        let filters = &self.config.response_behavior.response_filters;

        // Check blocked callsigns
        if filters.blocked_callsigns.contains(&callsign.to_string()) {
            return false;
        }

        // Check allowed callsigns (if specified)
        if !filters.allowed_callsigns.is_empty()
            && !filters.allowed_callsigns.contains(&callsign.to_string())
        {
            return false;
        }

        // Check signal strength
        if signal_strength < filters.min_signal_strength {
            return false;
        }

        // Check grid squares (if specified)
        if !filters.allowed_grid_squares.is_empty() {
            if let Some(grid) = grid {
                if !filters
                    .allowed_grid_squares
                    .iter()
                    .any(|allowed| grid.starts_with(allowed))
                {
                    return false;
                }
            } else {
                return false; // No grid provided but we have restrictions
            }
        }

        true
    }

    async fn calculate_snr(&self, signal_strength: f32) -> i8 {
        // Simple SNR calculation - in practice this would use actual noise measurements
        let noise_floor = -25.0; // Typical FT8 noise floor
        (signal_strength - noise_floor).round() as i8
    }

    async fn start_sequence(&self, qso_id: QsoId) {
        let sequence_state = SequenceState {
            last_action: None,
            retry_count: 0,
            started_at: Utc::now(),
            last_activity: Utc::now(),
            expected_state: None,
        };

        self.active_sequences
            .write()
            .await
            .insert(qso_id, sequence_state);
        debug!("Started sequence for QSO: {}", qso_id);
    }

    async fn end_sequence(&self, qso_id: QsoId) {
        self.active_sequences.write().await.remove(&qso_id);
        debug!("Ended sequence for QSO: {}", qso_id);
    }

    async fn update_sequence_activity(&self, qso_id: QsoId) {
        if let Some(sequence) = self.active_sequences.write().await.get_mut(&qso_id) {
            sequence.last_activity = Utc::now();
        }
    }

    async fn check_sequence_timeouts(&self) -> Result<(), AutoSequencerError> {
        let now = Utc::now();
        let timeout_duration =
            chrono::Duration::seconds(self.config.timing.response_timeout as i64);
        let mut timeouts = Vec::new();

        {
            let sequences = self.active_sequences.read().await;
            for (&qso_id, sequence) in sequences.iter() {
                if now - sequence.last_activity > timeout_duration {
                    timeouts.push(qso_id);
                }
            }
        }

        for qso_id in timeouts {
            warn!("Sequence timeout for QSO: {}", qso_id);

            let action = SequenceAction::CancelQso {
                qso_id,
                reason: "Sequence timeout".to_string(),
            };

            if let Err(e) = self.execute_action(action).await {
                error!("Error cancelling timed out QSO {}: {}", qso_id, e);
            }
        }

        Ok(())
    }
}

// NOTE: AutoSequencer must NOT be cloned for spawning background tasks.
// The old Clone impl created independent state for each clone, meaning
// spawned tasks (event_loop, cq_loop, timeout_check_loop) would operate
// on completely separate state maps. Use Arc<AutoSequencer> instead.
// Clone is intentionally not implemented.

impl Default for CqBehaviorConfig {
    fn default() -> Self {
        Self {
            auto_cq: false,
            cq_frequencies: vec![14074000.0], // 20m FT8
            max_cq_calls: 5,
            cq_interval: 300, // 5 minutes
            time_restrictions: None,
        }
    }
}

impl Default for ResponseBehaviorConfig {
    fn default() -> Self {
        Self {
            auto_respond: false,
            response_filters: ResponseFilters::default(),
            max_concurrent_qsos: 3,
            auto_send_reports: false,
            auto_send_confirmations: false,
        }
    }
}

impl Default for ResponseFilters {
    fn default() -> Self {
        Self {
            allowed_callsigns: vec![],
            blocked_callsigns: vec![],
            allowed_countries: vec![],
            min_signal_strength: -30.0,
            allowed_grid_squares: vec![],
        }
    }
}

impl Default for SequenceTiming {
    fn default() -> Self {
        Self {
            action_delay: 1000,   // 1 second
            response_timeout: 60, // 1 minute
            retry_delay: 15,      // 15 seconds
            max_retries: 3,
        }
    }
}

impl Default for SignalThresholds {
    fn default() -> Self {
        Self {
            min_cq_response_snr: -20,   // -20 dB minimum for responding to CQ
            min_qso_continue_snr: -25,  // -25 dB minimum to continue QSO
            weak_signal_threshold: -15, // -15 dB considered weak signal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qso_manager::{
        AutoSequenceConfig as QsoAutoConfig, DuplicateCheckConfig, QsoManagerConfig, TimeoutConfig,
    };

    fn test_config() -> AutoSequencerConfig {
        AutoSequencerConfig {
            enabled: true,
            cq_behavior: CqBehaviorConfig {
                auto_cq: true,
                cq_frequencies: vec![14074000.0],
                max_cq_calls: 3,
                cq_interval: 60,
                time_restrictions: None,
            },
            response_behavior: ResponseBehaviorConfig {
                auto_respond: true,
                response_filters: ResponseFilters::default(),
                max_concurrent_qsos: 2,
                auto_send_reports: true,
                auto_send_confirmations: true,
            },
            contest_behavior: None,
            timing: SequenceTiming::default(),
            signal_thresholds: SignalThresholds::default(),
        }
    }

    fn test_qso_manager_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: QsoAutoConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_auto_sequencer_creation() {
        let qso_manager = QsoManager::new(test_qso_manager_config());
        let sequencer = AutoSequencer::new(test_config(), qso_manager, "W1ABC".to_string());

        assert!(sequencer.config.enabled);
        assert!(sequencer.config.response_behavior.auto_respond);
    }

    #[tokio::test]
    async fn test_cq_evaluation() {
        let qso_manager = QsoManager::new(test_qso_manager_config());
        let sequencer = AutoSequencer::new(test_config(), qso_manager, "W1ABC".to_string());

        let decision = sequencer
            .evaluate_cq_call(
                "K1DEF",
                14074000.0,
                Some("FN31"),
                -10.0, // Good signal
            )
            .await
            .unwrap();

        match decision {
            SequenceDecision::Action(SequenceAction::RespondToCq {
                callsign,
                frequency,
            }) => {
                assert_eq!(callsign, "K1DEF");
                assert_eq!(frequency, 14074000.0);
            }
            _ => panic!("Expected RespondToCq action"),
        }
    }
}
