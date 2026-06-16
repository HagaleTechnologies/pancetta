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
        /// Their grid square, latched from the opening CqResponse (CQer
        /// flow) so it can be carried through to the logged QSO. `None`
        /// when the response carried no grid.
        #[serde(default)]
        their_grid: Option<GridSquare>,
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

    /// Replaced by a more recent QSO with the same station on the same band.
    /// Operator policy: when two exchanges exist for one (callsign, band),
    /// the more recent one wins and the older is superseded.
    Superseded,

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

    /// Which side of the exchange we play. `Cqer` when the QSO began with
    /// `start_cq` (we called CQ and a station answered), `Caller` otherwise
    /// (the historical responder path). Drives the role-aware display
    /// ladder; the state machine itself is role-agnostic. Defaults to
    /// `Caller` for every internal constructor that does not set it.
    #[serde(default)]
    pub role: QsoRole,

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

    /// C3 race guard: set `true` whenever this (manual) QSO makes a forward
    /// state advance, and cleared by each manual-watchdog pass. It grants a
    /// one-pass reprieve from `manual_call_max_calls` retirement to a QSO that
    /// advanced in the same slot the cap trips (a just-in-time DX answer), so
    /// the operator never loses a QSO the DX just came back on. It does NOT
    /// reset `call_count`, so the per-QSO cap still bounds total calls across
    /// the whole QSO (C12 per-QSO, not per-step semantics): a QSO that advances
    /// once then goes silent still retires at the cap on the following pass.
    #[serde(default)]
    pub progressed_this_cycle: bool,
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

/// Which side of the QSO we play.
///
/// FT8 has two roles that share the middle states (`WaitingForReport`,
/// `SendingReport`, `WaitingForConfirmation`) but ride DIFFERENT ladders:
///
/// - [`QsoRole::Cqer`] — we called CQ; a station answered. Our rungs are
///   `CQ → Grid → Rpt → R-Rpt → RR73`.
/// - [`QsoRole::Caller`] — we answered someone's CQ (or replied to a
///   station calling us). Our rungs are `Grid → Rpt → R-Rpt → RR73 → 73`.
///
/// The role is latched once at QSO creation (CQer when started via
/// `start_cq`, Caller otherwise) and carried in [`QsoMetadata`]. It is
/// used purely to pick the correct [`QsoLadderView`] for display — the
/// state-machine transitions are role-agnostic (they verify sender/target
/// and the message type, which already disambiguate direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QsoRole {
    /// We called CQ; the other station answered us.
    Cqer,

    /// We answered the other station (default — the historical responder path).
    #[default]
    Caller,
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

/// A read-only, role-aware display ladder for an in-progress QSO.
///
/// Derived from a [`QsoState`] plus the [`CallInitiation`] of the QSO. The
/// ladder shows the canonical FT8 exchange sequence as a row of rungs, which
/// rungs are ours to transmit, where we currently are, and human-readable
/// "now"/"next" lines. Intended purely for the UI; carries no QSO-engine
/// semantics over the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QsoLadderView {
    /// Rung labels, left-to-right.
    pub labels: Vec<&'static str>,
    /// Per-rung flag: `true` if the message at that rung is one WE transmit.
    pub ours: Vec<bool>,
    /// Index of the current rung (where we are right now).
    pub index: usize,
    /// What we're doing this moment (e.g. "sending R-15").
    pub now: String,
    /// What we expect next (e.g. "expect their RR73").
    pub next: String,
}

impl QsoState {
    /// Derive a [`QsoLadderView`] for this state given the QSO's [`QsoRole`].
    /// Returns `None` for terminal states (Completed/Failed), Idle, and
    /// Contest.
    ///
    /// The middle states (`WaitingForReport`, `SendingReport`,
    /// `WaitingForConfirmation`) are shared by both roles but ride DIFFERENT
    /// ladders — a CQer in `WaitingForReport` has already sent CQ and our
    /// report, and is waiting for the caller's R-report; a Caller in
    /// `WaitingForReport` has sent its grid and is waiting for the CQer's
    /// report. The `role` argument disambiguates so the panel shows the right
    /// rungs and now/next lines for each side.
    pub fn ladder_view(&self, role: QsoRole) -> Option<QsoLadderView> {
        // CQer ladder: we called CQ. Rungs: CQ → Grid(theirs) → Rpt(ours) →
        // R-Rpt(theirs) → RR73(ours).
        const CQER_LABELS: [&str; 5] = ["CQ", "Grid", "Rpt", "R-Rpt", "RR73"];
        const CQER_OURS: [bool; 5] = [true, false, true, false, true];
        // Caller ladder: we answered/called them. Rungs: Grid(ours) →
        // Rpt(theirs) → R-Rpt(ours) → RR73(theirs) → 73(ours).
        const CALLER_LABELS: [&str; 5] = ["Grid", "Rpt", "R-Rpt", "RR73", "73"];
        const CALLER_OURS: [bool; 5] = [true, false, true, false, true];

        let cqer = matches!(role, QsoRole::Cqer);

        match self {
            QsoState::CallingCq { .. } => Some(QsoLadderView {
                labels: CQER_LABELS.to_vec(),
                ours: CQER_OURS.to_vec(),
                index: 0,
                now: "calling CQ".to_string(),
                next: "expect a caller".to_string(),
            }),
            QsoState::SendingConfirmation { .. } => Some(QsoLadderView {
                labels: CQER_LABELS.to_vec(),
                ours: CQER_OURS.to_vec(),
                index: 4,
                now: "sending RR73".to_string(),
                next: "their 73 — QSO logged".to_string(),
            }),
            QsoState::RespondingToCq { .. } => Some(QsoLadderView {
                labels: CALLER_LABELS.to_vec(),
                ours: CALLER_OURS.to_vec(),
                index: 0,
                now: "sending our grid/call".to_string(),
                next: "expect their report".to_string(),
            }),
            // CQer: caller answered (our CQ → their grid), we now wait to send
            // our report. Caller: we sent grid, we wait for their report.
            QsoState::WaitingForReport { .. } if cqer => Some(QsoLadderView {
                labels: CQER_LABELS.to_vec(),
                ours: CQER_OURS.to_vec(),
                index: 1,
                now: "caller answered".to_string(),
                next: "we send their report".to_string(),
            }),
            QsoState::WaitingForReport { .. } => Some(QsoLadderView {
                labels: CALLER_LABELS.to_vec(),
                ours: CALLER_OURS.to_vec(),
                index: 1,
                now: "waiting".to_string(),
                next: "their signal report".to_string(),
            }),
            // CQer: sending the caller their report; expect their R-report.
            QsoState::SendingReport { our_report, .. } if cqer => Some(QsoLadderView {
                labels: CQER_LABELS.to_vec(),
                ours: CQER_OURS.to_vec(),
                index: 2,
                now: format!("sending {our_report:+}"),
                next: "expect their R-report".to_string(),
            }),
            QsoState::SendingReport { our_report, .. } => Some(QsoLadderView {
                labels: CALLER_LABELS.to_vec(),
                ours: CALLER_OURS.to_vec(),
                index: 2,
                now: format!("sending R{our_report:+}"),
                next: "expect their RR73".to_string(),
            }),
            // CQer: got the caller's R-report, we now send RR73 to close.
            QsoState::WaitingForConfirmation { .. } if cqer => Some(QsoLadderView {
                labels: CQER_LABELS.to_vec(),
                ours: CQER_OURS.to_vec(),
                index: 4,
                now: "sending RR73".to_string(),
                next: "their 73 — QSO logged".to_string(),
            }),
            QsoState::WaitingForConfirmation { .. } => Some(QsoLadderView {
                labels: CALLER_LABELS.to_vec(),
                ours: CALLER_OURS.to_vec(),
                index: 3,
                now: "waiting".to_string(),
                next: "their RR73 — we log + send 73".to_string(),
            }),
            QsoState::Idle
            | QsoState::Completed { .. }
            | QsoState::Failed { .. }
            | QsoState::Contest(_) => None,
        }
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

    /// The sender's displayed callsign, if this message type carries one.
    ///
    /// Used by the compound-callsign equivalence logic (catalog C18): when the
    /// DX's displayed call changes compound↔base mid-QSO, we keep the
    /// most-complete form (the compound carries DX/portable info) for logging
    /// while still matching the partner via `callsigns_match`. Returns `None`
    /// for message types with no single sender field (e.g. `NonStandard`).
    pub fn sender_callsign(&self) -> Option<&str> {
        match self {
            MessageType::Cq { callsign, .. } => Some(callsign),
            MessageType::CqResponse {
                responding_station, ..
            } => Some(responding_station),
            MessageType::SignalReport { from_station, .. }
            | MessageType::ReportAck { from_station, .. }
            | MessageType::FinalConfirmation { from_station, .. }
            | MessageType::SeventyThree { from_station, .. }
            | MessageType::ContestExchange { from_station, .. } => Some(from_station),
            MessageType::NonStandard { .. } => None,
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
            their_grid: None,
        };
        assert!(!active.is_terminal());
        assert!(active.is_active());

        let idle = QsoState::Idle;
        assert!(!idle.is_terminal());
        assert!(!idle.is_active());
    }

    #[test]
    fn test_ladder_view_caller_flow() {
        let resp = QsoState::RespondingToCq {
            target_callsign: "W1ABC".to_string(),
            frequency: 14074000.0,
            started_at: Utc::now(),
        };
        let v = resp.ladder_view(QsoRole::Caller).unwrap();
        assert_eq!(v.index, 0);
        assert_eq!(v.ours, vec![true, false, true, false, true]);
        assert_eq!(v.labels, vec!["Grid", "Rpt", "R-Rpt", "RR73", "73"]);
        assert!(v.now.contains("grid"));
        assert!(!v.next.is_empty());

        let wait_rpt = QsoState::WaitingForReport {
            their_callsign: "W1ABC".to_string(),
            frequency: 14074000.0,
            started_at: Utc::now(),
            their_grid: None,
        };
        let v = wait_rpt.ladder_view(QsoRole::Caller).unwrap();
        assert_eq!(v.index, 1);
        assert!(v.now.contains("waiting"));
        assert!(v.next.contains("report"));

        let send_rpt = QsoState::SendingReport {
            their_callsign: "W1ABC".to_string(),
            their_report: Some(-12),
            our_report: -15,
            frequency: 14074000.0,
            started_at: Utc::now(),
        };
        let v = send_rpt.ladder_view(QsoRole::Caller).unwrap();
        assert_eq!(v.index, 2);
        assert!(
            v.now.contains("-15"),
            "now-string should contain signed report: {}",
            v.now
        );
        assert!(!v.next.is_empty());

        let send_rpt_pos = QsoState::SendingReport {
            their_callsign: "W1ABC".to_string(),
            their_report: None,
            our_report: 5,
            frequency: 14074000.0,
            started_at: Utc::now(),
        };
        let v = send_rpt_pos.ladder_view(QsoRole::Caller).unwrap();
        assert!(
            v.now.contains("+5"),
            "now-string should contain signed report: {}",
            v.now
        );

        let wait_conf = QsoState::WaitingForConfirmation {
            their_callsign: "W1ABC".to_string(),
            their_report: -10,
            our_report: -15,
            frequency: 14074000.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let v = wait_conf.ladder_view(QsoRole::Caller).unwrap();
        assert_eq!(v.index, 3);
        assert!(v.now.contains("waiting"));
        assert!(!v.next.is_empty());
    }

    #[test]
    fn test_ladder_view_cqer_role_shared_states() {
        // The middle states ride the CQer ladder when role == Cqer.
        let wait_rpt = QsoState::WaitingForReport {
            their_callsign: "W1ABC".to_string(),
            frequency: 14074000.0,
            started_at: Utc::now(),
            their_grid: None,
        };
        let v = wait_rpt.ladder_view(QsoRole::Cqer).unwrap();
        assert_eq!(v.labels, vec!["CQ", "Grid", "Rpt", "R-Rpt", "RR73"]);
        assert_eq!(v.index, 1);

        let send_rpt = QsoState::SendingReport {
            their_callsign: "W1ABC".to_string(),
            their_report: None,
            our_report: -7,
            frequency: 14074000.0,
            started_at: Utc::now(),
        };
        let v = send_rpt.ladder_view(QsoRole::Cqer).unwrap();
        assert_eq!(v.labels, vec!["CQ", "Grid", "Rpt", "R-Rpt", "RR73"]);
        assert_eq!(v.index, 2);
        assert!(v.now.contains("-7"), "cqer now: {}", v.now);

        let wait_conf = QsoState::WaitingForConfirmation {
            their_callsign: "W1ABC".to_string(),
            their_report: -10,
            our_report: -15,
            frequency: 14074000.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let v = wait_conf.ladder_view(QsoRole::Cqer).unwrap();
        assert_eq!(v.labels, vec!["CQ", "Grid", "Rpt", "R-Rpt", "RR73"]);
        assert_eq!(v.index, 4);
        assert!(v.now.contains("RR73"));
    }

    #[test]
    fn test_ladder_view_cqer_flow() {
        let cq = QsoState::CallingCq {
            frequency: 14074000.0,
            started_at: Utc::now(),
            call_count: 1,
        };
        let v = cq.ladder_view(QsoRole::Cqer).unwrap();
        assert_eq!(v.index, 0);
        assert_eq!(v.labels, vec!["CQ", "Grid", "Rpt", "R-Rpt", "RR73"]);
        assert_eq!(v.ours, vec![true, false, true, false, true]);
        assert!(v.now.contains("CQ"));
        assert!(!v.next.is_empty());

        let send_conf = QsoState::SendingConfirmation {
            their_callsign: "W1ABC".to_string(),
            their_report: -10,
            our_report: -15,
            frequency: 14074000.0,
            grid_square: None,
            started_at: Utc::now(),
        };
        let v = send_conf.ladder_view(QsoRole::Cqer).unwrap();
        assert_eq!(v.index, 4);
        assert!(v.now.contains("RR73"));
        assert!(!v.next.is_empty());
    }

    #[test]
    fn test_ladder_view_terminal_none() {
        let completed = QsoState::Completed {
            their_callsign: "W1ABC".to_string(),
            their_report: -15,
            our_report: -10,
            frequency: 14074000.0,
            grid_square: None,
            completed_at: Utc::now(),
            duration_seconds: 120,
        };
        assert!(completed.ladder_view(QsoRole::Caller).is_none());

        let failed = QsoState::Failed {
            reason: QsoFailureReason::UserCancelled,
            failed_at: Utc::now(),
            last_state: Box::new(QsoState::Idle),
        };
        assert!(failed.ladder_view(QsoRole::Caller).is_none());

        assert!(QsoState::Idle.ladder_view(QsoRole::Caller).is_none());
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
