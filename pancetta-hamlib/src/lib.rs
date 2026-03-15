//! Pancetta Hamlib Integration
//!
//! This crate provides safe, async Rust bindings for the Hamlib amateur radio
//! control library, enabling comprehensive control of amateur radio transceivers
//! through CAT (Computer Aided Transceiver) interfaces.
//!
//! # Features
//!
//! - **Safe API**: Memory-safe wrappers around Hamlib C library
//! - **Async Support**: Full async/await support for non-blocking operations
//! - **Connection Management**: Automatic reconnection and error recovery
//! - **Comprehensive Control**: Frequency, mode, VFO, PTT, power, and more
//! - **Advanced Features**: Band switching, memory channels, scanning, monitoring
//! - **Mock Implementation**: Complete mock rig for testing and development
//! - **Real-time Monitoring**: Live S-meter, SWR, and other parameter monitoring
//! - **Band Management**: Standard amateur radio band plans and switching
//!
//! # Quick Start
//!
//! ```no_run
//! use pancetta_hamlib::{RigBuilder, RigModelType, Vfo};
//! use pancetta_core::Mode;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create and connect to a rig
//!     let rig = RigBuilder::new()
//!         .model(RigModelType::IcomIC7300)
//!         .device_path("/dev/ttyUSB0")
//!         .baud_rate(19200)
//!         .build()
//!         .await?;
//!
//!     // Connect to the rig
//!     rig.connect().await?;
//!
//!     // Set frequency to 14.200 MHz
//!     rig.set_frequency(Vfo::A, 14_200_000).await?;
//!
//!     // Set mode to USB
//!     rig.set_mode(Vfo::A, Mode::USB, None).await?;
//!
//!     // Read S-meter
//!     let s_meter = rig.get_s_meter().await?;
//!     println!("S-meter: {} dBm", s_meter);
//!
//!     Ok(())
//! }
//! ```
//!
//! # Mock Rig for Testing
//!
//! ```
//! use pancetta_hamlib::{MockRig, RigControl, Mode, Vfo};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create mock rig for testing
//!     let rig = MockRig::default();
//!     
//!     rig.connect().await?;
//!     rig.set_frequency(Vfo::A, 14_200_000).await?;
//!     
//!     let freq = rig.get_frequency(Vfo::A).await?;
//!     assert_eq!(freq, 14_200_000);
//!     
//!     Ok(())
//! }
//! ```
//!
//! # Advanced Features
//!
//! ```no_run
//! use pancetta_hamlib::{AdvancedRig, AdvancedRigControl, Band, ScanConfig, ScanType};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create advanced rig controller
//!     let rig = AdvancedRig::new(/* base rig */);
//!     
//!     // Switch to 20m band
//!     rig.switch_to_band(Band::Band20m).await?;
//!     
//!     // Start memory channel scanning
//!     let scan_config = ScanConfig {
//!         scan_type: ScanType::Memory,
//!         speed: 5.0,
//!         ..Default::default()
//!     };
//!     rig.start_scan(scan_config).await?;
//!     
//!     // Start real-time monitoring
//!     let mut monitor_rx = rig.start_monitoring(1000).await?;
//!     
//!     Ok(())
//! }
//! ```

#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod advanced;
pub mod bindings;
pub mod error;
pub mod models;
pub mod rig;
pub mod rigctld;

#[cfg(feature = "mock-rig")]
pub mod mock;

// Re-export key types for public API
pub use models::{Band, Mode, RigCapabilities, RigModelRegistry, RigModelType, Vfo};

pub use rig::{ConnectionState, PttState, Rig, RigConfig, RigControl, RigStatus};

pub use advanced::{
    AdvancedRig, AdvancedRigControl, BandPlan, MemoryChannel, MonitoringData, ScanConfig,
    ScanStatus, ScanType,
};

pub use error::{ContextualError, ContextualResult, ErrorContext, ErrorSeverity, HamlibError};

#[cfg(feature = "mock-rig")]
pub use mock::{MockRig, MockRigConfig};

pub use rigctld::{RigctldClient, RigctldConfig};

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

/// Simplified result type for hamlib operations (re-exported from error module)
pub type HamlibResult<T> = Result<T, HamlibError>;

/// Rig builder for convenient rig creation
pub struct RigBuilder {
    model: Option<RigModelType>,
    device_path: Option<String>,
    baud_rate: Option<u32>,
    timeout_ms: Option<u32>,
    retry_count: Option<u32>,
    auto_reconnect: Option<bool>,
    poll_interval_ms: Option<u32>,
    hamlib_params: HashMap<String, String>,
}

impl RigBuilder {
    /// Create new rig builder
    pub fn new() -> Self {
        Self {
            model: None,
            device_path: None,
            baud_rate: None,
            timeout_ms: None,
            retry_count: None,
            auto_reconnect: None,
            poll_interval_ms: None,
            hamlib_params: HashMap::new(),
        }
    }

    /// Set rig model
    pub fn model(mut self, model: RigModelType) -> Self {
        self.model = Some(model);
        self
    }

    /// Set device path
    pub fn device_path<S: Into<String>>(mut self, path: S) -> Self {
        self.device_path = Some(path.into());
        self
    }

    /// Set baud rate
    pub fn baud_rate(mut self, baud_rate: u32) -> Self {
        self.baud_rate = Some(baud_rate);
        self
    }

    /// Set timeout in milliseconds
    pub fn timeout_ms(mut self, timeout: u32) -> Self {
        self.timeout_ms = Some(timeout);
        self
    }

    /// Set retry count
    pub fn retry_count(mut self, retries: u32) -> Self {
        self.retry_count = Some(retries);
        self
    }

    /// Enable/disable auto reconnection
    pub fn auto_reconnect(mut self, enable: bool) -> Self {
        self.auto_reconnect = Some(enable);
        self
    }

    /// Set polling interval in milliseconds
    pub fn poll_interval_ms(mut self, interval: u32) -> Self {
        self.poll_interval_ms = Some(interval);
        self
    }

    /// Add hamlib parameter
    pub fn hamlib_param<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.hamlib_params.insert(key.into(), value.into());
        self
    }

    /// Build the rig
    pub async fn build(self) -> Result<Arc<Rig>> {
        let model = self
            .model
            .ok_or_else(|| anyhow!("Model must be specified"))?;
        let device_path = self
            .device_path
            .unwrap_or_else(|| "/dev/ttyUSB0".to_string());

        let config = RigConfig {
            model,
            device_path,
            baud_rate: self.baud_rate,
            timeout_ms: self.timeout_ms.unwrap_or(2000),
            retry_count: self.retry_count.unwrap_or(3),
            auto_reconnect: self.auto_reconnect.unwrap_or(true),
            poll_interval_ms: self.poll_interval_ms.unwrap_or(1000),
            hamlib_params: self.hamlib_params,
        };

        Ok(Arc::new(Rig::new(config)))
    }

    /// Build a mock rig for testing
    #[cfg(feature = "mock-rig")]
    pub async fn build_mock(self) -> Result<Arc<MockRig>> {
        let mock_config = MockRigConfig {
            connection_delay_ms: 10,
            operation_delay_ms: 1,
            failure_rate: 0.0,
            simulate_s_meter: true,
            s_meter_noise: 5,
            simulate_swr: true,
            base_swr: 1.1,
            frequency_range: (1_800_000, 450_000_000),
            memory_channels: 100,
        };

        Ok(Arc::new(MockRig::new(mock_config)))
    }
}

impl Default for RigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Advanced rig builder for creating AdvancedRig instances
pub struct AdvancedRigBuilder {
    base_rig: Option<Arc<Rig>>,
    #[cfg(feature = "mock-rig")]
    mock_rig: Option<Arc<MockRig>>,
}

impl AdvancedRigBuilder {
    /// Create new advanced rig builder
    pub fn new() -> Self {
        Self {
            base_rig: None,
            #[cfg(feature = "mock-rig")]
            mock_rig: None,
        }
    }

    /// Use existing rig
    pub fn with_rig(mut self, rig: Arc<Rig>) -> Self {
        self.base_rig = Some(rig);
        self
    }

    /// Use mock rig
    #[cfg(feature = "mock-rig")]
    pub fn with_mock_rig(mut self, rig: Arc<MockRig>) -> Self {
        self.mock_rig = Some(rig);
        self
    }

    /// Build from rig builder
    pub async fn from_rig_builder(self, builder: RigBuilder) -> Result<AdvancedRig> {
        let rig = builder.build().await?;
        Ok(AdvancedRig::new(rig))
    }

    /// Build the advanced rig
    pub async fn build(self) -> Result<AdvancedRig> {
        if let Some(rig) = self.base_rig {
            return Ok(AdvancedRig::new(rig));
        }

        #[cfg(feature = "mock-rig")]
        if let Some(_mock_rig) = self.mock_rig {
            // For mock rig, we need to wrap it in a way that's compatible with AdvancedRig
            // This is a simplified implementation - in practice you might want a different approach
            let rig_builder = RigBuilder::new()
                .model(RigModelType::Dummy)
                .device_path("mock");
            let rig = rig_builder.build().await?;
            return Ok(AdvancedRig::new(rig));
        }

        Err(anyhow!("No rig specified for advanced rig builder"))
    }
}

impl Default for AdvancedRigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Utility functions for working with rigs
pub mod utils {
    use super::*;

    /// Convert frequency to band
    pub fn frequency_to_band(frequency: u64) -> Option<Band> {
        Band::from_frequency(frequency)
    }

    /// Get default frequency for band
    pub fn band_default_frequency(band: Band) -> u64 {
        band.frequency_range().0 + 200_000 // 200 kHz above band edge
    }

    /// Get default mode for band
    pub fn band_default_mode(band: Band) -> Mode {
        match band {
            Band::Band160m | Band::Band80m | Band::Band40m => Mode::LSB,
            Band::Band20m | Band::Band15m | Band::Band10m => Mode::USB,
            Band::Band30m | Band::Band17m | Band::Band12m => Mode::USB,
            Band::Band6m | Band::Band2m | Band::Band70cm => Mode::FM,
            Band::Band60m => Mode::USB,
            Band::Custom(_, _) => Mode::USB,
        }
    }

    /// Validate frequency for amateur radio use
    pub fn is_amateur_frequency(frequency: u64) -> bool {
        let amateur_bands = [
            Band::Band160m,
            Band::Band80m,
            Band::Band60m,
            Band::Band40m,
            Band::Band30m,
            Band::Band20m,
            Band::Band17m,
            Band::Band15m,
            Band::Band12m,
            Band::Band10m,
            Band::Band6m,
            Band::Band2m,
            Band::Band70cm,
        ];

        amateur_bands.iter().any(|band| {
            let (low, high) = band.frequency_range();
            frequency >= low && frequency <= high
        })
    }

    /// Format frequency for display
    pub fn format_frequency(frequency: u64) -> String {
        if frequency >= 1_000_000_000 {
            format!("{:.3} GHz", frequency as f64 / 1_000_000_000.0)
        } else if frequency >= 1_000_000 {
            format!("{:.3} MHz", frequency as f64 / 1_000_000.0)
        } else if frequency >= 1_000 {
            format!("{:.1} kHz", frequency as f64 / 1_000.0)
        } else {
            format!("{} Hz", frequency)
        }
    }

    /// Parse frequency from string
    pub fn parse_frequency(input: &str) -> Result<u64> {
        let input = input.trim().to_lowercase();

        if let Some(value_str) = input.strip_suffix("ghz") {
            let value: f64 = value_str.trim().parse()?;
            Ok((value * 1_000_000_000.0) as u64)
        } else if let Some(value_str) = input.strip_suffix("mhz") {
            let value: f64 = value_str.trim().parse()?;
            Ok((value * 1_000_000.0) as u64)
        } else if let Some(value_str) = input.strip_suffix("khz") {
            let value: f64 = value_str.trim().parse()?;
            Ok((value * 1_000.0) as u64)
        } else if let Some(value_str) = input.strip_suffix("hz") {
            let value: u64 = value_str.trim().parse()?;
            Ok(value)
        } else {
            // Try to parse as Hz if no suffix
            let value: u64 = input.parse()?;
            Ok(value)
        }
    }

    /// Get S-meter reading as string
    pub fn format_s_meter(dbm: i32) -> String {
        if dbm >= -93 {
            let s_unit = ((dbm + 93) / 6) + 1;
            if s_unit <= 9 {
                format!("S{}", s_unit.min(9))
            } else {
                let over = s_unit - 9;
                format!("S9+{}", over * 10)
            }
        } else {
            format!("S0 ({} dBm)", dbm)
        }
    }

    /// Format SWR reading
    pub fn format_swr(swr: f32) -> String {
        format!("{:.2}:1", swr)
    }

    /// Get common frequencies for a band
    pub fn band_common_frequencies(band: Band) -> Vec<(u64, &'static str)> {
        match band {
            Band::Band20m => vec![
                (14_000_000, "CW"),
                (14_070_000, "Digital"),
                (14_074_000, "FT8"),
                (14_080_000, "FT4"),
                (14_200_000, "Phone"),
                (14_230_000, "Phone"),
            ],
            Band::Band40m => vec![
                (7_000_000, "CW"),
                (7_035_000, "Digital"),
                (7_074_000, "FT8"),
                (7_080_000, "FT4"),
                (7_200_000, "Phone"),
            ],
            Band::Band80m => vec![
                (3_500_000, "CW"),
                (3_570_000, "Digital"),
                (3_573_000, "FT8"),
                (3_575_000, "FT4"),
                (3_700_000, "Phone"),
            ],
            Band::Band15m => vec![
                (21_000_000, "CW"),
                (21_070_000, "Digital"),
                (21_074_000, "FT8"),
                (21_091_000, "FT4"),
                (21_200_000, "Phone"),
            ],
            Band::Band10m => vec![
                (28_000_000, "CW"),
                (28_070_000, "Digital"),
                (28_074_000, "FT8"),
                (28_180_000, "FT4"),
                (28_200_000, "Phone"),
                (29_600_000, "FM"),
            ],
            _ => vec![],
        }
    }
}

/// Prelude module for convenient imports
pub mod prelude {
    pub use crate::{
        AdvancedRig, AdvancedRigBuilder, AdvancedRigControl, Band, ConnectionState,
        ContextualError, ContextualResult, ErrorContext, ErrorSeverity, HamlibError, HamlibResult,
        MemoryChannel, Mode, MonitoringData, PttState, Rig, RigBuilder, RigCapabilities, RigConfig,
        RigControl, RigModelType, RigStatus, ScanConfig, ScanStatus, ScanType, Vfo,
    };

    pub use crate::models::ModeExt;

    #[cfg(feature = "mock-rig")]
    pub use crate::{MockRig, MockRigConfig};

    pub use crate::utils::*;
}

// Library initialization
static INIT: std::sync::Once = std::sync::Once::new();

/// Initialize the hamlib library
///
/// This function should be called once before using any hamlib functionality.
/// It's safe to call multiple times - subsequent calls will be ignored.
pub fn init() {
    INIT.call_once(|| {
        info!("Initializing pancetta-hamlib");

        // Initialize hamlib with appropriate debug level
        #[cfg(debug_assertions)]
        {
            unsafe {
                crate::bindings::rig_init(2); // Debug level 2 for development
            }
        }

        #[cfg(not(debug_assertions))]
        {
            unsafe {
                crate::bindings::rig_init(0); // Error level only for release
            }
        }

        info!("Hamlib initialized successfully");
    });
}

/// Get library version information
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Get supported rig models
pub fn supported_models() -> Vec<(RigModelType, &'static str, &'static str)> {
    vec![
        (RigModelType::Dummy, "Hamlib", "Dummy"),
        (RigModelType::NetRigctl, "Hamlib", "Network rigctl"),
        (RigModelType::YaesuFT991A, "Yaesu", "FT-991A"),
        (RigModelType::YaesuFTdx10, "Yaesu", "FT-dx10"),
        (RigModelType::YaesuFT818, "Yaesu", "FT-818"),
        (RigModelType::IcomIC7300, "Icom", "IC-7300"),
        (RigModelType::IcomIC7610, "Icom", "IC-7610"),
        (RigModelType::IcomIC705, "Icom", "IC-705"),
        (RigModelType::KenwoodTS590SG, "Kenwood", "TS-590SG"),
        (RigModelType::KenwoodTS890S, "Kenwood", "TS-890S"),
        (RigModelType::ElecraftK3S, "Elecraft", "K3S"),
        (RigModelType::ElecraftKX3, "Elecraft", "KX3"),
        (RigModelType::FlexRadio6000, "FlexRadio", "6000 Series"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::*;

    #[test]
    fn test_library_initialization() {
        // Should not panic
        init();
        init(); // Second call should be safe
    }

    #[test]
    fn test_version() {
        let version = version();
        assert!(!version.is_empty());
        assert!(version.chars().any(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_supported_models() {
        let models = supported_models();
        assert!(!models.is_empty());

        // Should include dummy model
        assert!(models
            .iter()
            .any(|(model, _, _)| *model == RigModelType::Dummy));
    }

    #[test]
    fn test_frequency_parsing() {
        assert_eq!(parse_frequency("14.200 MHz").unwrap(), 14_200_000);
        assert_eq!(parse_frequency("14200000").unwrap(), 14_200_000);
        assert_eq!(parse_frequency("14200 kHz").unwrap(), 14_200_000);
        assert_eq!(parse_frequency("1.42 GHz").unwrap(), 1_420_000_000);
    }

    #[test]
    fn test_frequency_formatting() {
        assert_eq!(format_frequency(14_200_000), "14.200 MHz");
        assert_eq!(format_frequency(1_420_000_000), "1.420 GHz");
        assert_eq!(format_frequency(14_200), "14.2 kHz");
        assert_eq!(format_frequency(142), "142 Hz");
    }

    #[test]
    fn test_s_meter_formatting() {
        assert_eq!(format_s_meter(-93), "S1");
        assert_eq!(format_s_meter(-87), "S2");
        assert_eq!(format_s_meter(-73), "S4");
        assert_eq!(format_s_meter(-53), "S8");
        assert_eq!(format_s_meter(-47), "S9");
        assert_eq!(format_s_meter(-37), "S9+10");
    }

    #[test]
    fn test_amateur_frequency_validation() {
        assert!(is_amateur_frequency(14_200_000)); // 20m
        assert!(is_amateur_frequency(7_100_000)); // 40m
        assert!(is_amateur_frequency(144_500_000)); // 2m
        assert!(!is_amateur_frequency(100_000_000)); // Broadcast FM
        assert!(!is_amateur_frequency(500_000)); // Below amateur bands
    }

    #[test]
    fn test_band_utilities() {
        assert_eq!(frequency_to_band(14_200_000), Some(Band::Band20m));
        assert_eq!(band_default_mode(Band::Band20m), Mode::USB);
        assert_eq!(band_default_mode(Band::Band40m), Mode::LSB);
        assert_eq!(band_default_mode(Band::Band2m), Mode::FM);
    }

    #[tokio::test]
    async fn test_rig_builder() {
        let result = RigBuilder::new()
            .model(RigModelType::Dummy)
            .device_path("/dev/null")
            .baud_rate(9600)
            .timeout_ms(1000)
            .build()
            .await;

        assert!(result.is_ok());
    }

    #[cfg(feature = "mock-rig")]
    #[tokio::test]
    async fn test_mock_rig_builder() {
        let result = RigBuilder::new().build_mock().await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_band_common_frequencies() {
        let freqs = band_common_frequencies(Band::Band20m);
        assert!(!freqs.is_empty());
        assert!(freqs.iter().any(|(freq, _)| *freq == 14_074_000)); // FT8
        assert!(freqs.iter().any(|(_, name)| *name == "Phone"));
    }
}
