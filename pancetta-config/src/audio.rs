//! Audio configuration module
//!
//! This module handles audio device selection, sample rates, buffer sizes,
//! and audio processing parameters for real-time amateur radio applications.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Audio system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Input audio device name or ID
    pub input_device: String,
    
    /// Output audio device name or ID
    pub output_device: String,
    
    /// Sample rate in Hz (e.g., 44100, 48000)
    pub sample_rate: u32,
    
    /// Buffer size in samples (affects latency)
    pub buffer_size: u32,
    
    /// Number of input channels
    pub input_channels: u8,
    
    /// Number of output channels
    pub output_channels: u8,
    
    /// Audio format bit depth
    pub bit_depth: BitDepth,
    
    /// Audio processing chain configuration
    pub processing: AudioProcessingConfig,
    
    /// AGC (Automatic Gain Control) settings
    pub agc: AgcConfig,
    
    /// Noise reduction settings
    pub noise_reduction: NoiseReductionConfig,
    
    /// Audio routing configuration
    pub routing: AudioRoutingConfig,
    
    /// Recording settings
    pub recording: RecordingConfig,
    
    /// Monitoring and level settings
    pub levels: AudioLevelsConfig,
    
    /// Custom audio device parameters
    #[serde(default)]
    pub device_parameters: HashMap<String, String>,
}

/// Audio bit depth options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitDepth {
    /// 16-bit signed integer
    Int16,
    
    /// 24-bit signed integer
    Int24,
    
    /// 32-bit signed integer
    Int32,
    
    /// 32-bit floating point
    Float32,
    
    /// 64-bit floating point
    Float64,
}

/// Audio processing chain configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProcessingConfig {
    /// Enable/disable audio processing
    pub enabled: bool,
    
    /// Low-pass filter cutoff frequency in Hz
    pub lowpass_cutoff: Option<f32>,
    
    /// High-pass filter cutoff frequency in Hz
    pub highpass_cutoff: Option<f32>,
    
    /// Bandwidth filter settings
    pub bandwidth: BandwidthConfig,
    
    /// Audio compression settings
    pub compression: CompressionConfig,
    
    /// Equalizer settings
    pub equalizer: EqualizerConfig,
    
    /// Pre-emphasis/de-emphasis settings
    pub emphasis: EmphasisConfig,
}

/// Bandwidth filter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandwidthConfig {
    /// Enable bandwidth filtering
    pub enabled: bool,
    
    /// Filter type (butterworth, chebyshev, etc.)
    pub filter_type: String,
    
    /// Filter order
    pub order: u8,
    
    /// Center frequency in Hz
    pub center_frequency: f32,
    
    /// Bandwidth in Hz
    pub bandwidth_hz: f32,
}

/// Audio compression configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    /// Enable audio compression
    pub enabled: bool,
    
    /// Compression ratio (e.g., 4.0 for 4:1)
    pub ratio: f32,
    
    /// Threshold in dB
    pub threshold_db: f32,
    
    /// Attack time in milliseconds
    pub attack_ms: f32,
    
    /// Release time in milliseconds
    pub release_ms: f32,
    
    /// Knee width in dB (0 = hard knee)
    pub knee_width_db: f32,
    
    /// Makeup gain in dB
    pub makeup_gain_db: f32,
}

/// Equalizer configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqualizerConfig {
    /// Enable equalizer
    pub enabled: bool,
    
    /// EQ bands configuration
    pub bands: Vec<EqBand>,
    
    /// Global EQ gain in dB
    pub global_gain_db: f32,
}

/// Individual equalizer band
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBand {
    /// Center frequency in Hz
    pub frequency: f32,
    
    /// Gain in dB
    pub gain_db: f32,
    
    /// Q factor (bandwidth)
    pub q_factor: f32,
    
    /// Band type (peak, shelf, etc.)
    pub band_type: EqBandType,
}

/// Equalizer band types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EqBandType {
    /// Peaking/bell filter
    Peak,
    
    /// Low shelf filter
    LowShelf,
    
    /// High shelf filter
    HighShelf,
    
    /// Low pass filter
    LowPass,
    
    /// High pass filter
    HighPass,
    
    /// Notch filter
    Notch,
}

/// Pre-emphasis/de-emphasis configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmphasisConfig {
    /// Enable pre-emphasis on transmission
    pub pre_emphasis_enabled: bool,
    
    /// Enable de-emphasis on reception
    pub de_emphasis_enabled: bool,
    
    /// Time constant in microseconds (75us for FM, 50us for some systems)
    pub time_constant_us: f32,
}

/// AGC (Automatic Gain Control) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgcConfig {
    /// Enable AGC
    pub enabled: bool,
    
    /// AGC mode
    pub mode: AgcMode,
    
    /// Target level in dB
    pub target_level_db: f32,
    
    /// Maximum gain in dB
    pub max_gain_db: f32,
    
    /// Attack time in milliseconds
    pub attack_time_ms: f32,
    
    /// Decay time in milliseconds
    pub decay_time_ms: f32,
    
    /// Hang time in milliseconds
    pub hang_time_ms: f32,
    
    /// Threshold in dB
    pub threshold_db: f32,
}

/// AGC modes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgcMode {
    /// Fast AGC response
    Fast,
    
    /// Medium AGC response
    Medium,
    
    /// Slow AGC response
    Slow,
    
    /// Manual gain control
    Manual,
    
    /// Peak AGC
    Peak,
    
    /// RMS AGC
    Rms,
}

/// Noise reduction configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseReductionConfig {
    /// Enable noise reduction
    pub enabled: bool,
    
    /// Noise reduction algorithm
    pub algorithm: NoiseReductionAlgorithm,
    
    /// Noise reduction strength (0.0 to 1.0)
    pub strength: f32,
    
    /// Noise gate threshold in dB
    pub noise_gate_threshold_db: f32,
    
    /// Spectral subtraction parameters
    pub spectral_subtraction: SpectralSubtractionConfig,
    
    /// Wiener filter parameters
    pub wiener_filter: WienerFilterConfig,
}

/// Noise reduction algorithms
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseReductionAlgorithm {
    /// Simple noise gate
    NoiseGate,
    
    /// Spectral subtraction
    SpectralSubtraction,
    
    /// Wiener filtering
    WienerFilter,
    
    /// Adaptive noise reduction
    Adaptive,
    
    /// Multi-band noise reduction
    MultiBand,
}

/// Spectral subtraction configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralSubtractionConfig {
    /// Over-subtraction factor
    pub alpha: f32,
    
    /// Spectral floor factor
    pub beta: f32,
    
    /// Frame size for FFT
    pub frame_size: u32,
    
    /// Frame overlap ratio
    pub overlap: f32,
}

/// Wiener filter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WienerFilterConfig {
    /// Noise estimation window size
    pub estimation_window_ms: f32,
    
    /// Smoothing factor
    pub smoothing_factor: f32,
    
    /// Minimum gain floor
    pub min_gain: f32,
}

/// Audio routing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRoutingConfig {
    /// Input routing matrix
    pub input_routing: Vec<AudioRoute>,
    
    /// Output routing matrix
    pub output_routing: Vec<AudioRoute>,
    
    /// Monitor routing
    pub monitor_routing: MonitorConfig,
    
    /// Sidetone configuration
    pub sidetone: SidetoneConfig,
}

/// Audio routing definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRoute {
    /// Source channel/device
    pub source: String,
    
    /// Destination channel/device
    pub destination: String,
    
    /// Gain in dB
    pub gain_db: f32,
    
    /// Enable/disable this route
    pub enabled: bool,
}

/// Monitor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Enable monitoring
    pub enabled: bool,
    
    /// Monitor level (0.0 to 1.0)
    pub level: f32,
    
    /// Monitor source
    pub source: MonitorSource,
    
    /// Monitor destination
    pub destination: String,
}

/// Monitor source options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorSource {
    /// Monitor input signal
    Input,
    
    /// Monitor processed signal
    Processed,
    
    /// Monitor output signal
    Output,
    
    /// Monitor transmit signal
    Transmit,
}

/// Sidetone configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidetoneConfig {
    /// Enable sidetone
    pub enabled: bool,
    
    /// Sidetone frequency in Hz
    pub frequency: f32,
    
    /// Sidetone level (0.0 to 1.0)
    pub level: f32,
    
    /// Sidetone shape (sine, square, etc.)
    pub shape: SidetoneShape,
}

/// Sidetone waveform shapes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidetoneShape {
    /// Pure sine wave
    Sine,
    
    /// Square wave
    Square,
    
    /// Sawtooth wave
    Sawtooth,
    
    /// Triangle wave
    Triangle,
}

/// Audio recording configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Enable automatic recording
    pub auto_record: bool,
    
    /// Recording format
    pub format: RecordingFormat,
    
    /// Recording bit depth
    pub bit_depth: BitDepth,
    
    /// Recording sample rate
    pub sample_rate: u32,
    
    /// Maximum recording length in minutes
    pub max_length_minutes: u32,
    
    /// Recording directory
    pub directory: String,
    
    /// File naming pattern
    pub filename_pattern: String,
    
    /// Compression level (0-9 for lossless formats)
    pub compression_level: u8,
}

/// Recording format options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingFormat {
    /// WAV format
    Wav,
    
    /// FLAC format
    Flac,
    
    /// OGG Vorbis format
    Ogg,
    
    /// MP3 format
    Mp3,
    
    /// Raw PCM data
    Raw,
}

/// Audio levels and metering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioLevelsConfig {
    /// Input level adjustment in dB
    pub input_gain_db: f32,
    
    /// Output level adjustment in dB
    pub output_gain_db: f32,
    
    /// Peak level warning threshold in dB
    pub peak_warning_db: f32,
    
    /// Peak level clipping threshold in dB
    pub peak_clip_db: f32,
    
    /// RMS level target in dB
    pub rms_target_db: f32,
    
    /// Meter ballistics configuration
    pub meter_ballistics: MeterBallisticsConfig,
}

/// Audio meter ballistics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeterBallisticsConfig {
    /// Peak meter attack time in milliseconds
    pub peak_attack_ms: f32,
    
    /// Peak meter decay time in milliseconds
    pub peak_decay_ms: f32,
    
    /// RMS meter averaging time in milliseconds
    pub rms_averaging_ms: f32,
    
    /// Hold time for peak values in milliseconds
    pub hold_time_ms: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: "default".to_string(),
            output_device: "default".to_string(),
            sample_rate: 48000,
            buffer_size: 512,
            input_channels: 2,
            output_channels: 2,
            bit_depth: BitDepth::Float32,
            processing: AudioProcessingConfig::default(),
            agc: AgcConfig::default(),
            noise_reduction: NoiseReductionConfig::default(),
            routing: AudioRoutingConfig::default(),
            recording: RecordingConfig::default(),
            levels: AudioLevelsConfig::default(),
            device_parameters: HashMap::new(),
        }
    }
}

impl Default for AudioProcessingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lowpass_cutoff: Some(3000.0),
            highpass_cutoff: Some(300.0),
            bandwidth: BandwidthConfig::default(),
            compression: CompressionConfig::default(),
            equalizer: EqualizerConfig::default(),
            emphasis: EmphasisConfig::default(),
        }
    }
}

impl Default for BandwidthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            filter_type: "butterworth".to_string(),
            order: 6,
            center_frequency: 1500.0,
            bandwidth_hz: 2700.0,
        }
    }
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ratio: 3.0,
            threshold_db: -20.0,
            attack_ms: 1.0,
            release_ms: 100.0,
            knee_width_db: 2.0,
            makeup_gain_db: 0.0,
        }
    }
}

impl Default for EqualizerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bands: vec![
                EqBand {
                    frequency: 300.0,
                    gain_db: 0.0,
                    q_factor: 1.0,
                    band_type: EqBandType::HighPass,
                },
                EqBand {
                    frequency: 3000.0,
                    gain_db: 0.0,
                    q_factor: 1.0,
                    band_type: EqBandType::LowPass,
                },
            ],
            global_gain_db: 0.0,
        }
    }
}

impl Default for EmphasisConfig {
    fn default() -> Self {
        Self {
            pre_emphasis_enabled: false,
            de_emphasis_enabled: false,
            time_constant_us: 75.0,
        }
    }
}

impl Default for AgcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: AgcMode::Medium,
            target_level_db: -20.0,
            max_gain_db: 30.0,
            attack_time_ms: 1.0,
            decay_time_ms: 500.0,
            hang_time_ms: 200.0,
            threshold_db: -60.0,
        }
    }
}

impl Default for NoiseReductionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithm: NoiseReductionAlgorithm::NoiseGate,
            strength: 0.5,
            noise_gate_threshold_db: -50.0,
            spectral_subtraction: SpectralSubtractionConfig::default(),
            wiener_filter: WienerFilterConfig::default(),
        }
    }
}

impl Default for SpectralSubtractionConfig {
    fn default() -> Self {
        Self {
            alpha: 2.0,
            beta: 0.01,
            frame_size: 1024,
            overlap: 0.5,
        }
    }
}

impl Default for WienerFilterConfig {
    fn default() -> Self {
        Self {
            estimation_window_ms: 500.0,
            smoothing_factor: 0.98,
            min_gain: 0.1,
        }
    }
}

impl Default for AudioRoutingConfig {
    fn default() -> Self {
        Self {
            input_routing: vec![],
            output_routing: vec![],
            monitor_routing: MonitorConfig::default(),
            sidetone: SidetoneConfig::default(),
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: 0.5,
            source: MonitorSource::Processed,
            destination: "default".to_string(),
        }
    }
}

impl Default for SidetoneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            frequency: 600.0,
            level: 0.3,
            shape: SidetoneShape::Sine,
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            auto_record: false,
            format: RecordingFormat::Wav,
            bit_depth: BitDepth::Int16,
            sample_rate: 44100,
            max_length_minutes: 60,
            directory: "~/Documents/Pancetta/Recordings".to_string(),
            filename_pattern: "pancetta_%Y%m%d_%H%M%S".to_string(),
            compression_level: 5,
        }
    }
}

impl Default for AudioLevelsConfig {
    fn default() -> Self {
        Self {
            input_gain_db: 0.0,
            output_gain_db: 0.0,
            peak_warning_db: -6.0,
            peak_clip_db: -1.0,
            rms_target_db: -20.0,
            meter_ballistics: MeterBallisticsConfig::default(),
        }
    }
}

impl Default for MeterBallisticsConfig {
    fn default() -> Self {
        Self {
            peak_attack_ms: 0.0,
            peak_decay_ms: 1600.0,
            rms_averaging_ms: 300.0,
            hold_time_ms: 2000.0,
        }
    }
}

impl ConfigSection for AudioConfig {
    fn validate(&self) -> ConfigResult<()> {
        // Validate sample rate
        if ![8000, 11025, 16000, 22050, 44100, 48000, 88200, 96000, 176400, 192000]
            .contains(&self.sample_rate) {
            return Err(ConfigError::InvalidValue {
                field: "sample_rate".to_string(),
                value: self.sample_rate.to_string(),
            });
        }
        
        // Validate buffer size (must be power of 2)
        if !self.buffer_size.is_power_of_two() || self.buffer_size < 32 || self.buffer_size > 8192 {
            return Err(ConfigError::InvalidValue {
                field: "buffer_size".to_string(),
                value: self.buffer_size.to_string(),
            });
        }
        
        // Validate channel counts
        if self.input_channels == 0 || self.input_channels > 32 {
            return Err(ConfigError::InvalidValue {
                field: "input_channels".to_string(),
                value: self.input_channels.to_string(),
            });
        }
        
        if self.output_channels == 0 || self.output_channels > 32 {
            return Err(ConfigError::InvalidValue {
                field: "output_channels".to_string(),
                value: self.output_channels.to_string(),
            });
        }
        
        // Validate compression settings
        if self.processing.compression.enabled {
            if self.processing.compression.ratio < 1.0 || self.processing.compression.ratio > 20.0 {
                return Err(ConfigError::InvalidValue {
                    field: "compression.ratio".to_string(),
                    value: self.processing.compression.ratio.to_string(),
                });
            }
        }
        
        // Validate AGC settings
        if self.agc.enabled {
            if self.agc.max_gain_db < 0.0 || self.agc.max_gain_db > 60.0 {
                return Err(ConfigError::InvalidValue {
                    field: "agc.max_gain_db".to_string(),
                    value: self.agc.max_gain_db.to_string(),
                });
            }
        }
        
        Ok(())
    }
    
    fn merge_with(&mut self, other: Self) {
        // Merge non-default values
        if other.input_device != "default" {
            self.input_device = other.input_device;
        }
        
        if other.output_device != "default" {
            self.output_device = other.output_device;
        }
        
        if other.sample_rate != 48000 {
            self.sample_rate = other.sample_rate;
        }
        
        if other.buffer_size != 512 {
            self.buffer_size = other.buffer_size;
        }
        
        if other.input_channels != 2 {
            self.input_channels = other.input_channels;
        }
        
        if other.output_channels != 2 {
            self.output_channels = other.output_channels;
        }
        
        // Merge complex structures
        self.processing.merge_with(other.processing);
        self.agc.merge_with(other.agc);
        self.noise_reduction.merge_with(other.noise_reduction);
        self.routing.merge_with(other.routing);
        self.recording.merge_with(other.recording);
        self.levels.merge_with(other.levels);
        
        // Merge custom parameters
        self.device_parameters.extend(other.device_parameters);
    }
}

// Implement merge methods for nested configurations
impl AudioProcessingConfig {
    fn merge_with(&mut self, other: Self) {
        if other.enabled != true {
            self.enabled = other.enabled;
        }
        if other.lowpass_cutoff != Some(3000.0) {
            self.lowpass_cutoff = other.lowpass_cutoff;
        }
        if other.highpass_cutoff != Some(300.0) {
            self.highpass_cutoff = other.highpass_cutoff;
        }
        // Continue for other fields...
    }
}

impl AgcConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for AGC config merging
    }
}

impl NoiseReductionConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for noise reduction config merging
    }
}

impl AudioRoutingConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for routing config merging
    }
}

impl RecordingConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for recording config merging
    }
}

impl AudioLevelsConfig {
    fn merge_with(&mut self, _other: Self) {
        // Implementation for levels config merging
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_audio_config() {
        let config = AudioConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.buffer_size, 512);
        assert!(config.validate().is_ok());
    }
    
    #[test]
    fn test_sample_rate_validation() {
        let mut config = AudioConfig::default();
        
        // Valid sample rates
        config.sample_rate = 44100;
        assert!(config.validate().is_ok());
        
        config.sample_rate = 48000;
        assert!(config.validate().is_ok());
        
        // Invalid sample rate
        config.sample_rate = 12345;
        assert!(config.validate().is_err());
    }
    
    #[test]
    fn test_buffer_size_validation() {
        let mut config = AudioConfig::default();
        
        // Valid buffer sizes (powers of 2)
        config.buffer_size = 256;
        assert!(config.validate().is_ok());
        
        config.buffer_size = 1024;
        assert!(config.validate().is_ok());
        
        // Invalid buffer size (not power of 2)
        config.buffer_size = 333;
        assert!(config.validate().is_err());
        
        // Invalid buffer size (too small)
        config.buffer_size = 16;
        assert!(config.validate().is_err());
    }
    
    #[test]
    fn test_compression_config() {
        let compression = CompressionConfig::default();
        assert_eq!(compression.ratio, 3.0);
        assert_eq!(compression.threshold_db, -20.0);
        assert!(!compression.enabled);
    }
    
    #[test]
    fn test_agc_config() {
        let agc = AgcConfig::default();
        assert!(agc.enabled);
        assert_eq!(agc.target_level_db, -20.0);
        assert_eq!(agc.max_gain_db, 30.0);
    }
}