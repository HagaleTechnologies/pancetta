//! Comprehensive error handling for hamlib operations
//!
//! This module provides structured error types, error recovery mechanisms,
//! and detailed error reporting for all hamlib operations.

use std::fmt;
use thiserror::Error;

/// Comprehensive error type for all hamlib operations
#[derive(Error, Debug, Clone)]
pub enum HamlibError {
    /// Connection-related errors
    #[error("Connection error: {message}")]
    Connection {
        /// Error message
        message: String,
        /// Optional error code from hamlib
        code: Option<i32>,
        /// Whether this error is recoverable
        recoverable: bool,
    },

    /// Communication errors with the rig
    #[error("Communication error: {message}")]
    Communication {
        /// Error message
        message: String,
        /// Optional error code from hamlib
        code: Option<i32>,
        /// Number of retries attempted
        retries: u32,
    },

    /// Invalid parameter errors
    #[error("Invalid parameter: {parameter} - {message}")]
    InvalidParameter {
        /// Parameter name
        parameter: String,
        /// Error message
        message: String,
        /// Suggested value or range
        suggestion: Option<String>,
    },

    /// Feature not supported by rig
    #[error("Feature not supported: {feature}")]
    NotSupported {
        /// Feature name
        feature: String,
        /// Rig model that doesn't support this feature
        model: Option<String>,
        /// Alternative feature suggestion
        alternative: Option<String>,
    },

    /// Operation timeout
    #[error("Operation timed out after {timeout_ms}ms: {operation}")]
    Timeout {
        /// Operation that timed out
        operation: String,
        /// Timeout duration in milliseconds
        timeout_ms: u32,
        /// Whether retry is recommended
        retry_recommended: bool,
    },

    /// Hardware-related errors
    #[error("Hardware error: {message}")]
    Hardware {
        /// Error message
        message: String,
        /// Hardware component involved
        component: Option<String>,
        /// Whether hardware intervention is required
        intervention_required: bool,
    },

    /// Configuration errors
    #[error("Configuration error: {message}")]
    Configuration {
        /// Error message
        message: String,
        /// Configuration parameter that's problematic
        parameter: Option<String>,
        /// Expected value or format
        expected: Option<String>,
    },

    /// Internal library errors
    #[error("Internal error: {message}")]
    Internal {
        /// Error message
        message: String,
        /// Source error message (converted from Box for Clone)
        source_message: Option<String>,
        /// Debug information
        debug_info: Option<String>,
    },

    /// Frequency-related errors
    #[error("Frequency error: {message}")]
    Frequency {
        /// Error message
        message: String,
        /// Requested frequency
        requested: Option<u64>,
        /// Valid frequency range
        valid_range: Option<(u64, u64)>,
    },

    /// Mode-related errors
    #[error("Mode error: {message}")]
    Mode {
        /// Error message
        message: String,
        /// Requested mode
        requested: Option<String>,
        /// Supported modes
        supported: Option<Vec<String>>,
    },

    /// Memory channel errors
    #[error("Memory channel error: {message}")]
    Memory {
        /// Error message
        message: String,
        /// Channel number
        channel: Option<i32>,
        /// Valid channel range
        valid_range: Option<(i32, i32)>,
    },

    /// PTT (Push-to-Talk) errors
    #[error("PTT error: {message}")]
    Ptt {
        /// Error message
        message: String,
        /// Current PTT state
        current_state: Option<String>,
        /// Whether rig is safe to operate
        safe_state: bool,
    },

    /// Scanning operation errors
    #[error("Scan error: {message}")]
    Scan {
        /// Error message
        message: String,
        /// Scan operation type
        operation: Option<String>,
        /// Whether scan can be resumed
        resumable: bool,
    },

    /// Monitoring errors
    #[error("Monitoring error: {message}")]
    Monitoring {
        /// Error message
        message: String,
        /// Monitoring parameter
        parameter: Option<String>,
        /// Whether monitoring can continue
        recoverable: bool,
    },
}

impl HamlibError {
    /// Create connection error
    pub fn connection<S: Into<String>>(message: S, code: Option<i32>, recoverable: bool) -> Self {
        Self::Connection {
            message: message.into(),
            code,
            recoverable,
        }
    }

    /// Create communication error
    pub fn communication<S: Into<String>>(message: S, code: Option<i32>, retries: u32) -> Self {
        Self::Communication {
            message: message.into(),
            code,
            retries,
        }
    }

    /// Create invalid parameter error
    pub fn invalid_parameter<S: Into<String>>(
        parameter: S,
        message: S,
        suggestion: Option<String>,
    ) -> Self {
        Self::InvalidParameter {
            parameter: parameter.into(),
            message: message.into(),
            suggestion,
        }
    }

    /// Create not supported error
    pub fn not_supported<S: Into<String>>(
        feature: S,
        model: Option<String>,
        alternative: Option<String>,
    ) -> Self {
        Self::NotSupported {
            feature: feature.into(),
            model,
            alternative,
        }
    }

    /// Create timeout error
    pub fn timeout<S: Into<String>>(
        operation: S,
        timeout_ms: u32,
        retry_recommended: bool,
    ) -> Self {
        Self::Timeout {
            operation: operation.into(),
            timeout_ms,
            retry_recommended,
        }
    }

    /// Create hardware error
    pub fn hardware<S: Into<String>>(
        message: S,
        component: Option<String>,
        intervention_required: bool,
    ) -> Self {
        Self::Hardware {
            message: message.into(),
            component,
            intervention_required,
        }
    }

    /// Create configuration error
    pub fn configuration<S: Into<String>>(
        message: S,
        parameter: Option<String>,
        expected: Option<String>,
    ) -> Self {
        Self::Configuration {
            message: message.into(),
            parameter,
            expected,
        }
    }

    /// Create internal error
    pub fn internal<S: Into<String>>(
        message: S,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
        debug_info: Option<String>,
    ) -> Self {
        Self::Internal {
            message: message.into(),
            source_message: source.map(|e| e.to_string()),
            debug_info,
        }
    }

    /// Create frequency error
    pub fn frequency<S: Into<String>>(
        message: S,
        requested: Option<u64>,
        valid_range: Option<(u64, u64)>,
    ) -> Self {
        Self::Frequency {
            message: message.into(),
            requested,
            valid_range,
        }
    }

    /// Create mode error
    pub fn mode<S: Into<String>>(
        message: S,
        requested: Option<String>,
        supported: Option<Vec<String>>,
    ) -> Self {
        Self::Mode {
            message: message.into(),
            requested,
            supported,
        }
    }

    /// Create memory error
    pub fn memory<S: Into<String>>(
        message: S,
        channel: Option<i32>,
        valid_range: Option<(i32, i32)>,
    ) -> Self {
        Self::Memory {
            message: message.into(),
            channel,
            valid_range,
        }
    }

    /// Create PTT error
    pub fn ptt<S: Into<String>>(
        message: S,
        current_state: Option<String>,
        safe_state: bool,
    ) -> Self {
        Self::Ptt {
            message: message.into(),
            current_state,
            safe_state,
        }
    }

    /// Create scan error
    pub fn scan<S: Into<String>>(message: S, operation: Option<String>, resumable: bool) -> Self {
        Self::Scan {
            message: message.into(),
            operation,
            resumable,
        }
    }

    /// Create monitoring error
    pub fn monitoring<S: Into<String>>(
        message: S,
        parameter: Option<String>,
        recoverable: bool,
    ) -> Self {
        Self::Monitoring {
            message: message.into(),
            parameter,
            recoverable,
        }
    }

    /// Check if error is recoverable
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Connection { recoverable, .. } => *recoverable,
            Self::Communication { .. } => true, // Usually recoverable with retry
            Self::InvalidParameter { .. } => false, // Need parameter fix
            Self::NotSupported { .. } => false, // Feature not available
            Self::Timeout {
                retry_recommended, ..
            } => *retry_recommended,
            Self::Hardware {
                intervention_required,
                ..
            } => !intervention_required,
            Self::Configuration { .. } => false, // Need configuration fix
            Self::Internal { .. } => false,      // Internal errors usually not recoverable
            Self::Frequency { .. } => false,     // Need frequency adjustment
            Self::Mode { .. } => false,          // Need mode adjustment
            Self::Memory { .. } => false,        // Need channel adjustment
            Self::Ptt { safe_state, .. } => *safe_state,
            Self::Scan { resumable, .. } => *resumable,
            Self::Monitoring { recoverable, .. } => *recoverable,
        }
    }

    /// Get error severity level
    pub fn severity(&self) -> ErrorSeverity {
        match self {
            Self::Connection { recoverable, .. } => {
                if *recoverable {
                    ErrorSeverity::Warning
                } else {
                    ErrorSeverity::Error
                }
            }
            Self::Communication { .. } => ErrorSeverity::Warning,
            Self::InvalidParameter { .. } => ErrorSeverity::Error,
            Self::NotSupported { .. } => ErrorSeverity::Info,
            Self::Timeout { .. } => ErrorSeverity::Warning,
            Self::Hardware {
                intervention_required,
                ..
            } => {
                if *intervention_required {
                    ErrorSeverity::Critical
                } else {
                    ErrorSeverity::Error
                }
            }
            Self::Configuration { .. } => ErrorSeverity::Error,
            Self::Internal { .. } => ErrorSeverity::Critical,
            Self::Frequency { .. } => ErrorSeverity::Error,
            Self::Mode { .. } => ErrorSeverity::Error,
            Self::Memory { .. } => ErrorSeverity::Error,
            Self::Ptt { safe_state, .. } => {
                if *safe_state {
                    ErrorSeverity::Warning
                } else {
                    ErrorSeverity::Critical
                }
            }
            Self::Scan { .. } => ErrorSeverity::Warning,
            Self::Monitoring { .. } => ErrorSeverity::Warning,
        }
    }

    /// Get user-friendly error message
    pub fn user_message(&self) -> String {
        match self {
            Self::Connection {
                message,
                recoverable,
                ..
            } => {
                if *recoverable {
                    format!("Connection issue: {}. Will retry automatically.", message)
                } else {
                    format!(
                        "Connection failed: {}. Please check cable and settings.",
                        message
                    )
                }
            }
            Self::Communication {
                message, retries, ..
            } => {
                format!(
                    "Communication problem: {}. Tried {} times.",
                    message,
                    retries + 1
                )
            }
            Self::InvalidParameter {
                parameter,
                suggestion,
                ..
            } => {
                if let Some(suggestion) = suggestion {
                    format!("Invalid {}: {}. Try: {}", parameter, self, suggestion)
                } else {
                    format!("Invalid {}: {}", parameter, self)
                }
            }
            Self::NotSupported {
                feature,
                alternative,
                ..
            } => {
                if let Some(alt) = alternative {
                    format!("Feature '{}' not supported. Try: {}", feature, alt)
                } else {
                    format!("Feature '{}' not supported by this rig.", feature)
                }
            }
            Self::Timeout {
                operation,
                timeout_ms,
                ..
            } => {
                format!(
                    "Operation '{}' timed out after {}ms. Rig may be unresponsive.",
                    operation, timeout_ms
                )
            }
            Self::Hardware {
                message,
                intervention_required,
                ..
            } => {
                if *intervention_required {
                    format!(
                        "Hardware problem: {}. Please check rig and connections.",
                        message
                    )
                } else {
                    format!("Hardware issue: {}. May resolve automatically.", message)
                }
            }
            Self::Frequency {
                requested,
                valid_range,
                ..
            } => {
                if let (Some(req), Some((min, max))) = (requested, valid_range) {
                    format!(
                        "Frequency {:.3} MHz is outside valid range ({:.3}-{:.3} MHz)",
                        *req as f64 / 1_000_000.0,
                        *min as f64 / 1_000_000.0,
                        *max as f64 / 1_000_000.0
                    )
                } else {
                    format!("Invalid frequency: {}", self)
                }
            }
            Self::Mode {
                requested,
                supported,
                ..
            } => {
                if let (Some(req), Some(sup)) = (requested, supported) {
                    format!(
                        "Mode '{}' not supported. Available: {}",
                        req,
                        sup.join(", ")
                    )
                } else {
                    format!("Invalid mode: {}", self)
                }
            }
            Self::Memory {
                channel,
                valid_range,
                ..
            } => {
                if let (Some(ch), Some((min, max))) = (channel, valid_range) {
                    format!(
                        "Memory channel {} is outside valid range ({}-{})",
                        ch, min, max
                    )
                } else {
                    format!("Memory channel error: {}", self)
                }
            }
            _ => self.to_string(),
        }
    }

    // `from_hamlib_code` was previously defined here as the conversion path
    // from libhamlib FFI return codes into HamlibError variants. It was
    // removed when the dead FFI bindings layer was deleted; the rigctld
    // TCP path produces structured errors directly without going through
    // RIG_E* numeric codes. If a future native-hamlib path is reintroduced,
    // the conversion can be reimplemented against whatever new bindings
    // surface ships with it.

    /// Get recovery suggestions
    pub fn recovery_suggestions(&self) -> Vec<String> {
        let mut suggestions = Vec::new();

        match self {
            Self::Connection { recoverable, .. } => {
                if *recoverable {
                    suggestions.push("Wait for automatic reconnection".to_string());
                    suggestions.push("Check USB/serial connections".to_string());
                } else {
                    suggestions.push("Verify device path and permissions".to_string());
                    suggestions.push("Check baud rate settings".to_string());
                    suggestions.push("Ensure rig is powered on".to_string());
                }
            }
            Self::Communication { .. } => {
                suggestions.push("Check cable connections".to_string());
                suggestions.push("Verify baud rate matches rig setting".to_string());
                suggestions.push("Try a different USB port".to_string());
                suggestions.push("Restart the rig".to_string());
            }
            Self::Timeout { .. } => {
                suggestions.push("Increase timeout value".to_string());
                suggestions.push("Check rig responsiveness".to_string());
                suggestions.push("Reduce operation frequency".to_string());
            }
            Self::Hardware {
                intervention_required,
                ..
            } => {
                if *intervention_required {
                    suggestions.push("Check rig power and connections".to_string());
                    suggestions.push("Verify antenna connections".to_string());
                    suggestions.push("Check for hardware faults".to_string());
                } else {
                    suggestions.push("Wait for rig to stabilize".to_string());
                    suggestions.push("Try operation again in a moment".to_string());
                }
            }
            Self::Frequency { valid_range, .. } => {
                if let Some((min, max)) = valid_range {
                    suggestions.push(format!(
                        "Use frequency between {:.3} and {:.3} MHz",
                        *min as f64 / 1_000_000.0,
                        *max as f64 / 1_000_000.0
                    ));
                }
                suggestions.push("Check amateur radio band plan".to_string());
            }
            Self::Mode { supported, .. } => {
                if let Some(modes) = supported {
                    suggestions.push(format!("Use one of: {}", modes.join(", ")));
                }
                suggestions.push("Check rig mode capabilities".to_string());
            }
            Self::Memory { valid_range, .. } => {
                if let Some((min, max)) = valid_range {
                    suggestions.push(format!("Use memory channel between {} and {}", min, max));
                }
                suggestions.push("Program memory channel first".to_string());
            }
            Self::Ptt { safe_state, .. } => {
                if !safe_state {
                    suggestions.push("Ensure PTT is OFF before adjusting settings".to_string());
                    suggestions.push("Check for stuck PTT".to_string());
                }
            }
            _ => {
                suggestions.push("Check rig documentation".to_string());
                suggestions.push("Consult hamlib compatibility notes".to_string());
            }
        }

        suggestions
    }
}

/// Error severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorSeverity {
    /// Informational - no action required
    Info,
    /// Warning - operation may continue
    Warning,
    /// Error - operation failed but recoverable
    Error,
    /// Critical - immediate attention required
    Critical,
}

impl fmt::Display for ErrorSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorSeverity::Info => write!(f, "INFO"),
            ErrorSeverity::Warning => write!(f, "WARN"),
            ErrorSeverity::Error => write!(f, "ERROR"),
            ErrorSeverity::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Error context for detailed error reporting
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Operation being performed
    pub operation: String,
    /// Rig model involved
    pub rig_model: Option<String>,
    /// Frequency involved (if applicable)
    pub frequency: Option<u64>,
    /// VFO involved (if applicable)
    pub vfo: Option<String>,
    /// Additional context information
    pub context: std::collections::HashMap<String, String>,
    /// Timestamp of error
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl ErrorContext {
    /// Create new error context
    pub fn new<S: Into<String>>(operation: S) -> Self {
        Self {
            operation: operation.into(),
            rig_model: None,
            frequency: None,
            vfo: None,
            context: std::collections::HashMap::new(),
            timestamp: chrono::Utc::now(),
        }
    }

    /// Add rig model to context
    pub fn with_rig_model<S: Into<String>>(mut self, model: S) -> Self {
        self.rig_model = Some(model.into());
        self
    }

    /// Add frequency to context
    pub fn with_frequency(mut self, frequency: u64) -> Self {
        self.frequency = Some(frequency);
        self
    }

    /// Add VFO to context
    pub fn with_vfo<S: Into<String>>(mut self, vfo: S) -> Self {
        self.vfo = Some(vfo.into());
        self
    }

    /// Add custom context information
    pub fn with_context<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }
}

/// Enhanced error type with context
#[derive(Debug, Clone)]
pub struct ContextualError {
    /// The underlying error
    pub error: HamlibError,
    /// Error context
    pub context: ErrorContext,
}

impl ContextualError {
    /// Create new contextual error
    pub fn new(error: HamlibError, context: ErrorContext) -> Self {
        Self { error, context }
    }

    /// Get formatted error report
    pub fn report(&self) -> String {
        let mut report = String::new();

        report.push_str(&format!(
            "[{}] {}\n",
            self.error.severity(),
            self.error.user_message()
        ));
        report.push_str(&format!("Operation: {}\n", self.context.operation));
        report.push_str(&format!(
            "Time: {}\n",
            self.context.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
        ));

        if let Some(model) = &self.context.rig_model {
            report.push_str(&format!("Rig: {}\n", model));
        }

        if let Some(freq) = self.context.frequency {
            report.push_str(&format!(
                "Frequency: {:.3} MHz\n",
                freq as f64 / 1_000_000.0
            ));
        }

        if let Some(vfo) = &self.context.vfo {
            report.push_str(&format!("VFO: {}\n", vfo));
        }

        if !self.context.context.is_empty() {
            report.push_str("Additional Context:\n");
            for (key, value) in &self.context.context {
                report.push_str(&format!("  {}: {}\n", key, value));
            }
        }

        let suggestions = self.error.recovery_suggestions();
        if !suggestions.is_empty() {
            report.push_str("Recovery Suggestions:\n");
            for suggestion in suggestions {
                report.push_str(&format!("  • {}\n", suggestion));
            }
        }

        report
    }
}

impl fmt::Display for ContextualError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for ContextualError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Result type with contextual errors
pub type ContextualResult<T> = Result<T, ContextualError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_creation() {
        let err = HamlibError::connection("Test connection error", None, true);
        assert!(err.is_recoverable());
        assert_eq!(err.severity(), ErrorSeverity::Warning);
    }

    #[test]
    fn test_frequency_error() {
        let err = HamlibError::frequency(
            "Invalid frequency",
            Some(100_000_000),
            Some((1_800_000, 30_000_000)),
        );

        assert!(!err.is_recoverable());
        assert_eq!(err.severity(), ErrorSeverity::Error);

        let message = err.user_message();
        assert!(message.contains("100.000 MHz"));
        assert!(message.contains("1.800-30.000 MHz"));
    }

    #[test]
    fn test_error_context() {
        let context = ErrorContext::new("set_frequency")
            .with_rig_model("IC-7300")
            .with_frequency(14_200_000)
            .with_vfo("A")
            .with_context("retry_count", "3");

        assert_eq!(context.operation, "set_frequency");
        assert_eq!(context.rig_model, Some("IC-7300".to_string()));
        assert_eq!(context.frequency, Some(14_200_000));
        assert_eq!(context.vfo, Some("A".to_string()));
        assert_eq!(context.context.get("retry_count"), Some(&"3".to_string()));
    }

    #[test]
    fn test_contextual_error() {
        let error = HamlibError::timeout("get_frequency", 2000, true);
        let context = ErrorContext::new("get_frequency").with_rig_model("FT-991A");

        let contextual_error = ContextualError::new(error, context);
        let report = contextual_error.report();

        assert!(report.contains("WARN"));
        assert!(report.contains("get_frequency"));
        assert!(report.contains("FT-991A"));
        assert!(report.contains("Recovery Suggestions"));
    }

    #[test]
    fn test_recovery_suggestions() {
        let error = HamlibError::communication("Port error", None, 2);
        let suggestions = error.recovery_suggestions();

        assert!(!suggestions.is_empty());
        assert!(suggestions.iter().any(|s| s.contains("cable")));
        assert!(suggestions.iter().any(|s| s.contains("baud rate")));
    }

    // test_hamlib_error_conversion removed — exercised the deleted
    // from_hamlib_code conversion path.

    #[test]
    fn test_mode_error() {
        let error = HamlibError::mode(
            "Unsupported mode",
            Some("INVALID".to_string()),
            Some(vec!["USB".to_string(), "LSB".to_string(), "CW".to_string()]),
        );

        let message = error.user_message();
        assert!(message.contains("INVALID"));
        assert!(message.contains("USB, LSB, CW"));
    }
}
