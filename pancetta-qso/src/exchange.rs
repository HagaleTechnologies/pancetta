//! Message exchange logic for FT8 QSO operations
//!
//! This module handles the parsing and generation of FT8 messages
//! according to the standard protocol and contest variations.

use crate::states::*;
use lazy_static::lazy_static;
use regex::Regex;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Message parsing and generation errors
#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("Invalid message format: {message}")]
    InvalidFormat { message: String },

    #[error("Invalid callsign: {callsign}")]
    InvalidCallsign { callsign: String },

    #[error("Invalid grid square: {grid}")]
    InvalidGrid { grid: String },

    #[error("Invalid signal report: {report}")]
    InvalidReport { report: String },

    #[error("Unsupported message type")]
    UnsupportedType,

    #[error("Message parsing error: {details}")]
    ParseError { details: String },

    #[error("Missing capture group {group} in message: {message}")]
    MissingCapture { group: usize, message: String },
}

/// FT8 message exchange handler
pub struct MessageExchange {
    /// Our callsign for message validation
    our_callsign: String,

    /// Contest mode configuration
    contest_mode: Option<ContestExchangeConfig>,

    /// Next contest serial number (incremented on each use)
    contest_serial: std::sync::atomic::AtomicU32,

    /// Path for persisting the contest serial number across restarts
    serial_persist_path: Option<PathBuf>,
}

/// Contest exchange configuration
#[derive(Debug, Clone)]
pub struct ContestExchangeConfig {
    /// Contest name
    pub contest_name: String,

    /// Exchange format (e.g., "RST + Serial", "RST + State")
    pub exchange_format: ExchangeFormat,

    /// Our contest exchange
    pub our_exchange: String,
}

/// Contest exchange formats
#[derive(Debug, Clone, PartialEq)]
pub enum ExchangeFormat {
    /// RST + Serial number
    RstSerial,

    /// RST + State/Province
    RstState,

    /// RST + Grid square
    RstGrid,

    /// Custom format
    Custom(String),
}

lazy_static! {
    /// Callsign validation regex
    static ref CALLSIGN_REGEX: Regex = Regex::new(
        r"^[A-Z0-9]{1,3}[0-9][A-Z0-9]{0,3}[A-Z]$|^[A-Z]{1,2}[0-9][A-Z0-9]{0,4}$"
    ).unwrap();

    /// Grid square validation regex (4 or 6 characters)
    static ref GRID_REGEX: Regex = Regex::new(r"^[A-R]{2}[0-9]{2}([A-X]{2})?$").unwrap();

    /// CQ message patterns
    static ref CQ_PATTERNS: Vec<Regex> = vec![
        Regex::new(r"^CQ\s+([A-Z0-9/]+)(?:\s+([A-R]{2}[0-9]{2}(?:[A-X]{2})?))?$").unwrap(),
        Regex::new(r"^CQ\s+([A-Z]+)\s+([A-Z0-9/]+)(?:\s+([A-R]{2}[0-9]{2}(?:[A-X]{2})?))?$").unwrap(),
    ];

    /// Standard QSO message patterns
    static ref QSO_PATTERNS: Vec<Regex> = vec![
        // Response to CQ: "W1ABC K1DEF FN31"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)(?:\s+([A-R]{2}[0-9]{2}(?:[A-X]{2})?))?$").unwrap(),
        // Signal report: "K1DEF W1ABC -15" or "K1DEF W1ABC R-12"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(R? ?[+-]?\d{1,2})$").unwrap(),
        // Final confirmation: "W1ABC K1DEF RR73" or "W1ABC K1DEF 73"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(RR73|73)$").unwrap(),
        // Contest exchange: "W1ABC K1DEF 599 001"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(\d{3})\s+(\d{3,4})$").unwrap(),
    ];
}

impl MessageExchange {
    /// Create a new message exchange handler.
    ///
    /// The contest serial number is loaded from `~/.pancetta/contest_serial.txt`
    /// if that file exists, so it persists across restarts.
    pub fn new(our_callsign: String) -> Self {
        let serial_persist_path: Option<PathBuf> = dirs::home_dir().map(|mut p| {
            p.push(".pancetta");
            p.push("contest_serial.txt");
            p
        });

        let starting_serial = serial_persist_path
            .as_deref()
            .map(Self::load_serial)
            .unwrap_or(1);

        Self {
            our_callsign,
            contest_mode: None,
            contest_serial: std::sync::atomic::AtomicU32::new(starting_serial),
            serial_persist_path,
        }
    }

    /// Save the current contest serial number to the persistence file.
    ///
    /// Silently ignores I/O errors so a missing or unwritable file never
    /// disrupts normal operation.
    fn save_serial(&self, serial: u32) {
        if let Some(ref path) = self.serial_persist_path {
            // Best-effort: create parent directory if needed
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, serial.to_string());
        }
    }

    /// Load a contest serial number from `path`.
    ///
    /// Returns 1 if the file does not exist or cannot be parsed.
    fn load_serial(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(1)
    }

    /// Enable contest mode
    pub fn set_contest_mode(&mut self, config: ContestExchangeConfig) {
        self.contest_mode = Some(config);
    }

    /// Disable contest mode
    pub fn disable_contest_mode(&mut self) {
        self.contest_mode = None;
    }

    /// Parse an incoming FT8 message
    pub fn parse_message(&self, message: &str) -> Result<MessageType, ExchangeError> {
        let message = message.trim().to_uppercase();

        // Try CQ patterns first
        if message.starts_with("CQ") {
            return self.parse_cq_message(&message);
        }

        // Try standard QSO patterns
        for pattern in QSO_PATTERNS.iter() {
            if let Some(captures) = pattern.captures(&message) {
                return self.parse_qso_message(&message, captures);
            }
        }

        // Handle non-standard messages
        Ok(MessageType::NonStandard { text: message })
    }

    /// Generate an outgoing FT8 message
    pub fn generate_message(&self, message_type: &MessageType) -> Result<String, ExchangeError> {
        match message_type {
            MessageType::Cq { callsign, grid } => match outbound_grid_field(grid.as_deref()) {
                Some(grid) => Ok(format!("CQ {} {}", callsign, grid)),
                None => Ok(format!("CQ {}", callsign)),
            },

            MessageType::CqResponse {
                calling_station,
                responding_station,
                grid,
            } => match outbound_grid_field(grid.as_deref()) {
                Some(grid) => Ok(format!(
                    "{} {} {}",
                    calling_station, responding_station, grid
                )),
                None => Ok(format!("{} {}", calling_station, responding_station)),
            },

            MessageType::SignalReport {
                to_station,
                from_station,
                report,
            } => Ok(format!("{} {} {:+}", to_station, from_station, report)),

            MessageType::ReportAck {
                to_station,
                from_station,
                report,
            } => Ok(format!("{} {} R{:+}", to_station, from_station, report)),

            MessageType::FinalConfirmation {
                to_station,
                from_station,
            } => Ok(format!("{} {} RR73", to_station, from_station)),

            MessageType::SeventyThree {
                to_station,
                from_station,
            } => Ok(format!("{} {} 73", to_station, from_station)),

            MessageType::ContestExchange {
                to_station,
                from_station,
                report,
                serial,
            } => Ok(format!(
                "{} {} {:03} {:03}",
                to_station,
                from_station,
                (report + 35) as u32,
                serial
            )),

            MessageType::NonStandard { text } => Ok(text.clone()),
        }
    }

    /// Validate a callsign format
    pub fn validate_callsign(&self, callsign: &str) -> Result<(), ExchangeError> {
        if !CALLSIGN_REGEX.is_match(callsign) {
            return Err(ExchangeError::InvalidCallsign {
                callsign: callsign.to_string(),
            });
        }
        Ok(())
    }

    /// Validate a grid square format
    pub fn validate_grid(&self, grid: &str) -> Result<(), ExchangeError> {
        if !GRID_REGEX.is_match(grid) {
            return Err(ExchangeError::InvalidGrid {
                grid: grid.to_string(),
            });
        }
        Ok(())
    }

    /// Validate a signal report
    pub fn validate_report(&self, report: i8) -> Result<(), ExchangeError> {
        if !(-50..=50).contains(&report) {
            return Err(ExchangeError::InvalidReport {
                report: report.to_string(),
            });
        }
        Ok(())
    }

    /// Generate appropriate response message based on QSO state
    ///
    /// `snr` is the measured signal-to-noise ratio of the received signal,
    /// used to compute the signal report we send back.
    pub fn generate_response(
        &self,
        current_state: &QsoState,
        received_message: &MessageType,
        snr: Option<f32>,
    ) -> Result<Option<MessageType>, ExchangeError> {
        // Compute signal report from SNR, defaulting to -10 if unavailable
        let computed_report = snr.map(|s| (s.round() as i8).clamp(-30, 50)).unwrap_or(-10);

        match (current_state, received_message) {
            // Received response to our CQ
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    responding_station, ..
                },
            ) => Ok(Some(MessageType::SignalReport {
                to_station: responding_station.clone(),
                from_station: self.our_callsign.clone(),
                report: computed_report,
            })),

            // Received signal report, send acknowledgment
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport { report: _, .. },
            ) => Ok(Some(MessageType::ReportAck {
                to_station: target_callsign.clone(),
                from_station: self.our_callsign.clone(),
                report: computed_report,
            })),

            // Received report acknowledgment, send final confirmation
            (QsoState::SendingReport { their_callsign, .. }, MessageType::ReportAck { .. }) => {
                Ok(Some(MessageType::FinalConfirmation {
                    to_station: their_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                }))
            }

            // FIX 2: the DX rogered our R-report directly with RR73 — answer
            // our 73 to close (then the QSO completes and logs).
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::FinalConfirmation { .. },
            ) => Ok(Some(MessageType::SeventyThree {
                to_station: their_callsign.clone(),
                from_station: self.our_callsign.clone(),
            })),

            // FIX 2: the DX closed with a plain 73 (not RR73). The QSO is
            // already complete from their side; we log it and do NOT re-send
            // a 73 (they are done — re-sending only adds QRM).
            (QsoState::SendingReport { .. }, MessageType::SeventyThree { .. }) => Ok(None),

            // Received final confirmation, send 73
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation { .. },
            ) => Ok(Some(MessageType::SeventyThree {
                to_station: their_callsign.clone(),
                from_station: self.our_callsign.clone(),
            })),

            // STATE REGRESSION (manual QSOs): we sent RR73 but the DX is still
            // sending us their report — they never copied our R. The state
            // machine has just regressed us to SendingReport; re-send our
            // R-report so the DX can advance. Mirrors the
            // (RespondingToCq, SignalReport) arm above.
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::SignalReport { .. },
            ) => Ok(Some(MessageType::ReportAck {
                to_station: their_callsign.clone(),
                from_station: self.our_callsign.clone(),
                report: computed_report,
            })),

            // STATE REGRESSION (manual QSOs): we sent RR73 but the DX re-sent
            // their original grid/call (CqResponse) — they restarted the
            // exchange. The state machine has regressed us to RespondingToCq;
            // re-send our grid/call (CqResponse) so the DX can re-sync.
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::CqResponse { .. },
            ) => Ok(Some(MessageType::CqResponse {
                calling_station: their_callsign.clone(),
                responding_station: self.our_callsign.clone(),
                grid: None,
            })),

            // Contest mode responses
            _ if self.contest_mode.is_some() => {
                self.generate_contest_response(current_state, received_message, computed_report)
            }

            // No response needed
            _ => Ok(None),
        }
    }

    /// Calculate signal report from signal strength
    pub fn calculate_signal_report(&self, signal_strength: f32, noise_floor: f32) -> SignalReport {
        let snr = signal_strength - noise_floor;

        // Convert SNR to FT8 report scale
        let report = (snr.round() as i8).clamp(-30, 50);

        // Round to nearest 3 dB for FT8 convention
        ((report + 1) / 3) * 3
    }

    /// Extract frequency information from message
    pub fn extract_frequency_info(&self, _message: &str) -> Option<f64> {
        // FT8 messages don't typically contain frequency information
        // This would be determined by the receive frequency
        None
    }

    /// Check if message is a duplicate
    pub fn is_duplicate_message(
        &self,
        message: &MessageType,
        previous_messages: &[QsoMessage],
    ) -> bool {
        previous_messages.iter().any(|prev| {
            std::mem::discriminant(&prev.message_type) == std::mem::discriminant(message)
                && prev.message_type == *message
        })
    }

    // Private helper methods

    fn parse_cq_message(&self, message: &str) -> Result<MessageType, ExchangeError> {
        for pattern in CQ_PATTERNS.iter() {
            if let Some(captures) = pattern.captures(message) {
                let callsign = captures
                    .get(1)
                    .ok_or_else(|| ExchangeError::ParseError {
                        details: "Missing callsign in CQ".to_string(),
                    })?
                    .as_str()
                    .to_string();

                self.validate_callsign(&callsign)?;

                let grid = if captures.len() > 2 {
                    captures.get(captures.len() - 1).and_then(|m| {
                        let grid_str = m.as_str();
                        if self.validate_grid(grid_str).is_ok() {
                            Some(grid_str.to_string())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };

                return Ok(MessageType::Cq { callsign, grid });
            }
        }

        Err(ExchangeError::InvalidFormat {
            message: message.to_string(),
        })
    }

    fn parse_qso_message(
        &self,
        message: &str,
        captures: regex::Captures,
    ) -> Result<MessageType, ExchangeError> {
        let station1 = captures
            .get(1)
            .ok_or_else(|| ExchangeError::ParseError {
                details: "Missing first callsign".to_string(),
            })?
            .as_str()
            .to_string();

        let station2 = captures
            .get(2)
            .ok_or_else(|| ExchangeError::ParseError {
                details: "Missing second callsign".to_string(),
            })?
            .as_str()
            .to_string();

        self.validate_callsign(&station1)?;
        self.validate_callsign(&station2)?;

        // Determine message type based on pattern
        if captures.len() == 3 {
            // Could be response to CQ with no grid
            Ok(MessageType::CqResponse {
                calling_station: station1,
                responding_station: station2,
                grid: None,
            })
        } else if captures.len() == 4 {
            let third_field = captures
                .get(3)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 3,
                    message: message.to_string(),
                })?
                .as_str();

            // Check for the protocol close tokens FIRST. "RR73" is a
            // syntactically valid Maidenhead grid (field RR, square 73), so it
            // ALSO matches GRID_REGEX — if we tested the grid first, a DX's
            // closing "DX K5ARH RR73" would be misclassified as a CqResponse
            // carrying grid "RR73", the FinalConfirmation arm would never fire,
            // and the QSO would stall one message short of completion (the DX's
            // RR73 ignored, our 73 never sent, nothing logged). This was the
            // NP4VA/T46FCR stall. The FT8 convention is unambiguous: "RR73" /
            // "73" in the third field after two callsigns is always the close,
            // never a grid.
            if third_field == "RR73" {
                Ok(MessageType::FinalConfirmation {
                    to_station: station1,
                    from_station: station2,
                })
            } else if third_field == "73" {
                Ok(MessageType::SeventyThree {
                    to_station: station1,
                    from_station: station2,
                })
            }
            // Check if it's a grid square
            else if GRID_REGEX.is_match(third_field) {
                Ok(MessageType::CqResponse {
                    calling_station: station1,
                    responding_station: station2,
                    grid: Some(third_field.to_string()),
                })
            }
            // Check if it's a signal report
            else if let Ok(report) = third_field.trim_start_matches('R').trim().parse::<i8>() {
                self.validate_report(report)?;

                if third_field.starts_with('R') {
                    Ok(MessageType::ReportAck {
                        to_station: station1,
                        from_station: station2,
                        report,
                    })
                } else {
                    Ok(MessageType::SignalReport {
                        to_station: station1,
                        from_station: station2,
                        report,
                    })
                }
            } else {
                Err(ExchangeError::InvalidFormat {
                    message: message.to_string(),
                })
            }
        } else if captures.len() == 5 {
            // Contest exchange format
            let report_str = captures
                .get(3)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 3,
                    message: message.to_string(),
                })?
                .as_str();
            let serial_str = captures
                .get(4)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 4,
                    message: message.to_string(),
                })?
                .as_str();

            let report = (report_str
                .parse::<u32>()
                .map_err(|_| ExchangeError::InvalidReport {
                    report: report_str.to_string(),
                })? as i8)
                - 35; // Convert from 599 format to dB

            let serial =
                serial_str
                    .parse::<SerialNumber>()
                    .map_err(|_| ExchangeError::ParseError {
                        details: format!("Invalid serial number: {}", serial_str),
                    })?;

            Ok(MessageType::ContestExchange {
                to_station: station1,
                from_station: station2,
                report,
                serial,
            })
        } else {
            Err(ExchangeError::InvalidFormat {
                message: message.to_string(),
            })
        }
    }

    fn generate_contest_response(
        &self,
        current_state: &QsoState,
        received_message: &MessageType,
        computed_report: i8,
    ) -> Result<Option<MessageType>, ExchangeError> {
        let _contest_config = self
            .contest_mode
            .as_ref()
            .ok_or(ExchangeError::InvalidFormat {
                message: "contest mode not configured".to_string(),
            })?;

        match (current_state, received_message) {
            // Received contest exchange, send our exchange
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::ContestExchange { serial: _, .. },
            ) => {
                let serial = self
                    .contest_serial
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Persist the next serial (serial + 1) so a restart picks up where we left off.
                self.save_serial(serial + 1);
                Ok(Some(MessageType::ContestExchange {
                    to_station: target_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                    report: computed_report,
                    serial,
                }))
            }

            _ => Ok(None),
        }
    }
}

/// Normalize a configured grid square into the field carried by a standard
/// FT8 type-1 "call call grid" message.
///
/// The standard message only carries a **4-character** Maidenhead grid.
/// Passing a 6-character grid (e.g. `"EM10ch"`) to the FT8 encoder silently
/// drops the grid (it is not a valid 4-char locator, so `packgrid` falls back
/// to the no-grid token) — the transmission degrades to a bare callsign. To
/// avoid that, we truncate to the first 4 characters and uppercase them here,
/// at the single message-generation boundary, so a 6-char configured grid is
/// still displayed in full elsewhere but the transmitted standard message
/// uses the proper 4-char form.
///
/// Returns:
/// - `Some(grid4)` — uppercased, first 4 chars — when a non-empty grid is set
/// - `None` — when the grid is `None` or empty (CQ / call without grid)
fn outbound_grid_field(grid: Option<&str>) -> Option<String> {
    let grid = grid?.trim();
    if grid.is_empty() {
        return None;
    }
    // First 4 chars (the 4-char Maidenhead field), uppercased. `chars` so a
    // non-ASCII grid never panics on a byte-slice boundary; real grids are
    // ASCII so this matches `[..4]` for them.
    Some(grid.chars().take(4).collect::<String>().to_uppercase())
}

/// Message validation result
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationResult {
    /// Message is valid
    Valid,

    /// Message has warnings but is usable
    Warning(String),

    /// Message is invalid
    Invalid(String),
}

/// Validate a complete message exchange sequence
pub fn validate_exchange_sequence(messages: &[MessageType]) -> ValidationResult {
    if messages.is_empty() {
        return ValidationResult::Invalid("Empty message sequence".to_string());
    }

    // Check for proper QSO flow
    let mut state = QsoSequenceState::Initial;

    for message in messages {
        match (&state, message) {
            (QsoSequenceState::Initial, MessageType::Cq { .. }) => {
                state = QsoSequenceState::CqSent;
            }

            (QsoSequenceState::CqSent, MessageType::CqResponse { .. }) => {
                state = QsoSequenceState::ResponseReceived;
            }

            (QsoSequenceState::ResponseReceived, MessageType::SignalReport { .. }) => {
                state = QsoSequenceState::ReportSent;
            }

            (QsoSequenceState::ReportSent, MessageType::ReportAck { .. }) => {
                state = QsoSequenceState::ReportAckReceived;
            }

            (QsoSequenceState::ReportAckReceived, MessageType::FinalConfirmation { .. }) => {
                state = QsoSequenceState::ConfirmationSent;
            }

            (QsoSequenceState::ConfirmationSent, MessageType::SeventyThree { .. }) => {
                state = QsoSequenceState::Complete;
            }

            _ => {
                return ValidationResult::Warning(format!(
                    "Unexpected message in sequence: {:?}",
                    message
                ));
            }
        }
    }

    match state {
        QsoSequenceState::Complete => ValidationResult::Valid,
        _ => ValidationResult::Warning("Incomplete QSO sequence".to_string()),
    }
}

/// QSO sequence state for validation
#[derive(Debug, Clone, PartialEq)]
enum QsoSequenceState {
    Initial,
    CqSent,
    ResponseReceived,
    ReportSent,
    ReportAckReceived,
    ConfirmationSent,
    Complete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cq_message() {
        let exchange = MessageExchange::new("W1ABC".to_string());

        let result = exchange.parse_message("CQ W1ABC FN42").unwrap();
        if let MessageType::Cq { callsign, grid } = result {
            assert_eq!(callsign, "W1ABC");
            assert_eq!(grid, Some("FN42".to_string()));
        } else {
            panic!("Expected CQ message");
        }
    }

    #[test]
    fn test_parse_signal_report() {
        let exchange = MessageExchange::new("W1ABC".to_string());

        let result = exchange.parse_message("K1DEF W1ABC -15").unwrap();
        if let MessageType::SignalReport {
            to_station,
            from_station,
            report,
        } = result
        {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(from_station, "W1ABC");
            assert_eq!(report, -15);
        } else {
            panic!("Expected signal report message");
        }
    }

    #[test]
    fn test_parse_rr73_is_final_confirmation_not_grid() {
        // Regression: "RR73" is a syntactically valid Maidenhead grid (RR + 73)
        // and also matches the CqResponse grid pattern. The parser must treat
        // it as the protocol close, not a grid — otherwise the DX's RR73 is
        // swallowed as a CqResponse and the QSO never completes (NP4VA stall).
        let exchange = MessageExchange::new("K5ARH".to_string());
        let result = exchange.parse_message("K5ARH NP4VA RR73").unwrap();
        match result {
            MessageType::FinalConfirmation {
                to_station,
                from_station,
            } => {
                assert_eq!(to_station, "K5ARH");
                assert_eq!(from_station, "NP4VA");
            }
            other => panic!("Expected FinalConfirmation for RR73, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_bare_73_is_seventy_three() {
        let exchange = MessageExchange::new("K5ARH".to_string());
        let result = exchange.parse_message("K5ARH NP4VA 73").unwrap();
        assert!(
            matches!(result, MessageType::SeventyThree { .. }),
            "Expected SeventyThree for bare 73, got {result:?}"
        );
    }

    #[test]
    fn test_parse_real_grid_still_cqresponse() {
        // A genuine grid in the third field must still parse as a CqResponse —
        // the RR73/73 close check must not over-capture ordinary grids.
        let exchange = MessageExchange::new("K5ARH".to_string());
        let result = exchange.parse_message("K5ARH NP4VA FN42").unwrap();
        match result {
            MessageType::CqResponse {
                calling_station,
                responding_station,
                grid,
            } => {
                assert_eq!(calling_station, "K5ARH");
                assert_eq!(responding_station, "NP4VA");
                assert_eq!(grid.as_deref(), Some("FN42"));
            }
            other => panic!("Expected CqResponse with grid, got {other:?}"),
        }
    }

    #[test]
    fn test_generate_cq_message() {
        let exchange = MessageExchange::new("W1ABC".to_string());

        let message = MessageType::Cq {
            callsign: "W1ABC".to_string(),
            grid: Some("FN42".to_string()),
        };

        let result = exchange.generate_message(&message).unwrap();
        assert_eq!(result, "CQ W1ABC FN42");
    }

    #[test]
    fn test_callsign_validation() {
        let exchange = MessageExchange::new("W1ABC".to_string());

        assert!(exchange.validate_callsign("W1ABC").is_ok());
        assert!(exchange.validate_callsign("K1DEF").is_ok());
        assert!(exchange.validate_callsign("VE3XYZ").is_ok());
        assert!(exchange.validate_callsign("G0ABC").is_ok());

        assert!(exchange.validate_callsign("ABC123").is_err());
        assert!(exchange.validate_callsign("1ABC").is_err());
        assert!(exchange.validate_callsign("").is_err());
    }

    #[test]
    fn test_signal_report_calculation() {
        let exchange = MessageExchange::new("W1ABC".to_string());

        let report = exchange.calculate_signal_report(-10.0, -25.0);
        assert_eq!(report, 15); // SNR of 15 dB

        let report = exchange.calculate_signal_report(-20.0, -25.0);
        assert_eq!(report, 6); // SNR of 5 dB, rounded to 6
    }

    // --- FIX 1: 4-char grid in standard messages -----------------------

    /// A 6-char configured grid (e.g. EM10ch) must produce a *standard*
    /// "DX K5ARH EM10" call/grid message — 4-char, uppercased — not a
    /// free-text-mangled bare callsign.
    #[test]
    fn cq_response_truncates_six_char_grid_to_four() {
        let exchange = MessageExchange::new("K5ARH".to_string());
        let msg = MessageType::CqResponse {
            calling_station: "PY2GIG".to_string(),
            responding_station: "K5ARH".to_string(),
            grid: Some("EM10ch".to_string()),
        };
        let text = exchange.generate_message(&msg).unwrap();
        assert_eq!(text, "PY2GIG K5ARH EM10");
        // Three fields, the third a 4-char Maidenhead grid → encodes as a
        // standard FT8 type-1 message (not free-text-mangled). Full standard
        // encodability is asserted end-to-end in the loopback integration test
        // (pancetta crate, which depends on the FT8 encoder).
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[2].len(), 4, "grid field must be 4-char: {:?}", text);
    }

    /// CQ with a 6-char grid is likewise truncated to 4 chars.
    #[test]
    fn cq_truncates_six_char_grid_to_four() {
        let exchange = MessageExchange::new("K5ARH".to_string());
        let msg = MessageType::Cq {
            callsign: "K5ARH".to_string(),
            grid: Some("EM10ch".to_string()),
        };
        assert_eq!(exchange.generate_message(&msg).unwrap(), "CQ K5ARH EM10");
    }

    /// An already-4-char grid is passed through unchanged (uppercased).
    #[test]
    fn four_char_grid_unchanged() {
        let exchange = MessageExchange::new("K5ARH".to_string());
        let msg = MessageType::CqResponse {
            calling_station: "PY2GIG".to_string(),
            responding_station: "K5ARH".to_string(),
            grid: Some("EM10".to_string()),
        };
        assert_eq!(
            exchange.generate_message(&msg).unwrap(),
            "PY2GIG K5ARH EM10"
        );
    }

    /// Empty/absent grid still produces a CQ/call without grid (today's
    /// behavior — must not regress).
    #[test]
    fn empty_or_absent_grid_omits_grid_field() {
        let exchange = MessageExchange::new("K5ARH".to_string());
        let none = MessageType::CqResponse {
            calling_station: "PY2GIG".to_string(),
            responding_station: "K5ARH".to_string(),
            grid: None,
        };
        assert_eq!(exchange.generate_message(&none).unwrap(), "PY2GIG K5ARH");
        let empty = MessageType::CqResponse {
            calling_station: "PY2GIG".to_string(),
            responding_station: "K5ARH".to_string(),
            grid: Some(String::new()),
        };
        assert_eq!(exchange.generate_message(&empty).unwrap(), "PY2GIG K5ARH");
    }
}
