//! Message exchange logic for FT8 QSO operations
//! 
//! This module handles the parsing and generation of FT8 messages
//! according to the standard protocol and contest variations.

use crate::states::*;
use regex::Regex;
use lazy_static::lazy_static;
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
}

/// FT8 message exchange handler
pub struct MessageExchange {
    /// Our callsign for message validation
    our_callsign: String,
    
    /// Contest mode configuration
    contest_mode: Option<ContestExchangeConfig>,
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
        r"^[A-Z0-9]{1,3}[0-9][A-Z0-9]{0,3}[A-Z]$|^[A-Z0-9]{1,2}[0-9][A-Z0-9]{0,4}$"
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
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+R?([+-]?\d{1,2})$").unwrap(),
        // Final confirmation: "W1ABC K1DEF RR73" or "W1ABC K1DEF 73"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(RR73|73)$").unwrap(),
        // Contest exchange: "W1ABC K1DEF 599 001"
        Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(\d{3})\s+(\d{3,4})$").unwrap(),
    ];
}

impl MessageExchange {
    /// Create a new message exchange handler
    pub fn new(our_callsign: String) -> Self {
        Self {
            our_callsign,
            contest_mode: None,
        }
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
            MessageType::Cq { callsign, grid } => {
                if let Some(grid) = grid {
                    Ok(format!("CQ {} {}", callsign, grid))
                } else {
                    Ok(format!("CQ {}", callsign))
                }
            }
            
            MessageType::CqResponse { calling_station, responding_station, grid } => {
                if let Some(grid) = grid {
                    Ok(format!("{} {} {}", calling_station, responding_station, grid))
                } else {
                    Ok(format!("{} {}", calling_station, responding_station))
                }
            }
            
            MessageType::SignalReport { to_station, from_station, report } => {
                Ok(format!("{} {} {:+}", to_station, from_station, report))
            }
            
            MessageType::ReportAck { to_station, from_station, report } => {
                Ok(format!("{} {} R{:+}", to_station, from_station, report))
            }
            
            MessageType::FinalConfirmation { to_station, from_station } => {
                Ok(format!("{} {} RR73", to_station, from_station))
            }
            
            MessageType::SeventyThree { to_station, from_station } => {
                Ok(format!("{} {} 73", to_station, from_station))
            }
            
            MessageType::ContestExchange { to_station, from_station, report, serial } => {
                Ok(format!("{} {} {:03} {:03}", to_station, from_station, 
                          (report + 35) as u32, serial))
            }
            
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
        if report < -50 || report > 50 {
            return Err(ExchangeError::InvalidReport {
                report: report.to_string(),
            });
        }
        Ok(())
    }
    
    /// Generate appropriate response message based on QSO state
    pub fn generate_response(
        &self,
        current_state: &QsoState,
        received_message: &MessageType,
    ) -> Result<Option<MessageType>, ExchangeError> {
        match (current_state, received_message) {
            // Received response to our CQ
            (QsoState::CallingCq { .. }, 
             MessageType::CqResponse { responding_station, .. }) => {
                Ok(Some(MessageType::SignalReport {
                    to_station: responding_station.clone(),
                    from_station: self.our_callsign.clone(),
                    report: -15, // Default report, should be calculated from signal strength
                }))
            }
            
            // Received signal report, send acknowledgment
            (QsoState::RespondingToCq { target_callsign, .. }, 
             MessageType::SignalReport { report, .. }) => {
                Ok(Some(MessageType::ReportAck {
                    to_station: target_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                    report: -12, // Our report
                }))
            }
            
            // Received report acknowledgment, send final confirmation
            (QsoState::SendingReport { their_callsign, .. }, 
             MessageType::ReportAck { .. }) => {
                Ok(Some(MessageType::FinalConfirmation {
                    to_station: their_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                }))
            }
            
            // Received final confirmation, send 73
            (QsoState::WaitingForConfirmation { their_callsign, .. }, 
             MessageType::FinalConfirmation { .. }) => {
                Ok(Some(MessageType::SeventyThree {
                    to_station: their_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                }))
            }
            
            // Contest mode responses
            _ if self.contest_mode.is_some() => {
                self.generate_contest_response(current_state, received_message)
            }
            
            // No response needed
            _ => Ok(None),
        }
    }
    
    /// Calculate signal report from signal strength
    pub fn calculate_signal_report(&self, signal_strength: f32, noise_floor: f32) -> SignalReport {
        let snr = signal_strength - noise_floor;
        
        // Convert SNR to FT8 report scale
        let report = (snr.round() as i8).max(-30).min(50);
        
        // Round to nearest 3 dB for FT8 convention
        ((report + 1) / 3) * 3
    }
    
    /// Extract frequency information from message
    pub fn extract_frequency_info(&self, message: &str) -> Option<f64> {
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
            std::mem::discriminant(&prev.message_type) == std::mem::discriminant(message) &&
            prev.message_type == *message
        })
    }
    
    // Private helper methods
    
    fn parse_cq_message(&self, message: &str) -> Result<MessageType, ExchangeError> {
        for pattern in CQ_PATTERNS.iter() {
            if let Some(captures) = pattern.captures(message) {
                let callsign = captures.get(1)
                    .ok_or_else(|| ExchangeError::ParseError {
                        details: "Missing callsign in CQ".to_string(),
                    })?
                    .as_str()
                    .to_string();
                
                self.validate_callsign(&callsign)?;
                
                let grid = if captures.len() > 2 {
                    captures.get(captures.len() - 1)
                        .and_then(|m| {
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
        let station1 = captures.get(1)
            .ok_or_else(|| ExchangeError::ParseError {
                details: "Missing first callsign".to_string(),
            })?
            .as_str()
            .to_string();
        
        let station2 = captures.get(2)
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
            let third_field = captures.get(3).unwrap().as_str();
            
            // Check if it's a grid square
            if GRID_REGEX.is_match(third_field) {
                Ok(MessageType::CqResponse {
                    calling_station: station1,
                    responding_station: station2,
                    grid: Some(third_field.to_string()),
                })
            }
            // Check if it's a signal report
            else if let Ok(report) = third_field.trim_start_matches('R').parse::<i8>() {
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
            }
            // Check if it's RR73 or 73
            else if third_field == "RR73" {
                Ok(MessageType::FinalConfirmation {
                    to_station: station1,
                    from_station: station2,
                })
            } else if third_field == "73" {
                Ok(MessageType::SeventyThree {
                    to_station: station1,
                    from_station: station2,
                })
            } else {
                Err(ExchangeError::InvalidFormat {
                    message: message.to_string(),
                })
            }
        } else if captures.len() == 5 {
            // Contest exchange format
            let report_str = captures.get(3).unwrap().as_str();
            let serial_str = captures.get(4).unwrap().as_str();
            
            let report = (report_str.parse::<u32>()
                .map_err(|_| ExchangeError::InvalidReport {
                    report: report_str.to_string(),
                })? as i8) - 35; // Convert from 599 format to dB
            
            let serial = serial_str.parse::<SerialNumber>()
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
    ) -> Result<Option<MessageType>, ExchangeError> {
        let contest_config = self.contest_mode.as_ref().unwrap();
        
        match (current_state, received_message) {
            // Received contest exchange, send our exchange
            (QsoState::RespondingToCq { target_callsign, .. },
             MessageType::ContestExchange { serial, .. }) => {
                Ok(Some(MessageType::ContestExchange {
                    to_station: target_callsign.clone(),
                    from_station: self.our_callsign.clone(),
                    report: -15, // Default report
                    serial: 1, // Should be generated by contest manager
                }))
            }
            
            _ => Ok(None),
        }
    }
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
                return ValidationResult::Warning(
                    format!("Unexpected message in sequence: {:?}", message)
                );
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
        if let MessageType::SignalReport { to_station, from_station, report } = result {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(from_station, "W1ABC");
            assert_eq!(report, -15);
        } else {
            panic!("Expected signal report message");
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
        assert_eq!(report, -6); // SNR of -5 dB, rounded to -6
    }
}