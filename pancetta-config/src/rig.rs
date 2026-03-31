//! Rig control configuration module
//!
//! This module handles transceiver control settings including CAT (Computer Aided Transceiver)
//! interface, PTT (Push-To-Talk) control, frequency management, and rig-specific parameters.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Rig control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigConfig {
    /// Transceiver model/manufacturer
    pub model: String,

    /// CAT interface configuration
    pub interface: CatInterfaceConfig,

    /// PTT control configuration
    pub ptt: PttConfig,

    /// Frequency management settings
    pub frequency: FrequencyConfig,

    /// Band switching configuration
    pub band_switching: BandSwitchingConfig,

    /// Antenna switching configuration
    pub antenna_switching: AntennaSwitchingConfig,

    /// Power control settings
    pub power_control: PowerControlConfig,

    /// Mode and filter settings
    pub modes: ModeConfig,

    /// Timing and polling configuration
    pub timing: TimingConfig,

    /// Rig-specific parameters
    pub rig_parameters: RigParametersConfig,

    /// Custom commands and macros
    #[serde(default)]
    pub custom_commands: HashMap<String, String>,
}

/// CAT (Computer Aided Transceiver) interface configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatInterfaceConfig {
    /// Serial port device (e.g., "/dev/ttyUSB0", "COM3")
    pub port: String,

    /// Baud rate
    pub baud_rate: u32,

    /// Data bits (5, 6, 7, 8)
    pub data_bits: u8,

    /// Stop bits (1, 2)
    pub stop_bits: StopBits,

    /// Parity setting
    pub parity: Parity,

    /// Flow control
    pub flow_control: FlowControl,

    /// Connection timeout in milliseconds
    pub timeout_ms: u64,

    /// Enable CAT control
    pub enabled: bool,

    /// CAT protocol type
    pub protocol: CatProtocol,

    /// Command termination character
    pub termination: String,

    /// Response timeout in milliseconds
    pub response_timeout_ms: u64,

    /// Retry count for failed commands
    pub retry_count: u8,
}

/// Serial port stop bits configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopBits {
    One,
    Two,
}

/// Serial port parity configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

/// Serial port flow control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    None,
    Hardware,
    Software,
}

/// CAT protocol types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatProtocol {
    /// Hamlib generic protocol
    Hamlib,

    /// Yaesu CAT protocol
    Yaesu,

    /// Icom CI-V protocol
    Icom,

    /// Kenwood protocol
    Kenwood,

    /// Elecraft protocol
    Elecraft,

    /// FlexRadio protocol
    FlexRadio,

    /// Custom protocol implementation
    Custom,
}

/// PTT (Push-To-Talk) control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PttConfig {
    /// PTT control method
    pub method: PttMethod,

    /// PTT device/port for hardware control
    pub device: Option<String>,

    /// PTT signal polarity
    pub polarity: PttPolarity,

    /// PTT delay before transmission (milliseconds)
    pub tx_delay_ms: u64,

    /// PTT delay after transmission (milliseconds)
    pub tx_tail_ms: u64,

    /// VOX (Voice Operated eXchange) settings
    pub vox: VoxConfig,

    /// PTT timeout in seconds (safety feature)
    pub timeout_seconds: u64,
}

/// PTT control methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PttMethod {
    /// No PTT control
    None,

    /// CAT command PTT
    Cat,

    /// Serial port RTS/DTR
    Serial,

    /// Parallel port
    Parallel,

    /// USB device
    Usb,

    /// Sound card PTT
    SoundCard,

    /// VOX only
    Vox,

    /// Network PTT (for remote rigs)
    Network,
}

/// PTT signal polarity
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PttPolarity {
    /// Active high (positive logic)
    Positive,

    /// Active low (negative logic)
    Negative,
}

/// VOX (Voice Operated eXchange) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoxConfig {
    /// Enable VOX
    pub enabled: bool,

    /// VOX gain/sensitivity (0.0 to 1.0)
    pub gain: f32,

    /// VOX delay in milliseconds
    pub delay_ms: u64,

    /// Anti-VOX (prevents receiver audio from triggering VOX)
    pub anti_vox: bool,

    /// VOX threshold level in dB
    pub threshold_db: f32,
}

/// Frequency management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyConfig {
    /// Enable frequency control
    pub control_enabled: bool,

    /// Automatically follow rig frequency
    pub follow_rig: bool,

    /// Frequency polling interval in milliseconds
    pub polling_interval_ms: u64,

    /// Memory channel management
    pub memory_channels: MemoryChannelConfig,

    /// Frequency limits and ranges
    pub limits: FrequencyLimitsConfig,

    /// Band plan configuration
    pub band_plan: BandPlanConfig,
}

/// Memory channel configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MemoryChannelConfig {
    /// Enable memory channel control
    pub enabled: bool,

    /// Automatically save current frequency to memory
    pub auto_save: bool,

    /// Memory channel database file
    pub database_file: Option<String>,

    /// Quick memory slots (1-10)
    pub quick_memories: Vec<MemoryChannel>,
}

/// Individual memory channel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChannel {
    /// Channel number
    pub channel: u16,

    /// Frequency in Hz
    pub frequency: u64,

    /// Operating mode
    pub mode: String,

    /// Channel name/description
    pub name: String,

    /// Bandwidth/filter setting
    pub bandwidth: Option<u32>,

    /// Power level for this channel
    pub power: Option<u8>,

    /// Antenna selection
    pub antenna: Option<u8>,
}

/// Frequency limits configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyLimitsConfig {
    /// Minimum frequency in Hz
    pub min_frequency: u64,

    /// Maximum frequency in Hz
    pub max_frequency: u64,

    /// Per-band frequency limits
    pub band_limits: HashMap<String, FrequencyRange>,
}

/// Frequency range definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyRange {
    /// Start frequency in Hz
    pub start: u64,

    /// End frequency in Hz
    pub end: u64,

    /// Allowed modes for this range
    pub modes: Vec<String>,
}

/// Band plan configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandPlanConfig {
    /// Region-specific band plan (ITU Region 1, 2, or 3)
    pub region: u8,

    /// Custom band definitions
    pub custom_bands: HashMap<String, BandDefinition>,

    /// Band edge warnings
    pub edge_warnings: bool,
}

/// Band definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandDefinition {
    /// Band name (e.g., "40m", "20m")
    pub name: String,

    /// Frequency ranges for this band
    pub ranges: Vec<FrequencyRange>,

    /// Default mode for this band
    pub default_mode: String,

    /// Band type (HF, VHF, UHF, etc.)
    pub band_type: BandType,
}

/// Band type classification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum BandType {
    /// Low Frequency (30-300 kHz)
    Lf,

    /// Medium Frequency (300 kHz - 3 MHz)
    Mf,

    /// High Frequency (3-30 MHz)
    Hf,

    /// Very High Frequency (30-300 MHz)
    Vhf,

    /// Ultra High Frequency (300 MHz - 3 GHz)
    Uhf,

    /// Super High Frequency (3-30 GHz)
    Shf,

    /// Extremely High Frequency (30-300 GHz)
    Ehf,
}

/// Band switching configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandSwitchingConfig {
    /// Enable automatic band switching
    pub auto_switching: bool,

    /// Band switching method
    pub method: BandSwitchMethod,

    /// Band switching device/interface
    pub device: Option<String>,

    /// Band switching delay in milliseconds
    pub switching_delay_ms: u64,

    /// Band-to-output mapping
    pub band_outputs: HashMap<String, u8>,
}

/// Band switching methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandSwitchMethod {
    /// No band switching
    None,

    /// Serial port control
    Serial,

    /// Parallel port control
    Parallel,

    /// USB device control
    Usb,

    /// Network control
    Network,

    /// CAT command
    Cat,
}

/// Antenna switching configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntennaSwitchingConfig {
    /// Enable antenna switching
    pub enabled: bool,

    /// Antenna switching method
    pub method: AntennaSwitchMethod,

    /// Antenna switching device
    pub device: Option<String>,

    /// Antenna switching delay in milliseconds
    pub switching_delay_ms: u64,

    /// Antenna definitions
    pub antennas: Vec<AntennaDefinition>,

    /// Auto-switching rules
    pub auto_rules: Vec<AntennaRule>,
}

/// Antenna switching methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AntennaSwitchMethod {
    /// No antenna switching
    None,

    /// Manual selection only
    Manual,

    /// Serial port control
    Serial,

    /// Network control
    Network,

    /// CAT command
    Cat,
}

/// Antenna definition for switching
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntennaDefinition {
    /// Antenna ID
    pub id: u8,

    /// Antenna name
    pub name: String,

    /// Supported bands
    pub bands: Vec<String>,

    /// Antenna type
    pub antenna_type: String,

    /// Control signal/address
    pub control_address: u8,
}

/// Antenna switching rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntennaRule {
    /// Rule name
    pub name: String,

    /// Frequency range for this rule
    pub frequency_range: FrequencyRange,

    /// Preferred antenna ID
    pub antenna_id: u8,

    /// Rule priority (higher = more important)
    pub priority: u8,

    /// Rule enabled
    pub enabled: bool,
}

/// Power control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerControlConfig {
    /// Enable power control
    pub enabled: bool,

    /// Power control method
    pub method: PowerControlMethod,

    /// Default power level (0-100%)
    pub default_level: u8,

    /// Per-band power settings
    pub band_power: HashMap<String, u8>,

    /// Power ramping settings
    pub ramping: PowerRampingConfig,

    /// Power protection settings
    pub protection: PowerProtectionConfig,
}

/// Power control methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerControlMethod {
    /// No power control
    None,

    /// CAT command control
    Cat,

    /// Manual control only
    Manual,

    /// Automatic power control
    Automatic,
}

/// Power ramping configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerRampingConfig {
    /// Enable power ramping
    pub enabled: bool,

    /// Ramp up time in milliseconds
    pub ramp_up_ms: u64,

    /// Ramp down time in milliseconds
    pub ramp_down_ms: u64,

    /// Ramping curve (linear, exponential, etc.)
    pub curve: RampingCurve,
}

/// Power ramping curve types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RampingCurve {
    Linear,
    Exponential,
    Logarithmic,
    Smooth,
}

/// Power protection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerProtectionConfig {
    /// Maximum power limit (watts)
    pub max_power_watts: u16,

    /// Enable SWR protection
    pub swr_protection: bool,

    /// Maximum SWR threshold
    pub max_swr: f32,

    /// Temperature protection
    pub temperature_protection: bool,

    /// Maximum temperature (Celsius)
    pub max_temperature_c: f32,
}

/// Operating mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Supported modes
    pub supported_modes: Vec<String>,

    /// Default mode
    pub default_mode: String,

    /// Mode-specific settings
    pub mode_settings: HashMap<String, ModeSettings>,

    /// Filter settings
    pub filters: FilterConfig,
}

/// Individual mode settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeSettings {
    /// Mode bandwidth in Hz
    pub bandwidth: u32,

    /// Default power level for this mode
    pub power_level: u8,

    /// AGC setting for this mode
    pub agc_mode: String,

    /// Noise blanker settings
    pub noise_blanker: bool,

    /// Mode-specific parameters
    pub parameters: HashMap<String, String>,
}

/// Filter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Available filter widths
    pub available_widths: Vec<u32>,

    /// Default filter for each mode
    pub mode_defaults: HashMap<String, u32>,

    /// Custom filter definitions
    pub custom_filters: Vec<CustomFilter>,
}

/// Custom filter definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomFilter {
    /// Filter name
    pub name: String,

    /// Filter bandwidth in Hz
    pub bandwidth: u32,

    /// Filter shape factor
    pub shape_factor: f32,

    /// Filter type (roofing, IF, etc.)
    pub filter_type: String,
}

/// Timing configuration for rig operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingConfig {
    /// Command interval in milliseconds
    pub command_interval_ms: u64,

    /// Status polling interval in milliseconds
    pub status_polling_ms: u64,

    /// Connection retry interval in milliseconds
    pub retry_interval_ms: u64,

    /// Keep-alive interval in milliseconds
    pub keepalive_interval_ms: u64,

    /// Rig response timeout in milliseconds
    pub response_timeout_ms: u64,
}

/// Rig-specific parameters
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RigParametersConfig {
    /// Rig identification string
    pub rig_id: Option<String>,

    /// Firmware version
    pub firmware_version: Option<String>,

    /// Extended features supported
    pub extended_features: Vec<String>,

    /// Calibration data
    pub calibration: CalibrationConfig,

    /// Rig-specific quirks and workarounds
    pub quirks: QuirksConfig,
}

/// Calibration configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CalibrationConfig {
    /// Frequency calibration offset in Hz
    pub frequency_offset: i32,

    /// Power meter calibration
    pub power_calibration: Vec<CalibrationPoint>,

    /// S-meter calibration
    pub s_meter_calibration: Vec<CalibrationPoint>,

    /// SWR meter calibration
    pub swr_calibration: Vec<CalibrationPoint>,
}

/// Calibration point for meters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationPoint {
    /// Raw value from rig
    pub raw_value: u16,

    /// Calibrated value
    pub calibrated_value: f32,

    /// Unit of measurement
    pub unit: String,
}

/// Rig quirks and workarounds
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct QuirksConfig {
    /// Commands that need special handling
    pub special_commands: HashMap<String, String>,

    /// Known issues and workarounds
    pub workarounds: Vec<String>,

    /// Timing adjustments for specific operations
    pub timing_adjustments: HashMap<String, u64>,
}

impl Default for RigConfig {
    fn default() -> Self {
        Self {
            model: "Generic".to_string(),
            interface: CatInterfaceConfig::default(),
            ptt: PttConfig::default(),
            frequency: FrequencyConfig::default(),
            band_switching: BandSwitchingConfig::default(),
            antenna_switching: AntennaSwitchingConfig::default(),
            power_control: PowerControlConfig::default(),
            modes: ModeConfig::default(),
            timing: TimingConfig::default(),
            rig_parameters: RigParametersConfig::default(),
            custom_commands: HashMap::new(),
        }
    }
}

impl Default for CatInterfaceConfig {
    fn default() -> Self {
        Self {
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 9600,
            data_bits: 8,
            stop_bits: StopBits::One,
            parity: Parity::None,
            flow_control: FlowControl::None,
            timeout_ms: 1000,
            enabled: false,
            protocol: CatProtocol::Hamlib,
            termination: "\n".to_string(),
            response_timeout_ms: 500,
            retry_count: 3,
        }
    }
}

impl Default for PttConfig {
    fn default() -> Self {
        Self {
            method: PttMethod::None,
            device: None,
            polarity: PttPolarity::Positive,
            tx_delay_ms: 100,
            tx_tail_ms: 100,
            vox: VoxConfig::default(),
            timeout_seconds: 300, // 5 minutes safety timeout
        }
    }
}

impl Default for VoxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gain: 0.5,
            delay_ms: 500,
            anti_vox: true,
            threshold_db: -30.0,
        }
    }
}

impl Default for FrequencyConfig {
    fn default() -> Self {
        Self {
            control_enabled: false,
            follow_rig: true,
            polling_interval_ms: 1000,
            memory_channels: MemoryChannelConfig::default(),
            limits: FrequencyLimitsConfig::default(),
            band_plan: BandPlanConfig::default(),
        }
    }
}

impl Default for FrequencyLimitsConfig {
    fn default() -> Self {
        Self {
            min_frequency: 1_800_000,   // 1.8 MHz
            max_frequency: 440_000_000, // 440 MHz
            band_limits: HashMap::new(),
        }
    }
}

impl Default for BandPlanConfig {
    fn default() -> Self {
        Self {
            region: 2, // ITU Region 2 (Americas)
            custom_bands: HashMap::new(),
            edge_warnings: true,
        }
    }
}

impl Default for BandSwitchingConfig {
    fn default() -> Self {
        Self {
            auto_switching: false,
            method: BandSwitchMethod::None,
            device: None,
            switching_delay_ms: 100,
            band_outputs: HashMap::new(),
        }
    }
}

impl Default for AntennaSwitchingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: AntennaSwitchMethod::None,
            device: None,
            switching_delay_ms: 100,
            antennas: vec![],
            auto_rules: vec![],
        }
    }
}

impl Default for PowerControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: PowerControlMethod::None,
            default_level: 50,
            band_power: HashMap::new(),
            ramping: PowerRampingConfig::default(),
            protection: PowerProtectionConfig::default(),
        }
    }
}

impl Default for PowerRampingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ramp_up_ms: 1000,
            ramp_down_ms: 500,
            curve: RampingCurve::Linear,
        }
    }
}

impl Default for PowerProtectionConfig {
    fn default() -> Self {
        Self {
            max_power_watts: 100,
            swr_protection: true,
            max_swr: 3.0,
            temperature_protection: true,
            max_temperature_c: 80.0,
        }
    }
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            supported_modes: vec![
                "LSB".to_string(),
                "USB".to_string(),
                "CW".to_string(),
                "FM".to_string(),
                "AM".to_string(),
                "RTTY".to_string(),
                "PSK31".to_string(),
            ],
            default_mode: "USB".to_string(),
            mode_settings: HashMap::new(),
            filters: FilterConfig::default(),
        }
    }
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            available_widths: vec![500, 1000, 1500, 2400, 3000],
            mode_defaults: HashMap::new(),
            custom_filters: vec![],
        }
    }
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            command_interval_ms: 100,
            status_polling_ms: 1000,
            retry_interval_ms: 1000,
            keepalive_interval_ms: 30000,
            response_timeout_ms: 500,
        }
    }
}

impl ConfigSection for RigConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // Validate CAT interface settings
        if self.interface.enabled {
            if self.interface.baud_rate == 0 {
                return Err(ConfigError::InvalidValue {
                    field: "interface.baud_rate".to_string(),
                    value: self.interface.baud_rate.to_string(),
                });
            }

            if ![5, 6, 7, 8].contains(&self.interface.data_bits) {
                return Err(ConfigError::InvalidValue {
                    field: "interface.data_bits".to_string(),
                    value: self.interface.data_bits.to_string(),
                });
            }
        }

        // Validate PTT timeout
        if self.ptt.timeout_seconds == 0 {
            return Err(ConfigError::InvalidValue {
                field: "ptt.timeout_seconds".to_string(),
                value: self.ptt.timeout_seconds.to_string(),
            });
        }

        // Validate power settings
        if self.power_control.default_level > 100 {
            return Err(ConfigError::InvalidValue {
                field: "power_control.default_level".to_string(),
                value: self.power_control.default_level.to_string(),
            });
        }

        // Validate frequency limits
        if self.frequency.limits.min_frequency >= self.frequency.limits.max_frequency {
            return Err(ConfigError::InvalidValue {
                field: "frequency.limits".to_string(),
                value: format!(
                    "min >= max ({} >= {})",
                    self.frequency.limits.min_frequency, self.frequency.limits.max_frequency
                ),
            });
        }

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // Merge non-default values
        if other.model != "Generic" {
            self.model = other.model;
        }

        // Merge complex configurations
        self.interface.merge_with(other.interface);
        self.ptt.merge_with(other.ptt);
        self.frequency.merge_with(other.frequency);
        self.band_switching.merge_with(other.band_switching);
        self.antenna_switching.merge_with(other.antenna_switching);
        self.power_control.merge_with(other.power_control);
        self.modes.merge_with(other.modes);
        self.timing.merge_with(other.timing);
        self.rig_parameters.merge_with(other.rig_parameters);

        // Merge custom commands
        self.custom_commands.extend(other.custom_commands);
    }
}

// Implement merge methods for nested configurations
impl CatInterfaceConfig {
    fn merge_with(&mut self, other: Self) {
        if other.port != "/dev/ttyUSB0" {
            self.port = other.port;
        }
        if other.baud_rate != 9600 {
            self.baud_rate = other.baud_rate;
        }
        if other.enabled {
            self.enabled = other.enabled;
        }
        // Continue for other fields...
    }
}

impl PttConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl FrequencyConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl BandSwitchingConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl AntennaSwitchingConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl PowerControlConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl ModeConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl TimingConfig {
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

impl RigParametersConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for rig parameters config merging
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_rig_config() {
        let config = RigConfig::default();
        assert_eq!(config.model, "Generic");
        assert!(!config.interface.enabled);
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_cat_interface_validation() {
        let mut config = RigConfig::default();
        config.interface.enabled = true;

        // Valid configuration
        assert!(config.validate_section().is_ok());

        // Invalid data bits
        config.interface.data_bits = 9;
        assert!(config.validate_section().is_err());

        // Invalid baud rate
        config.interface.data_bits = 8; // Reset to valid
        config.interface.baud_rate = 0;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_power_control_validation() {
        let mut config = RigConfig::default();

        // Valid power level
        config.power_control.default_level = 50;
        assert!(config.validate_section().is_ok());

        // Invalid power level
        config.power_control.default_level = 150;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_frequency_limits_validation() {
        let mut config = RigConfig::default();

        // Valid frequency limits
        assert!(config.validate_section().is_ok());

        // Invalid frequency limits (min >= max)
        config.frequency.limits.min_frequency = 50_000_000;
        config.frequency.limits.max_frequency = 30_000_000;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_memory_channel() {
        let channel = MemoryChannel {
            channel: 1,
            frequency: 14_205_000,
            mode: "USB".to_string(),
            name: "20m PSK".to_string(),
            bandwidth: Some(2400),
            power: Some(50),
            antenna: Some(1),
        };

        assert_eq!(channel.frequency, 14_205_000);
        assert_eq!(channel.mode, "USB");
    }
}
