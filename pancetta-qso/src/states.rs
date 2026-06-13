//! QSO state definitions for FT8 communications
//!
//! This module defines the various states a QSO can be in during the
//! standard FT8 communication flow and contest operations.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Unique identifier for a QSO
pub type QsoId = Uuid;

/// Signal report in dB
pub type SignalReport = i8;

/// Grid square locator (e.g., "FN42")
pub type GridSquare = String;

/// Contest serial number
pub type SerialNumber = u32;

/// QSO state in the FT8 communication flow
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QsoState {
    /// Initial state - no communication started
    Idle,

    /// Calling CQ and waiting for response
    CallingCq {
        frequency: f64,
        started_at: DateTime<Utc>,
        call_count: u32,
    },

    /// Responding to a CQ call
    RespondingToCq {
        target_callsign: String,
        frequency: f64,
        started_at: DateTime<Utc>,
    },

    /// Waiting for signal report
    WaitingForReport {
        their_callsign: String,
        frequency: f64,
        started_at: DateTime<Utc>,
    },

    /// Sending signal report
    SendingReport {
        their_callsign: String,
        their_report: Option<SignalReport>,
        our_report: SignalReport,
        frequency: f64,
        started_at: DateTime<Utc>,
    },

    /// Waiting for confirmation (RR73 or similar)
    WaitingForConfirmation {
        their_callsign: String,
        their_report: SignalReport,
        our_report: SignalReport,
        frequency: f64,
        grid_square: Option<GridSquare>,
        started_at: DateTime<Utc>,
    },

    /// Sending final confirmation
    SendingConfirmation {
        their_callsign: String,
        their_report: SignalReport,
        our_report: SignalReport,
        frequency: f64,
        grid_square: Option<GridSquare>,
        started_at: DateTime<Utc>,
    },

    /// QSO completed successfully
    Completed {
        their_callsign: String,
        their_report: SignalReport,
        our_report: SignalReport,
        frequency: f64,
        grid_square: Option<GridSquare>,
        completed_at: DateTime<Utc>,
        duration_seconds: u32,
    },

    /// QSO failed or timed out
    Failed {
        reason: QsoFailureReason,
        failed_at: DateTime<Utc>,
        last_state: Box<QsoState>,
    },

    /// Contest-specific states
    Contest(ContestState),
}

/// Contest-specific QSO states
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContestState {
    /// Exchanging contest information (serial numbers, etc.)
    ExchangingInfo {
        their_callsign: String,
        our_serial: SerialNumber,
        their_serial: Option<SerialNumber>,
        frequency: f64,
        started_at: DateTime<Utc>,
    },

    /// Contest QSO completed
    ContestCompleted {
        their_callsign: String,
        our_serial: SerialNumber,
        their_serial: SerialNumber,
        frequency: f64,
        completed_at: DateTime<Utc>,
        contest_category: String,
    },
}

/// Reasons why a QSO might fail
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QsoFailureReason {
    /// Timeout waiting for response
    Timeout,

    /// Signal lost or too weak
    SignalLost,

    /// Duplicate QSO detected
    Duplicate,

    /// Invalid callsign format
    InvalidCallsign,

    /// Frequency conflict
    FrequencyConflict,

    /// User cancelled the QSO
    UserCancelled,

    /// Other station went QRT
    StationQrt,

    /// Protocol error
    ProtocolError(String),
}

/// FT8 message types used in QSO flow
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// CQ call: "CQ W1ABC FN42"
    Cq {
        callsign: String,
        grid: Option<GridSquare>,
    },

    /// Response to CQ: "W1ABC K1DEF FN31"
    CqResponse {
        calling_station: String,
        responding_station: String,
        grid: Option<GridSquare>,
    },

    /// Signal report: "K1DEF W1ABC -15"
    SignalReport {
        to_station: String,
        from_station: String,
        report: SignalReport,
    },

    /// Report acknowledgment: "W1ABC K1DEF R-12"
    ReportAck {
        to_station: String,
        from_station: String,
        report: SignalReport,
    },

    /// Final confirmation: "K1DEF W1ABC RR73"
    FinalConfirmation {
        to_station: String,
        from_station: String,
    },

    /// 73 message: "W1ABC K1DEF 73"
    SeventyThree {
        to_station: String,
        from_station: String,
    },

    /// Contest exchange: "W1ABC K1DEF 599 001"
    ContestExchange {
        to_station: String,
        from_station: String,
        report: SignalReport,
        serial: SerialNumber,
    },

    /// Non-standard message
    NonStandard { text: String },
}

/// QSO progress tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoProgress {
    /// Current state
    pub state: QsoState,

    /// State history
    pub state_history: Vec<StateTransition>,

    /// Messages exchanged
    pub messages: Vec<QsoMessage>,

    /// QSO metadata
    pub metadata: QsoMetadata,
}

/// State transition record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateTransition {
    /// Previous state
    pub from_state: QsoState,

    /// New state
    pub to_state: QsoState,

    /// Timestamp of transition
    pub timestamp: DateTime<Utc>,

    /// Reason for transition
    pub reason: TransitionReason,
}

/// Reasons for state transitions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransitionReason {
    /// Message received
    MessageReceived(MessageType),

    /// Message sent
    MessageSent(MessageType),

    /// Timeout occurred
    Timeout,

    /// User initiated
    UserAction,

    /// Automatic progression
    AutoSequence,

    /// Error condition
    Error(String),
}

/// QSO message record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoMessage {
    /// Message timestamp
    pub timestamp: DateTime<Utc>,

    /// Message direction
    pub direction: MessageDirection,

    /// Message content
    pub message_type: MessageType,

    /// Raw message text
    pub raw_text: String,

    /// Signal strength when received
    pub signal_strength: Option<f32>,

    /// Frequency
    pub frequency: f64,
}

/// Message direction
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDirection {
    /// Message we sent
    Sent,

    /// Message we received
    Received,
}

/// QSO metadata and additional information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoMetadata {
    /// QSO unique identifier
    pub qso_id: QsoId,

    /// Our callsign
    pub our_callsign: String,

    /// Their callsign (if known)
    pub their_callsign: Option<String>,

    /// Operating frequency in Hz
    pub frequency: f64,

    /// Operating mode (should be "FT8")
    pub mode: String,

    /// QSO start time
    pub start_time: DateTime<Utc>,

    /// QSO end time (if completed)
    pub end_time: Option<DateTime<Utc>>,

    /// Signal reports
    pub reports: SignalReports,

    /// Grid squares
    pub grids: GridSquares,

    /// Contest information
    pub contest_info: Option<ContestInfo>,

    /// Additional tags
    pub tags: HashMap<String, String>,

    /// Notes
    pub notes: Option<String>,

    /// Latched TX parity for this QSO. Set once at QSO creation
    /// (respond_to_cq passes the DX's parity, which is flipped to
    /// the opposite for our TX; start_cq passes our self-parity
    /// directly). Every subsequent MessageToSend event for this QSO
    /// carries the same value, ensuring all of our transmissions stay
    /// on the slot the contra station expects.
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,

    /// How this QSO was initiated. Manual calls bypass the self-duplicate
    /// gate and keep-call under the manual watchdog; auto calls do not.
    /// Defaults to `Auto` (the pre-existing behavior for every internal
    /// constructor that does not set it explicitly).
    #[serde(default)]
    pub initiated_by: CallInitiation,

    /// Number of times we have transmitted the initial call for this QSO
    /// (relevant only for manual keep-calling). Starts at 1 when the QSO
    /// is created (the first call is emitted immediately) and increments
    /// on each watchdog re-arm. Drives the `manual_call_max_calls` cap.
    #[serde(default)]
    pub call_count: u32,

    /// Timestamp of the first call transmitted for this QSO. Used by the
    /// manual watchdog's elapsed-time bound (`manual_call_watchdog_minutes`).
    #[serde(default)]
    pub first_call_at: Option<DateTime<Utc>>,

    /// Timestamp of the most recent call transmitted (manual keep-calling).
    /// Used to rate-limit re-arms to roughly one per FT8 slot.
    #[serde(default)]
    pub last_call_at: Option<DateTime<Utc>>,
}

/// Signal reports exchanged
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalReports {
    /// Report we sent
    pub sent: Option<SignalReport>,

    /// Report we received
    pub received: Option<SignalReport>,
}

/// Grid squares exchanged
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GridSquares {
    /// Our grid square
    pub ours: Option<GridSquare>,

    /// Their grid square
    pub theirs: Option<GridSquare>,
}

/// Contest-specific information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestInfo {
    /// Contest name/identifier
    pub contest_name: String,

    /// Contest category
    pub category: String,

    /// Serial numbers
    pub serials: ContestSerials,

    /// Points value
    pub points: u32,

    /// Multiplier information
    pub multiplier: Option<String>,
}

/// Contest serial numbers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestSerials {
    /// Serial number we sent
    pub sent: Option<SerialNumber>,

    /// Serial number we received
    pub received: Option<SerialNumber>,
}

/// How a QSO was initiated.
///
/// Distinguishes operator-driven manual calls from autonomous-operator
/// calls. Manual calls bypass the self-duplicate gate (the operator
/// explicitly chose to call this station) and keep-call every TX slot
/// under a watchdog; autonomous calls retain the duplicate gate and the
/// autonomous operator's own cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CallInitiation {
    /// Operator pressed call / Space — explicit, bypasses duplicate check
    /// and keep-calls under the manual watchdog.
    Manual,

    /// Autonomous operator initiated — duplicate check applies, autonomous
    /// cadence drives re-calls (no manual keep-calling).
    #[default]
    Auto,
}

/// QSO validation result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QsoValidation {
    /// QSO is valid
    Valid,

    /// QSO is invalid with reason
    Invalid(String),

    /// QSO is duplicate
    Duplicate {
        original_qso_id: QsoId,
        original_timestamp: DateTime<Utc>,
    },
}

impl QsoState {
    /// Check if the QSO is in a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(self, QsoState::Completed { .. } | QsoState::Failed { .. })
    }

    /// Check if the QSO is active (not idle or terminal)
    pub fn is_active(&self) -> bool {
        !matches!(self, QsoState::Idle) && !self.is_terminal()
    }

    /// Get the current frequency if available
    pub fn frequency(&self) -> Option<f64> {
        match self {
            QsoState::CallingCq { frequency, .. }
            | QsoState::RespondingToCq { frequency, .. }
            | QsoState::WaitingForReport { frequency, .. }
            | QsoState::SendingReport { frequency, .. }
            | QsoState::WaitingForConfirmation { frequency, .. }
            | QsoState::SendingConfirmation { frequency, .. }
            | QsoState::Completed { frequency, .. } => Some(*frequency),
            QsoState::Contest(ContestState::ExchangingInfo { frequency, .. }) => Some(*frequency),
            QsoState::Contest(ContestState::ContestCompleted { frequency, .. }) => Some(*frequency),
            _ => None,
        }
    }

    /// Get the other station's callsign if known
    pub fn their_callsign(&self) -> Option<&str> {
        match self {
            QsoState::RespondingToCq {
                target_callsign, ..
            } => Some(target_callsign),
            QsoState::WaitingForReport { their_callsign, .. }
            | QsoState::SendingReport { their_callsign, .. }
            | QsoState::WaitingForConfirmation { their_callsign, .. }
            | QsoState::SendingConfirmation { their_callsign, .. }
            | QsoState::Completed { their_callsign, .. } => Some(their_callsign),
            QsoState::Contest(ContestState::ExchangingInfo { their_callsign, .. }) => {
                Some(their_callsign)
            }
            QsoState::Contest(ContestState::ContestCompleted { their_callsign, .. }) => {
                Some(their_callsign)
            }
            _ => None,
        }
    }

    /// Get the duration of the current state
    pub fn state_duration(&self, now: DateTime<Utc>) -> Option<chrono::Duration> {
        let start_time = match self {
            QsoState::CallingCq { started_at, .. }
            | QsoState::RespondingToCq { started_at, .. }
            | QsoState::WaitingForReport { started_at, .. }
            | QsoState::SendingReport { started_at, .. }
            | QsoState::WaitingForConfirmation { started_at, .. }
            | QsoState::SendingConfirmation { started_at, .. } => Some(*started_at),
            QsoState::Contest(ContestState::ExchangingInfo { started_at, .. }) => Some(*started_at),
            _ => None,
        };

        start_time.map(|start| now - start)
    }
}

impl MessageType {
    /// Check if this message type is addressed to a specific station
    pub fn is_addressed_to(&self, callsign: &str) -> bool {
        match self {
            MessageType::CqResponse {
                calling_station, ..
            } => calling_station == callsign,
            MessageType::SignalReport { to_station, .. }
            | MessageType::ReportAck { to_station, .. }
            | MessageType::FinalConfirmation { to_station, .. }
            | MessageType::SeventyThree { to_station, .. }
            | MessageType::ContestExchange { to_station, .. } => to_station == callsign,
            _ => false,
        }
    }

    /// Check if this message type is from a specific station
    pub fn is_from(&self, callsign: &str) -> bool {
        match self {
            MessageType::Cq { callsign: call, .. } => call == callsign,
            MessageType::CqResponse {
                responding_station, ..
            } => responding_station == callsign,
            MessageType::SignalReport { from_station, .. }
            | MessageType::ReportAck { from_station, .. }
            | MessageType::FinalConfirmation { from_station, .. }
            | MessageType::SeventyThree { from_station, .. }
            | MessageType::ContestExchange { from_station, .. } => from_station == callsign,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qso_state_terminal() {
        let completed = QsoState::Completed {
            their_callsign: "W1ABC".to_string(),
            their_report: -15,
            our_report: -10,
            frequency: 14074000.0,
            grid_square: Some("FN42".to_string()),
            completed_at: Utc::now(),
            duration_seconds: 120,
        };
        assert!(completed.is_terminal());
        assert!(!completed.is_active());

        let active = QsoState::WaitingForReport {
            their_callsign: "W1ABC".to_string(),
            frequency: 14074000.0,
            started_at: Utc::now(),
        };
        assert!(!active.is_terminal());
        assert!(active.is_active());

        let idle = QsoState::Idle;
        assert!(!idle.is_terminal());
        assert!(!idle.is_active());
    }

    #[test]
    fn test_message_addressing() {
        let msg = MessageType::SignalReport {
            to_station: "W1ABC".to_string(),
            from_station: "K1DEF".to_string(),
            report: -15,
        };

        assert!(msg.is_addressed_to("W1ABC"));
        assert!(!msg.is_addressed_to("K1DEF"));
        assert!(msg.is_from("K1DEF"));
        assert!(!msg.is_from("W1ABC"));
    }
}
