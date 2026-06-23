//! Error types for the audio processing system
//!
//! Provides comprehensive error handling for all audio operations
//! with proper error categorization for debugging and recovery.

use thiserror::Error;

/// Audio system errors
#[derive(Error, Debug)]
pub enum AudioError {
    /// Device-related errors
    #[error("Device error: {message}")]
    Device { message: String },

    /// Stream-related errors  
    #[error("Stream error: {message}")]
    Stream { message: String },

    /// Configuration errors
    #[error("Configuration error: {message}")]
    Configuration { message: String },

    /// Sample rate conversion errors
    #[error("Sample rate conversion error: {message}")]
    SampleRate { message: String },

    /// Buffer overflow/underflow
    #[error("Buffer error: {message}")]
    Buffer { message: String },

    /// Latency target exceeded
    #[error("Latency exceeded: {actual_ms}ms > {target_ms}ms")]
    LatencyExceeded { actual_ms: f64, target_ms: f64 },

    /// Threading/synchronization errors
    #[error("Threading error: {message}")]
    Threading { message: String },

    /// CPAL stream errors (manually converted)
    #[error("CPAL stream error: {message}")]
    CpalStream { message: String },

    /// Device not found by name pattern
    #[error("Audio device not found matching pattern: {pattern}")]
    DeviceNotFound { pattern: String },

    /// General audio system failure
    #[error("Audio system failure: {message}")]
    System { message: String },
}

impl AudioError {
    /// Create a device error
    pub fn device(message: impl Into<String>) -> Self {
        Self::Device {
            message: message.into(),
        }
    }

    /// Create a stream error
    pub fn stream(message: impl Into<String>) -> Self {
        Self::Stream {
            message: message.into(),
        }
    }

    /// Create a configuration error
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    /// Create a sample rate error
    pub fn sample_rate(message: impl Into<String>) -> Self {
        Self::SampleRate {
            message: message.into(),
        }
    }

    /// Create a buffer error
    pub fn buffer(message: impl Into<String>) -> Self {
        Self::Buffer {
            message: message.into(),
        }
    }

    /// Create a threading error
    pub fn threading(message: impl Into<String>) -> Self {
        Self::Threading {
            message: message.into(),
        }
    }

    /// Create a device-not-found error
    pub fn device_not_found(pattern: impl Into<String>) -> Self {
        Self::DeviceNotFound {
            pattern: pattern.into(),
        }
    }

    /// Create a system error
    pub fn system(message: impl Into<String>) -> Self {
        Self::System {
            message: message.into(),
        }
    }

    /// Check if this is a recoverable error
    pub fn is_recoverable(&self) -> bool {
        match self {
            AudioError::Device { .. } => false, // Device issues usually require restart
            AudioError::DeviceNotFound { .. } => false, // Need correct device name
            AudioError::Stream { .. } => true,  // Streams can be recreated
            AudioError::Configuration { .. } => false, // Config errors need fixing
            AudioError::SampleRate { .. } => true, // Can fallback to different rates
            AudioError::Buffer { .. } => true,  // Buffer issues can be handled
            AudioError::LatencyExceeded { .. } => true, // Can adjust buffer sizes
            AudioError::Threading { .. } => false, // Threading issues are serious
            AudioError::CpalStream { .. } => true, // CPAL errors might be transient
            AudioError::System { .. } => false, // System failures are usually fatal
        }
    }

    /// Get error severity level
    pub fn severity(&self) -> ErrorSeverity {
        match self {
            AudioError::Device { .. } => ErrorSeverity::Critical,
            AudioError::DeviceNotFound { .. } => ErrorSeverity::Error,
            AudioError::Stream { .. } => ErrorSeverity::Warning,
            AudioError::Configuration { .. } => ErrorSeverity::Error,
            AudioError::SampleRate { .. } => ErrorSeverity::Warning,
            AudioError::Buffer { .. } => ErrorSeverity::Warning,
            AudioError::LatencyExceeded { .. } => ErrorSeverity::Warning,
            AudioError::Threading { .. } => ErrorSeverity::Critical,
            AudioError::CpalStream { .. } => ErrorSeverity::Error,
            AudioError::System { .. } => ErrorSeverity::Critical,
        }
    }
}

/// Error severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    /// Informational - not actually an error
    Info,
    /// Warning - system can continue but with degraded performance
    Warning,
    /// Error - operation failed but system can recover
    Error,
    /// Critical - system cannot continue and requires intervention
    Critical,
}

impl std::fmt::Display for ErrorSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorSeverity::Info => write!(f, "INFO"),
            ErrorSeverity::Warning => write!(f, "WARN"),
            ErrorSeverity::Error => write!(f, "ERROR"),
            ErrorSeverity::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Audio processing result type
pub type AudioResult<T> = Result<T, AudioError>;

/// Convert CPAL device name errors to AudioError
impl From<cpal::DeviceNameError> for AudioError {
    fn from(err: cpal::DeviceNameError) -> Self {
        AudioError::device(format!("Device name error: {}", err))
    }
}

/// Convert CPAL supported configs errors to AudioError
impl From<cpal::SupportedStreamConfigsError> for AudioError {
    fn from(err: cpal::SupportedStreamConfigsError) -> Self {
        AudioError::device(format!("Supported configs error: {}", err))
    }
}

/// Convert CPAL build stream errors to AudioError
impl From<cpal::BuildStreamError> for AudioError {
    fn from(err: cpal::BuildStreamError) -> Self {
        AudioError::stream(format!("Build stream error: {}", err))
    }
}

/// Convert CPAL stream errors to AudioError
impl From<cpal::StreamError> for AudioError {
    fn from(err: cpal::StreamError) -> Self {
        AudioError::CpalStream {
            message: format!("{}", err),
        }
    }
}

/// Convert CPAL play stream errors to AudioError
impl From<cpal::PlayStreamError> for AudioError {
    fn from(err: cpal::PlayStreamError) -> Self {
        AudioError::stream(format!("Play stream error: {}", err))
    }
}

/// Convert CPAL pause stream errors to AudioError
impl From<cpal::PauseStreamError> for AudioError {
    fn from(err: cpal::PauseStreamError) -> Self {
        AudioError::stream(format!("Pause stream error: {}", err))
    }
}
