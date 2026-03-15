//! Unified error handling for all Pancetta modules

use std::error::Error;
use std::fmt;
use std::io;

/// Unified error type for all Pancetta operations
#[derive(Debug)]
pub enum PancettaError {
    /// IO error
    Io(io::Error),

    /// Serialization error
    Serialization(serde_json::Error),

    /// Database error
    Database(String),

    /// Network error
    Network(String),

    /// Websocket error
    WebSocket(String),

    /// Audio processing error
    Audio(String),

    /// FT8 codec error
    Ft8(String),

    /// Hamlib error
    Hamlib(String),

    /// Configuration error
    Configuration(String),

    /// Invalid input/parameter
    InvalidInput(String),

    /// Operation not supported
    NotSupported(String),

    /// Timeout occurred
    Timeout(String),

    /// Parse error
    Parse(String),

    /// Generic error with message
    Other(String),
}

impl fmt::Display for PancettaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PancettaError::Io(e) => write!(f, "IO error: {}", e),
            PancettaError::Serialization(e) => write!(f, "Serialization error: {}", e),
            PancettaError::Database(msg) => write!(f, "Database error: {}", msg),
            PancettaError::Network(msg) => write!(f, "Network error: {}", msg),
            PancettaError::WebSocket(msg) => write!(f, "WebSocket error: {}", msg),
            PancettaError::Audio(msg) => write!(f, "Audio error: {}", msg),
            PancettaError::Ft8(msg) => write!(f, "FT8 error: {}", msg),
            PancettaError::Hamlib(msg) => write!(f, "Hamlib error: {}", msg),
            PancettaError::Configuration(msg) => write!(f, "Configuration error: {}", msg),
            PancettaError::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            PancettaError::NotSupported(msg) => write!(f, "Not supported: {}", msg),
            PancettaError::Timeout(msg) => write!(f, "Timeout: {}", msg),
            PancettaError::Parse(msg) => write!(f, "Parse error: {}", msg),
            PancettaError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

impl Error for PancettaError {}

// Conversion implementations for common error types
impl From<io::Error> for PancettaError {
    fn from(err: io::Error) -> Self {
        PancettaError::Io(err)
    }
}

impl From<serde_json::Error> for PancettaError {
    fn from(err: serde_json::Error) -> Self {
        PancettaError::Serialization(err)
    }
}

impl From<std::num::ParseIntError> for PancettaError {
    fn from(err: std::num::ParseIntError) -> Self {
        PancettaError::Parse(err.to_string())
    }
}

impl From<std::num::ParseFloatError> for PancettaError {
    fn from(err: std::num::ParseFloatError) -> Self {
        PancettaError::Parse(err.to_string())
    }
}

impl From<std::string::FromUtf8Error> for PancettaError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        PancettaError::Parse(err.to_string())
    }
}

impl From<std::str::Utf8Error> for PancettaError {
    fn from(err: std::str::Utf8Error) -> Self {
        PancettaError::Parse(err.to_string())
    }
}

// Convenience conversions for module-specific errors
// These are commented out until the modules are available
// #[cfg(feature = "hamlib")]
// impl From<crate::hamlib::HamlibError> for PancettaError {
//     fn from(err: crate::hamlib::HamlibError) -> Self {
//         PancettaError::Hamlib(err.to_string())
//     }
// }

// #[cfg(feature = "ft8")]
// impl From<crate::ft8::Ft8Error> for PancettaError {
//     fn from(err: crate::ft8::Ft8Error) -> Self {
//         PancettaError::Ft8(err.to_string())
//     }
// }

/// Convenience Result type using PancettaError
pub type PancettaResult<T> = Result<T, PancettaError>;

/// Helper trait for converting errors with context
pub trait ErrorContext<T> {
    /// Add context to an error
    fn context<S: Into<String>>(self, msg: S) -> PancettaResult<T>;

    /// Add context using a closure (lazy evaluation)
    fn with_context<F, S>(self, f: F) -> PancettaResult<T>
    where
        F: FnOnce() -> S,
        S: Into<String>;
}

impl<T, E> ErrorContext<T> for Result<T, E>
where
    E: Error + 'static,
{
    fn context<S: Into<String>>(self, msg: S) -> PancettaResult<T> {
        self.map_err(|e| PancettaError::Other(format!("{}: {}", msg.into(), e)))
    }

    fn with_context<F, S>(self, f: F) -> PancettaResult<T>
    where
        F: FnOnce() -> S,
        S: Into<String>,
    {
        self.map_err(|e| PancettaError::Other(format!("{}: {}", f().into(), e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = PancettaError::InvalidInput("bad frequency".to_string());
        assert_eq!(err.to_string(), "Invalid input: bad frequency");
    }

    #[test]
    fn test_error_conversion() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let pancetta_err: PancettaError = io_err.into();
        assert!(matches!(pancetta_err, PancettaError::Io(_)));
    }

    #[test]
    fn test_error_context() {
        let result: Result<i32, io::Error> = Err(io::Error::new(io::ErrorKind::NotFound, "test"));
        let with_context = result.context("Failed to read file");
        assert!(with_context.is_err());
        assert!(with_context
            .unwrap_err()
            .to_string()
            .contains("Failed to read file"));
    }
}
