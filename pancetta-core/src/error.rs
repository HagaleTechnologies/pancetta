// Unified Error Handling for Pancetta
//
// This module provides a standardized error handling approach across all Pancetta components.
// It implements a hierarchical error system with context tracking, severity levels, and
// automatic retry policies.

use std::fmt;
use std::error::Error as StdError;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Severity levels for errors
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorSeverity {
    /// Debug-level error (can be ignored in production)
    Debug,
    /// Informational warning
    Info,
    /// Warning that should be addressed
    Warning,
    /// Error that affects functionality
    Error,
    /// Critical error requiring immediate attention
    Critical,
    /// Fatal error that will cause shutdown
    Fatal,
}

/// Error context providing additional information
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Component where error occurred
    pub component: String,
    /// Operation being performed
    pub operation: String,
    /// Timestamp when error occurred
    pub timestamp: Instant,
    /// Additional key-value pairs for context
    pub metadata: Vec<(String, String)>,
    /// Suggested recovery action
    pub recovery_hint: Option<String>,
    /// Whether error is retryable
    pub retryable: bool,
    /// Maximum retry attempts
    pub max_retries: u32,
    /// Retry delay strategy
    pub retry_delay: RetryDelay,
}

/// Retry delay strategies
#[derive(Debug, Clone, Copy)]
pub enum RetryDelay {
    /// Fixed delay between retries
    Fixed(Duration),
    /// Linear backoff (delay * attempt)
    Linear(Duration),
    /// Exponential backoff (delay * 2^attempt)
    Exponential(Duration),
    /// No retry
    None,
}

/// Base error type for all Pancetta components
#[derive(Debug, Error)]
pub struct PancettaError {
    /// Error severity
    pub severity: ErrorSeverity,
    /// Error context
    pub context: ErrorContext,
    /// Error message
    pub message: String,
    /// Optional error code for programmatic handling
    pub code: Option<String>,
    /// Source error (if any)
    #[source]
    pub source: Option<Box<dyn StdError + Send + Sync>>,
}

impl fmt::Display for PancettaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{:?}] {} in {}: {}",
            self.severity,
            self.context.component,
            self.context.operation,
            self.message
        )?;
        
        if let Some(ref code) = self.code {
            write!(f, " ({})", code)?;
        }
        
        if let Some(ref hint) = self.context.recovery_hint {
            write!(f, " | Hint: {}", hint)?;
        }
        
        Ok(())
    }
}

/// Component-specific error types using the unified base
#[derive(Debug, Error)]
pub enum ComponentError {
    /// Audio component errors
    #[error("Audio error: {0}")]
    Audio(PancettaError),
    
    /// DSP pipeline errors
    #[error("DSP error: {0}")]
    Dsp(PancettaError),
    
    /// FT8 decoder errors
    #[error("FT8 error: {0}")]
    Ft8(PancettaError),
    
    /// TUI errors
    #[error("TUI error: {0}")]
    Tui(PancettaError),
    
    /// Configuration errors
    #[error("Config error: {0}")]
    Config(PancettaError),
    
    /// Hamlib errors
    #[error("Hamlib error: {0}")]
    Hamlib(PancettaError),
    
    /// QSO management errors
    #[error("QSO error: {0}")]
    Qso(PancettaError),
    
    /// DX cluster errors
    #[error("DX error: {0}")]
    Dx(PancettaError),
    
    /// Message bus errors
    #[error("Message bus error: {0}")]
    MessageBus(PancettaError),
    
    /// Coordinator errors
    #[error("Coordinator error: {0}")]
    Coordinator(PancettaError),
}

/// Error builder for fluent error construction
pub struct ErrorBuilder {
    severity: ErrorSeverity,
    component: String,
    operation: String,
    message: String,
    code: Option<String>,
    metadata: Vec<(String, String)>,
    recovery_hint: Option<String>,
    retryable: bool,
    max_retries: u32,
    retry_delay: RetryDelay,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl ErrorBuilder {
    /// Create new error builder
    pub fn new(component: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            severity: ErrorSeverity::Error,
            component: component.into(),
            operation: operation.into(),
            message: String::new(),
            code: None,
            metadata: Vec::new(),
            recovery_hint: None,
            retryable: false,
            max_retries: 0,
            retry_delay: RetryDelay::None,
            source: None,
        }
    }
    
    /// Set severity
    pub fn severity(mut self, severity: ErrorSeverity) -> Self {
        self.severity = severity;
        self
    }
    
    /// Set message
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
    
    /// Set error code
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    
    /// Add metadata
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.push((key.into(), value.into()));
        self
    }
    
    /// Set recovery hint
    pub fn recovery_hint(mut self, hint: impl Into<String>) -> Self {
        self.recovery_hint = Some(hint.into());
        self
    }
    
    /// Make retryable with strategy
    pub fn retryable(mut self, max_retries: u32, delay: RetryDelay) -> Self {
        self.retryable = true;
        self.max_retries = max_retries;
        self.retry_delay = delay;
        self
    }
    
    /// Set source error
    pub fn source<E: StdError + Send + Sync + 'static>(mut self, source: E) -> Self {
        self.source = Some(Box::new(source));
        self
    }
    
    /// Build the error
    pub fn build(self) -> PancettaError {
        PancettaError {
            severity: self.severity,
            context: ErrorContext {
                component: self.component,
                operation: self.operation,
                timestamp: Instant::now(),
                metadata: self.metadata,
                recovery_hint: self.recovery_hint,
                retryable: self.retryable,
                max_retries: self.max_retries,
                retry_delay: self.retry_delay,
            },
            message: self.message,
            code: self.code,
            source: self.source,
        }
    }
}

/// Result type alias for Pancetta operations
pub type PancettaResult<T> = Result<T, PancettaError>;

/// Trait for converting various error types to PancettaError
pub trait IntoPancettaError {
    /// Convert to PancettaError with context
    fn into_pancetta_error(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
    ) -> PancettaError;
}

impl<E: StdError + Send + Sync + 'static> IntoPancettaError for E {
    fn into_pancetta_error(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
    ) -> PancettaError {
        ErrorBuilder::new(component, operation)
            .message(format!("Operation failed: {}", self))
            .source(self)
            .build()
    }
}

/// Extension trait for Result types
pub trait ResultExt<T> {
    /// Add context to error
    fn with_context(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
    ) -> PancettaResult<T>;
    
    /// Add context and make retryable
    fn retryable(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
        max_retries: u32,
        delay: RetryDelay,
    ) -> PancettaResult<T>;
}

impl<T, E: StdError + Send + Sync + 'static> ResultExt<T> for Result<T, E> {
    fn with_context(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
    ) -> PancettaResult<T> {
        self.map_err(|e| e.into_pancetta_error(component, operation))
    }
    
    fn retryable(
        self,
        component: impl Into<String>,
        operation: impl Into<String>,
        max_retries: u32,
        delay: RetryDelay,
    ) -> PancettaResult<T> {
        self.map_err(|e| {
            ErrorBuilder::new(component, operation)
                .message(format!("Operation failed: {}", e))
                .source(e)
                .retryable(max_retries, delay)
                .build()
        })
    }
}

/// Error recovery manager for handling retries
pub struct ErrorRecovery {
    attempt: u32,
    max_attempts: u32,
    delay_strategy: RetryDelay,
    last_error: Option<PancettaError>,
}

impl ErrorRecovery {
    /// Create new recovery manager
    pub fn new(error: &PancettaError) -> Option<Self> {
        if error.context.retryable {
            Some(Self {
                attempt: 0,
                max_attempts: error.context.max_retries,
                delay_strategy: error.context.retry_delay,
                last_error: None,
            })
        } else {
            None
        }
    }
    
    /// Check if retry should be attempted
    pub fn should_retry(&self) -> bool {
        self.attempt < self.max_attempts
    }
    
    /// Get delay before next retry
    pub fn next_delay(&self) -> Duration {
        match self.delay_strategy {
            RetryDelay::Fixed(d) => d,
            RetryDelay::Linear(d) => d * (self.attempt + 1),
            RetryDelay::Exponential(d) => d * 2_u32.pow(self.attempt),
            RetryDelay::None => Duration::ZERO,
        }
    }
    
    /// Record retry attempt
    pub fn record_attempt(&mut self, error: PancettaError) {
        self.attempt += 1;
        self.last_error = Some(error);
    }
    
    /// Get final error after all retries
    pub fn final_error(self) -> Option<PancettaError> {
        self.last_error
    }
}

/// Macro for quickly creating errors
#[macro_export]
macro_rules! pancetta_error {
    ($component:expr, $operation:expr, $message:expr) => {
        $crate::error::ErrorBuilder::new($component, $operation)
            .message($message)
            .build()
    };
    
    ($component:expr, $operation:expr, $message:expr, $severity:expr) => {
        $crate::error::ErrorBuilder::new($component, $operation)
            .message($message)
            .severity($severity)
            .build()
    };
}

/// Macro for creating retryable errors
#[macro_export]
macro_rules! retryable_error {
    ($component:expr, $operation:expr, $message:expr, $retries:expr) => {
        $crate::error::ErrorBuilder::new($component, $operation)
            .message($message)
            .retryable($retries, $crate::error::RetryDelay::Exponential(
                std::time::Duration::from_millis(100)
            ))
            .build()
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_error_builder() {
        let error = ErrorBuilder::new("TestComponent", "test_operation")
            .message("Test error message")
            .severity(ErrorSeverity::Warning)
            .code("TEST001")
            .metadata("key", "value")
            .recovery_hint("Try again later")
            .retryable(3, RetryDelay::Fixed(Duration::from_secs(1)))
            .build();
        
        assert_eq!(error.severity, ErrorSeverity::Warning);
        assert_eq!(error.context.component, "TestComponent");
        assert_eq!(error.context.operation, "test_operation");
        assert_eq!(error.message, "Test error message");
        assert_eq!(error.code, Some("TEST001".to_string()));
        assert!(error.context.retryable);
        assert_eq!(error.context.max_retries, 3);
    }
    
    #[test]
    fn test_error_recovery() {
        let make_error = || ErrorBuilder::new("Test", "op")
            .message("Retryable error")
            .retryable(3, RetryDelay::Exponential(Duration::from_millis(100)))
            .build();

        let error = make_error();
        let mut recovery = ErrorRecovery::new(&error).unwrap();

        assert!(recovery.should_retry());
        assert_eq!(recovery.next_delay(), Duration::from_millis(100));

        recovery.record_attempt(make_error());
        assert!(recovery.should_retry());
        assert_eq!(recovery.next_delay(), Duration::from_millis(200));

        recovery.record_attempt(make_error());
        assert!(recovery.should_retry());
        assert_eq!(recovery.next_delay(), Duration::from_millis(400));

        recovery.record_attempt(make_error());
        assert!(!recovery.should_retry());
    }
    
    #[test]
    fn test_error_display() {
        let error = ErrorBuilder::new("Audio", "process_samples")
            .message("Buffer overflow")
            .severity(ErrorSeverity::Error)
            .code("AUD001")
            .recovery_hint("Reduce sample rate")
            .build();
        
        let display = format!("{}", error);
        assert!(display.contains("Audio"));
        assert!(display.contains("process_samples"));
        assert!(display.contains("Buffer overflow"));
        assert!(display.contains("AUD001"));
        assert!(display.contains("Reduce sample rate"));
    }
}