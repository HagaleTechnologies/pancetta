//! Core rig control implementation with connection management
//!
//! This module provides the main rig control interface with safe wrappers
//! around hamlib functionality, including connection management, error recovery,
//! and timeout handling.
use crate::error::HamlibError;
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

/// A confirmed pair of receive-gain controls in integer dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiveGain {
    /// Preamp gain, with zero meaning bypass.
    pub preamp_db: i32,
    /// Attenuation, with zero meaning bypass.
    pub attenuator_db: i32,
}

/// Gain steps advertised by the connected backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveGainCapabilities {
    /// Supported preamp settings, including bypass.
    pub preamp_db: Vec<i32>,
    /// Supported attenuation settings, including bypass.
    pub attenuator_db: Vec<i32>,
}

/// Trait for rig control operations
#[async_trait]
pub trait RigControl: Send + Sync {
    /// Discover gain support; unsupported backends never invent settings.
    async fn receive_gain_capabilities(&self) -> Result<ReceiveGainCapabilities> {
        Err(HamlibError::not_supported("receive_gain_capabilities", None, None).into())
    }

    /// Read both receive controls without inventing bypass on failure.
    async fn get_receive_gain(&self) -> Result<ReceiveGain> {
        Err(HamlibError::not_supported("get_receive_gain", None, None).into())
    }

    /// Apply both controls and confirm readback before an absolute deadline.
    /// A partial update or timeout is an error and still requires restoration.
    async fn set_receive_gain(
        &self,
        _gain: ReceiveGain,
        _deadline: tokio::time::Instant,
    ) -> Result<()> {
        Err(HamlibError::not_supported("set_receive_gain", None, None).into())
    }

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

    /// Enable or disable split operation, selecting which VFO transmits.
    /// Default impl is a no-op for rigs/mocks that do not model split.
    async fn set_split(&self, _enabled: bool, _tx_vfo: Vfo) -> Result<()> {
        Ok(())
    }

    /// Read whether split is enabled and which VFO transmits.
    /// Unsupported backends must report an error, never invent simplex state.
    async fn get_split(&self) -> Result<(bool, Vfo)> {
        Err(HamlibError::not_supported("get_split", None, None).into())
    }

    /// Read the split TX-VFO frequency in Hz, meaningful when split is enabled.
    async fn get_split_freq(&self) -> Result<u64> {
        Err(HamlibError::not_supported("get_split_freq", None, None).into())
    }

    /// Set the transmit-VFO frequency (Hz) used while split is enabled.
    /// Default impl is a no-op for rigs/mocks that do not model split.
    async fn set_split_freq(&self, _tx_freq: u64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod split_default_tests {
    use super::*;

    struct DefaultOnlyRig;

    #[async_trait]
    impl RigControl for DefaultOnlyRig {
        async fn connect(&self) -> Result<()> {
            unimplemented!()
        }
        async fn disconnect(&self) -> Result<()> {
            unimplemented!()
        }
        async fn get_status(&self) -> Result<RigStatus> {
            unimplemented!()
        }
        async fn set_frequency(&self, _vfo: Vfo, _freq: u64) -> Result<()> {
            unimplemented!()
        }
        async fn get_frequency(&self, _vfo: Vfo) -> Result<u64> {
            unimplemented!()
        }
        async fn set_mode(&self, _vfo: Vfo, _mode: Mode, _width: Option<i32>) -> Result<()> {
            unimplemented!()
        }
        async fn get_mode(&self, _vfo: Vfo) -> Result<(Mode, i32)> {
            unimplemented!()
        }
        async fn set_vfo(&self, _vfo: Vfo) -> Result<()> {
            unimplemented!()
        }
        async fn get_vfo(&self) -> Result<Vfo> {
            unimplemented!()
        }
        async fn set_ptt(&self, _vfo: Vfo, _ptt: PttState) -> Result<()> {
            unimplemented!()
        }
        async fn get_ptt(&self, _vfo: Vfo) -> Result<PttState> {
            unimplemented!()
        }
        async fn set_power_level(&self, _level: f32) -> Result<()> {
            unimplemented!()
        }
        async fn get_power_level(&self) -> Result<f32> {
            unimplemented!()
        }
        async fn get_s_meter(&self) -> Result<i32> {
            unimplemented!()
        }
        async fn get_swr(&self) -> Result<f32> {
            unimplemented!()
        }
        async fn set_memory_channel(&self, _vfo: Vfo, _channel: i32) -> Result<()> {
            unimplemented!()
        }
        async fn get_memory_channel(&self, _vfo: Vfo) -> Result<i32> {
            unimplemented!()
        }
        async fn set_scan(&self, _vfo: Vfo, _enable: bool) -> Result<()> {
            unimplemented!()
        }
        async fn get_info(&self) -> Result<String> {
            unimplemented!()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_receive_gain_is_unsupported() {
        assert!(DefaultOnlyRig.receive_gain_capabilities().await.is_err());
        assert!(DefaultOnlyRig.get_receive_gain().await.is_err());
        assert!(DefaultOnlyRig
            .set_receive_gain(
                ReceiveGain {
                    preamp_db: 0,
                    attenuator_db: 0
                },
                tokio::time::Instant::now()
            )
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_split_read_fails_closed() {
        assert!(DefaultOnlyRig
            .get_split()
            .await
            .unwrap_err()
            .to_string()
            .contains("Feature not supported"));
        assert!(DefaultOnlyRig
            .get_split_freq()
            .await
            .unwrap_err()
            .to_string()
            .contains("Feature not supported"));
    }
}
