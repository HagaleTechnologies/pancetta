use std::collections::VecDeque;
use thiserror::Error;
use tracing::{debug, trace};

#[derive(Debug, Error)]
pub enum AgcError {
    #[error("Invalid AGC parameter: {parameter} = {value}")]
    InvalidParameter { parameter: String, value: f32 },
    #[error("AGC processing failed: {message}")]
    ProcessingFailed { message: String },
}

pub type Result<T> = std::result::Result<T, AgcError>;

/// AGC operating modes
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgcMode {
    /// Slow AGC for speech and general audio
    Slow,
    /// Medium AGC for digital modes
    Medium,
    /// Fast AGC for CW and rapid signals  
    Fast,
    /// Custom AGC with user-defined parameters
    Custom,
}

/// AGC configuration parameters
#[derive(Debug, Clone)]
pub struct AgcConfig {
    /// Target output level (0.0 to 1.0)
    pub target_level: f32,
    /// Attack time constant in seconds
    pub attack_time: f32,
    /// Decay time constant in seconds  
    pub decay_time: f32,
    /// Hang time in seconds (time to hold gain after signal drops)
    pub hang_time: f32,
    /// Maximum gain in dB
    pub max_gain_db: f32,
    /// Minimum gain in dB (prevents excessive amplification of noise)
    pub min_gain_db: f32,
    /// Threshold below which AGC doesn't operate
    pub threshold: f32,
    /// Knee compression ratio (1.0 = no compression, >1.0 = compression)
    pub compression_ratio: f32,
}

impl Default for AgcConfig {
    fn default() -> Self {
        Self::new_for_mode(AgcMode::Medium)
    }
}

impl AgcConfig {
    /// Create AGC configuration for specific mode
    pub fn new_for_mode(mode: AgcMode) -> Self {
        match mode {
            AgcMode::Slow => Self {
                target_level: 0.5,
                attack_time: 0.1,
                decay_time: 2.0,
                hang_time: 1.0,
                max_gain_db: 40.0,
                min_gain_db: -20.0,
                threshold: 0.001,
                compression_ratio: 3.0,
            },
            AgcMode::Medium => Self {
                target_level: 0.5,
                attack_time: 0.01,
                decay_time: 0.5,
                hang_time: 0.2,
                max_gain_db: 40.0,
                min_gain_db: -20.0,
                threshold: 0.001,
                compression_ratio: 4.0,
            },
            AgcMode::Fast => Self {
                target_level: 0.5,
                attack_time: 0.001,
                decay_time: 0.1,
                hang_time: 0.05,
                max_gain_db: 40.0,
                min_gain_db: -20.0,
                threshold: 0.001,
                compression_ratio: 6.0,
            },
            AgcMode::Custom => Self::default(),
        }
    }

    /// Create AGC configuration optimized for FT8
    pub fn new_ft8_optimized() -> Self {
        Self {
            target_level: 0.4,      // Conservative level for digital mode
            attack_time: 0.01,      // Fast attack for digital pulses
            decay_time: 0.3,        // Medium decay
            hang_time: 0.1,         // Short hang time
            max_gain_db: 30.0,      // Moderate max gain
            min_gain_db: -10.0,     // Prevent excessive noise amplification
            threshold: 0.0005,      // Low threshold for weak signals
            compression_ratio: 3.0, // Gentle compression
        }
    }

    /// Validate configuration parameters
    pub fn validate(&self) -> Result<()> {
        if self.target_level <= 0.0 || self.target_level > 1.0 {
            return Err(AgcError::InvalidParameter {
                parameter: "target_level".to_string(),
                value: self.target_level,
            });
        }

        if self.attack_time <= 0.0 {
            return Err(AgcError::InvalidParameter {
                parameter: "attack_time".to_string(),
                value: self.attack_time,
            });
        }

        if self.decay_time <= 0.0 {
            return Err(AgcError::InvalidParameter {
                parameter: "decay_time".to_string(),
                value: self.decay_time,
            });
        }

        if self.max_gain_db <= self.min_gain_db {
            return Err(AgcError::InvalidParameter {
                parameter: "gain_range".to_string(),
                value: self.max_gain_db - self.min_gain_db,
            });
        }

        Ok(())
    }
}

/// High-performance Automatic Gain Control
/// Implements sophisticated AGC with hang time, compression, and noise gating
pub struct AutomaticGainControl {
    /// AGC configuration
    config: AgcConfig,
    /// Sample rate
    sample_rate: f32,
    /// Current gain (linear scale)
    current_gain: f32,
    /// Peak level detector
    peak_detector: PeakDetector,
    /// Hang timer
    hang_timer: f32,
    /// Gain smoothing filter
    gain_smoother: ExponentialFilter,
    /// Attack time constant (samples)
    attack_alpha: f32,
    /// Decay time constant (samples)
    decay_alpha: f32,
    /// Recent gain history for statistics
    gain_history: VecDeque<f32>,
    /// Processing statistics
    stats: AgcStats,
}

#[derive(Debug, Clone, Default)]
pub struct AgcStats {
    pub samples_processed: u64,
    pub gain_adjustments: u64,
    pub current_gain_db: f32,
    pub peak_input_level: f32,
    pub peak_output_level: f32,
    pub average_gain_db: f32,
}

/// Peak detector for AGC level sensing
struct PeakDetector {
    /// Current peak value
    peak: f32,
    /// Attack time constant
    attack_alpha: f32,
    /// Decay time constant  
    decay_alpha: f32,
}

impl PeakDetector {
    fn new(attack_time: f32, decay_time: f32, sample_rate: f32) -> Self {
        Self {
            peak: 0.0,
            attack_alpha: (-1.0 / (attack_time * sample_rate)).exp(),
            decay_alpha: (-1.0 / (decay_time * sample_rate)).exp(),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let input_abs = input.abs();

        if input_abs > self.peak {
            // Attack: fast rise
            self.peak = self.peak * self.attack_alpha + input_abs * (1.0 - self.attack_alpha);
        } else {
            // Decay: slow fall
            self.peak = self.peak * self.decay_alpha;
        }

        self.peak
    }

    fn reset(&mut self) {
        self.peak = 0.0;
    }
}

/// Exponential smoothing filter for gain changes
struct ExponentialFilter {
    value: f32,
    alpha: f32,
}

impl ExponentialFilter {
    fn new(time_constant: f32, sample_rate: f32) -> Self {
        Self {
            value: 1.0,
            alpha: (-1.0 / (time_constant * sample_rate)).exp(),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.value = self.value * self.alpha + input * (1.0 - self.alpha);
        self.value
    }

    fn set_time_constant(&mut self, time_constant: f32, sample_rate: f32) {
        self.alpha = (-1.0 / (time_constant * sample_rate)).exp();
    }
}

impl AutomaticGainControl {
    /// Create a new AGC instance
    pub fn new(config: AgcConfig, sample_rate: f32) -> Result<Self> {
        config.validate()?;

        let attack_alpha = (-1.0 / (config.attack_time * sample_rate)).exp();
        let decay_alpha = (-1.0 / (config.decay_time * sample_rate)).exp();

        let peak_detector = PeakDetector::new(
            config.attack_time * 0.1, // Faster peak detection
            config.decay_time * 0.5,  // Medium peak decay
            sample_rate,
        );

        let gain_smoother = ExponentialFilter::new(config.attack_time, sample_rate);

        debug!(
            "Created AGC: target={}, attack={}ms, decay={}ms, hang={}ms, gain_range={}dB to {}dB",
            config.target_level,
            config.attack_time * 1000.0,
            config.decay_time * 1000.0,
            config.hang_time * 1000.0,
            config.min_gain_db,
            config.max_gain_db
        );

        Ok(Self {
            config,
            sample_rate,
            current_gain: 1.0,
            peak_detector,
            hang_timer: 0.0,
            gain_smoother,
            attack_alpha,
            decay_alpha,
            gain_history: VecDeque::with_capacity(1000),
            stats: AgcStats::default(),
        })
    }

    /// Create AGC optimized for FT8 processing
    pub fn new_ft8_optimized(sample_rate: f32) -> Result<Self> {
        Self::new(AgcConfig::new_ft8_optimized(), sample_rate)
    }

    /// Process audio samples through the AGC
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<()> {
        if input.len() != output.len() {
            return Err(AgcError::ProcessingFailed {
                message: format!(
                    "Input/output length mismatch: {} vs {}",
                    input.len(),
                    output.len()
                ),
            });
        }

        for (i, &sample) in input.iter().enumerate() {
            // Peak detection
            let peak_level = self.peak_detector.process(sample);

            // Calculate desired gain
            let desired_gain = if peak_level > self.config.threshold {
                let compression_gain = if peak_level > self.config.target_level {
                    // Apply compression above target level
                    let overshoot = peak_level / self.config.target_level;
                    let compressed_overshoot = overshoot.powf(1.0 / self.config.compression_ratio);
                    self.config.target_level / (peak_level / compressed_overshoot)
                } else {
                    // Linear gain below target level
                    self.config.target_level / peak_level
                };

                // Convert to dB and clamp
                let gain_db = 20.0 * compression_gain.log10();
                let clamped_gain_db =
                    gain_db.clamp(self.config.min_gain_db, self.config.max_gain_db);
                10.0_f32.powf(clamped_gain_db / 20.0)
            } else {
                // Below threshold - minimal gain
                10.0_f32.powf(self.config.min_gain_db / 20.0)
            };

            // AGC state machine with hang time
            let new_gain = if desired_gain < self.current_gain {
                // Gain needs to decrease (signal got stronger)
                self.hang_timer = self.config.hang_time * self.sample_rate;
                self.current_gain * self.attack_alpha + desired_gain * (1.0 - self.attack_alpha)
            } else if self.hang_timer > 0.0 {
                // In hang period - hold current gain
                self.hang_timer -= 1.0;
                self.current_gain
            } else {
                // Gain can increase (signal got weaker)
                self.current_gain * self.decay_alpha + desired_gain * (1.0 - self.decay_alpha)
            };

            // Smooth gain changes
            self.current_gain = self.gain_smoother.process(new_gain);

            // Apply gain to sample
            output[i] = sample * self.current_gain;

            // Update statistics
            self.stats.samples_processed += 1;
            if (self.current_gain - new_gain).abs() > 0.001 {
                self.stats.gain_adjustments += 1;
            }

            self.stats.peak_input_level = self.stats.peak_input_level.max(sample.abs());
            self.stats.peak_output_level = self.stats.peak_output_level.max(output[i].abs());
        }

        // Update gain history for average calculation
        let current_gain_db = 20.0 * self.current_gain.log10();
        self.stats.current_gain_db = current_gain_db;

        self.gain_history.push_back(current_gain_db);
        if self.gain_history.len() > 1000 {
            self.gain_history.pop_front();
        }

        // Calculate average gain
        if !self.gain_history.is_empty() {
            self.stats.average_gain_db =
                self.gain_history.iter().sum::<f32>() / self.gain_history.len() as f32;
        }

        trace!(
            "AGC processed {} samples, current_gain={}dB, peak_in={:.3}, peak_out={:.3}",
            input.len(),
            current_gain_db,
            self.stats.peak_input_level,
            self.stats.peak_output_level
        );

        Ok(())
    }

    /// Process samples in-place
    pub fn process_inplace(&mut self, samples: &mut [f32]) -> Result<()> {
        // Create temporary buffer for output
        let mut output = vec![0.0; samples.len()];
        self.process(samples, &mut output)?;
        samples.copy_from_slice(&output);
        Ok(())
    }

    /// Get current gain in dB
    pub fn current_gain_db(&self) -> f32 {
        20.0 * self.current_gain.log10()
    }

    /// Get current gain (linear)
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Get AGC statistics
    pub fn stats(&self) -> &AgcStats {
        &self.stats
    }

    /// Reset AGC state
    pub fn reset(&mut self) {
        self.current_gain = 1.0;
        self.peak_detector.reset();
        self.hang_timer = 0.0;
        self.gain_smoother = ExponentialFilter::new(self.config.attack_time, self.sample_rate);
        self.gain_history.clear();
        self.stats = AgcStats::default();
    }

    /// Update AGC configuration
    pub fn update_config(&mut self, config: AgcConfig) -> Result<()> {
        config.validate()?;

        self.config = config;
        self.attack_alpha = (-1.0 / (self.config.attack_time * self.sample_rate)).exp();
        self.decay_alpha = (-1.0 / (self.config.decay_time * self.sample_rate)).exp();

        // Update peak detector time constants
        self.peak_detector = PeakDetector::new(
            self.config.attack_time * 0.1,
            self.config.decay_time * 0.5,
            self.sample_rate,
        );

        // Update gain smoother
        self.gain_smoother
            .set_time_constant(self.config.attack_time, self.sample_rate);

        debug!("AGC configuration updated");
        Ok(())
    }

    /// Get configuration
    pub fn config(&self) -> &AgcConfig {
        &self.config
    }

    /// Reset statistics
    pub fn reset_stats(&mut self) {
        self.stats = AgcStats::default();
        self.gain_history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agc_creation() {
        let config = AgcConfig::new_ft8_optimized();
        let agc = AutomaticGainControl::new(config, 12000.0);
        assert!(agc.is_ok());
    }

    #[test]
    fn test_agc_processing() {
        let mut agc = AutomaticGainControl::new_ft8_optimized(12000.0).unwrap();

        let input = vec![0.1; 1000];
        let mut output = vec![0.0; 1000];

        let result = agc.process(&input, &mut output);
        assert!(result.is_ok());
        assert!(agc.stats().samples_processed == 1000);
    }

    #[test]
    fn test_config_validation() {
        let mut config = AgcConfig::default();
        config.target_level = 1.5; // Invalid: > 1.0

        assert!(config.validate().is_err());
    }
}
