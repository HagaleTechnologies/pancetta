//! # Pancetta FT8 Codec
//!
//! High-performance FT8 digital mode decoder and encoder optimized for real-time processing.
//!
//! This crate provides a complete FT8 implementation with:
//! - >95% decode accuracy at SNR -20dB
//! - 12.64 second processing windows
//! - Support for 50+ simultaneous decodes
//! - Zero-allocation hot path
//! - Comprehensive time synchronization
//! - FT8 message encoding and transmission
//! - PTT control and safety features
//! - FCC Part 97 compliance
//!
//! ## Decoding Example
//!
//! ```rust
//! use pancetta_ft8::{Ft8Decoder, Ft8Config, DecodedMessage};
//!
//! let config = Ft8Config::default();
//! let mut decoder = Ft8Decoder::new(config)?;
//!
//! // Process 12.64 seconds of audio at 12kHz sample rate
//! let samples: Vec<f32> = vec![0.0; 151680]; // 12.64s * 12000 Hz
//! let decoded_messages = decoder.decode_window(&samples)?;
//!
//! for message in decoded_messages {
//!     println!("Decoded: {} (SNR: {:.1}dB, Confidence: {:.2})",
//!              message.text, message.snr_db, message.confidence);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Transmission Example
//!
//! ```rust,ignore
//! use pancetta_ft8::{Ft8Transmitter, TransmissionConfig};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = TransmissionConfig::default();
//! let mut transmitter = Ft8Transmitter::new(config)?;
//!
//! // Transmit a CQ call
//! let report = transmitter.transmit_cq("W1ABC", "FN42", 0.0).await?;
//! println!("Transmitted: {} in {:?}", report.message, report.duration);
//!
//! // Transmit a signal report
//! let report = transmitter.transmit_signal_report("K1DEF", "W1ABC", -12, 0.0).await?;
//! println!("Sent signal report: {}", report.message);
//! # Ok(())
//! # }
//! ```

#![allow(dead_code, unused_imports)]

// Protocol abstraction layer (FT8/FT4/FT2)
pub mod protocol;

// A Priori (AP) decoding support
pub mod ap;
pub use ap::{ApContext, ApLevel, MyCallAp, QsoAp, QsoApProgress, RecentCallAp};

// Core FT8 decoding modules
pub mod decoder;
pub mod ldpc;
pub mod message;
pub mod osd;
pub mod signal_processing;
pub mod sync;

// ft8_lib reference implementation (FFI)
pub mod ft8_lib_ffi;

// Benchmark harness for decoder comparison
#[cfg(feature = "benchmark")]
pub mod benchmark;

// FT8 transmission modules
#[cfg(feature = "transmit")]
pub mod encoder;
#[cfg(feature = "transmit")]
pub mod modulator;
#[cfg(feature = "transmit")]
pub mod transmit;

// Protocol exports
pub use protocol::{ModulationType, Protocol, ProtocolParams};

// Core decoding exports
pub use decoder::{Ft8Config, Ft8Decoder, WaterfallData};
pub use message::{DecodedMessage, Ft8Message, MessageType};
pub use signal_processing::{FftProcessor, WindowFunction};
pub use sync::{SyncResult, TimeSync};

// Transmission exports (when transmit feature is enabled)
#[cfg(feature = "transmit")]
pub use encoder::{Ft8Encoder, Ft8EncodingConfig};
#[cfg(feature = "transmit")]
pub use modulator::{
    convert_samples, modulate_multi_tx, AudioFormat, Ft8Modulator, ModulatorConfig, MultiTxItem,
    PulseShape, SampleType,
};
#[cfg(feature = "transmit")]
pub use transmit::{
    AudioConfig, BandLimits, FrequencyConfig, Ft8Transmitter, PowerConfig, PttConfig, PttMethod,
    SafetyConfig, TestReport, TransmissionConfig, TransmissionReport, TransmissionState,
    TransmissionStatistics,
};

use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Sample rate for FT8 processing (12 kHz)
pub const SAMPLE_RATE: u32 = 12_000;

/// FT8 symbol duration in seconds
pub const SYMBOL_DURATION: f64 = 0.16;

/// FT8 message duration in seconds (79 symbols * 0.16s)
pub const MESSAGE_DURATION: f64 = 12.64;

/// Number of samples in an FT8 window
pub const WINDOW_SAMPLES: usize = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;

/// FT8 bandwidth in Hz
pub const FT8_BANDWIDTH: f64 = 50.0;

/// Base frequency offset for FT8 (typically 1500 Hz)
pub const BASE_FREQUENCY: f64 = 1500.0;

/// Number of FT8 tones (8-FSK)
pub const NUM_TONES: usize = 8;

/// Number of symbols in FT8 transmission
pub const NUM_SYMBOLS: usize = 79;

/// Tone spacing in Hz
pub const TONE_SPACING: f64 = 6.25;

/// Result type for FT8 operations
pub type Ft8Result<T> = Result<T, Ft8Error>;

/// Errors that can occur during FT8 processing
#[derive(Error, Debug)]
pub enum Ft8Error {
    #[error("Invalid sample rate: expected {expected}, got {actual}")]
    InvalidSampleRate { expected: u32, actual: u32 },

    #[error("Invalid window size: expected {expected}, got {actual}")]
    InvalidWindowSize { expected: usize, actual: usize },

    #[error("FFT processing error: {0}")]
    FftError(String),

    #[error("Signal processing error: {0}")]
    SignalProcessingError(String),

    #[error("Message decoding error: {0}")]
    MessageDecodingError(String),

    #[error("Time synchronization error: {0}")]
    SyncError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Insufficient data: need {needed} samples, got {available}")]
    InsufficientData { needed: usize, available: usize },

    #[error("Invalid data size: expected {expected}, got {actual}")]
    InvalidDataSize { expected: usize, actual: usize },
}

/// Performance metrics for FT8 decoding
#[derive(Debug, Clone)]
pub struct DecodingMetrics {
    /// Number of messages decoded in this window
    pub messages_decoded: usize,

    /// Processing time for this window
    pub processing_time: Duration,

    /// Average SNR of decoded messages
    pub average_snr: f32,

    /// Peak memory usage during decoding
    pub peak_memory_bytes: usize,

    /// Time synchronization quality (0.0 - 1.0)
    pub sync_quality: f32,

    /// Timestamp when decoding completed
    pub timestamp: SystemTime,
}

impl Default for DecodingMetrics {
    fn default() -> Self {
        Self {
            messages_decoded: 0,
            processing_time: Duration::ZERO,
            average_snr: 0.0,
            peak_memory_bytes: 0,
            sync_quality: 0.0,
            timestamp: SystemTime::now(),
        }
    }
}

/// Trait for custom message handlers
pub trait MessageHandler {
    /// Called when a new message is decoded
    fn on_message_decoded(&mut self, message: &DecodedMessage, metrics: &DecodingMetrics);

    /// Called when decoding window starts
    fn on_window_start(&mut self, timestamp: SystemTime);

    /// Called when decoding window completes
    fn on_window_complete(&mut self, metrics: &DecodingMetrics);
}

/// No-op message handler for testing
pub struct NullMessageHandler;

impl MessageHandler for NullMessageHandler {
    fn on_message_decoded(&mut self, _message: &DecodedMessage, _metrics: &DecodingMetrics) {}
    fn on_window_start(&mut self, _timestamp: SystemTime) {}
    fn on_window_complete(&mut self, _metrics: &DecodingMetrics) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(SAMPLE_RATE, 12_000);
        assert_eq!(WINDOW_SAMPLES, 151_680); // 12.64s * 12kHz
        assert_eq!(NUM_TONES, 8);
        assert_eq!(TONE_SPACING, 6.25);
    }

    #[test]
    fn test_error_creation() {
        let error = Ft8Error::InvalidSampleRate {
            expected: 12000,
            actual: 48000,
        };
        assert!(error.to_string().contains("Invalid sample rate"));
    }

    #[test]
    fn test_metrics_default() {
        let metrics = DecodingMetrics::default();
        assert_eq!(metrics.messages_decoded, 0);
        assert_eq!(metrics.processing_time, Duration::ZERO);
    }
}
