//! Core rig control implementation with connection management
//!
//! This module provides the main rig control interface with safe wrappers
//! around hamlib functionality, including connection management, error recovery,
//! and timeout handling.
use crate::models::{Mode, RigModelType, Vfo};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default timeout for rig operations in milliseconds.
const DEFAULT_TIMEOUT_MS: u32 = 2000;

/// Default retry count for failed operations.
const DEFAULT_RETRY_COUNT: u32 = 3;

/// Rig connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Disconnected
    Disconnected,
    /// Connecting
    Connecting,
    /// Connected and ready
    Connected,
    /// Error state
    Error,
    /// Reconnecting after error
    Reconnecting,
}

/// PTT (Push-to-Talk) state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    /// PTT off (receive mode)
    Off,
    /// PTT on (transmit mode)
    On,
    /// PTT on via microphone
    OnMic,
    /// PTT on via data port
    OnData,
}

/// Rig configuration for connection
#[derive(Debug, Clone)]
pub struct RigConfig {
    /// Rig model
    pub model: RigModelType,
    /// Device path (e.g., "/dev/ttyUSB0", "COM3", "localhost:4532")
    pub device_path: String,
    /// Baud rate for serial connections
    pub baud_rate: Option<u32>,
    /// Connection timeout in milliseconds
    pub timeout_ms: u32,
    /// Number of retries for failed operations
    pub retry_count: u32,
    /// Enable automatic reconnection
    pub auto_reconnect: bool,
    /// Poll interval for status updates in milliseconds
    pub poll_interval_ms: u32,
    /// Additional hamlib parameters
    pub hamlib_params: HashMap<String, String>,
}

impl Default for RigConfig {
    fn default() -> Self {
        Self {
            model: RigModelType::Dummy,
            device_path: "/dev/ttyUSB0".to_string(),
            baud_rate: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            retry_count: DEFAULT_RETRY_COUNT,
            auto_reconnect: true,
            poll_interval_ms: 1000,
            hamlib_params: HashMap::new(),
        }
    }
}

/// Current rig status
#[derive(Debug, Clone)]
pub struct RigStatus {
    /// Connection state
    pub connection_state: ConnectionState,
    /// Current frequency in Hz
    pub frequency: Option<u64>,
    /// Current mode
    pub mode: Option<Mode>,
    /// Current passband width in Hz
    pub width: Option<i32>,
    /// Current VFO
    pub vfo: Option<Vfo>,
    /// PTT state
    pub ptt: Option<PttState>,
    /// Power level (0.0-1.0)
    pub power_level: Option<f32>,
    /// S-meter reading
    pub s_meter: Option<i32>,
    /// SWR reading
    pub swr: Option<f32>,
    /// Current memory channel
    pub memory_channel: Option<i32>,
    /// Last update timestamp
    pub last_update: Instant,
    /// Last error message
    pub last_error: Option<String>,
}

impl Default for RigStatus {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected,
            frequency: None,
            mode: None,
            width: None,
            vfo: None,
            ptt: None,
            power_level: None,
            s_meter: None,
            swr: None,
            memory_channel: None,
            last_update: Instant::now(),
            last_error: None,
        }
    }
}

/// Rig operation result with error recovery information
#[derive(Debug)]
pub struct RigOperationResult<T> {
    /// Operation result
    pub result: Result<T>,
    /// Number of retries attempted
    pub retries: u32,
    /// Operation duration
    pub duration: Duration,
    /// Whether operation required reconnection
    pub reconnected: bool,
}

/// Trait for rig control operations
#[async_trait]
pub trait RigControl: Send + Sync {
    /// Connect to the rig
    async fn connect(&self) -> Result<()>;

    /// Disconnect from the rig
    async fn disconnect(&self) -> Result<()>;

    /// Get current rig status
    async fn get_status(&self) -> Result<RigStatus>;

    /// Set frequency in Hz
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()>;

    /// Get frequency in Hz
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64>;

    /// Set mode and passband
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()>;

    /// Get mode and passband
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)>;

    /// Set VFO
    async fn set_vfo(&self, vfo: Vfo) -> Result<()>;

    /// Get current VFO
    async fn get_vfo(&self) -> Result<Vfo>;

    /// Set PTT state
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()>;

    /// Get PTT state
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState>;

    /// Set power level (0.0-1.0)
    async fn set_power_level(&self, level: f32) -> Result<()>;

    /// Get power level
    async fn get_power_level(&self) -> Result<f32>;

    /// Get S-meter reading
    async fn get_s_meter(&self) -> Result<i32>;

    /// Get SWR reading
    async fn get_swr(&self) -> Result<f32>;

    /// Set memory channel
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()>;

    /// Get memory channel
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32>;

    /// Start/stop scanning
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()>;

    /// Get rig information
    async fn get_info(&self) -> Result<String>;
}
