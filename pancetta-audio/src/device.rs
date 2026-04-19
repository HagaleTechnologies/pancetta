//! Audio device enumeration and selection
//!
//! Provides comprehensive device discovery and configuration matching
//! for optimal FT8 audio processing setup.

use crate::error::{AudioError, AudioResult};
use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device, Host, SampleFormat, SampleRate, SupportedStreamConfig,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Audio device information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    /// Device name as reported by the system
    pub name: String,
    /// Whether this device supports input (recording)
    pub supports_input: bool,
    /// Whether this device supports output (playback)
    pub supports_output: bool,
    /// Supported sample rates for input
    pub input_sample_rates: Vec<u32>,
    /// Supported sample rates for output
    pub output_sample_rates: Vec<u32>,
    /// Supported input channel counts
    pub input_channels: Vec<u16>,
    /// Supported output channel counts
    pub output_channels: Vec<u16>,
    /// Supported sample formats
    pub sample_formats: Vec<String>,
    /// Whether this is the default input device
    pub is_default_input: bool,
    /// Whether this is the default output device
    pub is_default_output: bool,
}

impl fmt::Display for AudioDeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if self.is_default_input || self.is_default_output {
            write!(f, " (default")?;
            if self.is_default_input && self.is_default_output {
                write!(f, " I/O")?;
            } else if self.is_default_input {
                write!(f, " input")?;
            } else {
                write!(f, " output")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// Audio device manager for discovery and selection
pub struct AudioDeviceManager {
    host: Host,
    devices: Vec<(Device, AudioDeviceInfo)>,
}

impl AudioDeviceManager {
    /// Create a new device manager
    pub fn new() -> AudioResult<Self> {
        let host = cpal::default_host();
        let mut manager = Self {
            host,
            devices: Vec::new(),
        };

        manager.refresh_devices()?;
        Ok(manager)
    }

    /// Refresh the device list
    pub fn refresh_devices(&mut self) -> AudioResult<()> {
        self.devices.clear();

        let default_input = self.host.default_input_device();
        let default_output = self.host.default_output_device();

        // Get all available devices
        let devices = self
            .host
            .devices()
            .map_err(|e| AudioError::device(format!("Failed to enumerate devices: {}", e)))?;

        for device in devices {
            let device_info = self.get_device_info(&device, &default_input, &default_output)?;
            self.devices.push((device, device_info));
        }

        Ok(())
    }

    /// Get information about all available devices
    pub fn list_devices(&self) -> &[(Device, AudioDeviceInfo)] {
        &self.devices
    }

    /// Get device information only (without Device handles)
    pub fn list_device_info(&self) -> Vec<&AudioDeviceInfo> {
        self.devices.iter().map(|(_, info)| info).collect()
    }

    /// Find devices suitable for FT8 processing
    pub fn find_ft8_compatible_devices(&self) -> Vec<&AudioDeviceInfo> {
        self.devices
            .iter()
            .map(|(_, info)| info)
            .filter(|info| {
                // FT8 requires:
                // - Input capability for receiving signals
                // - Support for 12kHz sample rate (or rates we can convert from)
                // - At least mono input
                info.supports_input
                    && !info.input_channels.is_empty()
                    && (info.input_sample_rates.contains(&12000)
                        || info.input_sample_rates.contains(&48000)
                        || info.input_sample_rates.contains(&44100))
            })
            .collect()
    }

    /// Get the best input device for FT8
    pub fn get_best_ft8_input_device(&self) -> AudioResult<&Device> {
        // Priority order:
        // 1. Default input device if FT8 compatible
        // 2. First device that supports 12kHz natively
        // 3. First device that supports 48kHz (for conversion)
        // 4. Any compatible device

        let compatible_devices: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, info)| {
                info.supports_input
                    && !info.input_channels.is_empty()
                    && (!info.input_sample_rates.is_empty())
            })
            .collect();

        if compatible_devices.is_empty() {
            return Err(AudioError::device("No compatible input devices found"));
        }

        // Try default device first
        if let Some((device, _)) = compatible_devices
            .iter()
            .find(|(_, info)| info.is_default_input)
        {
            return Ok(device);
        }

        // Try device with native 12kHz support
        if let Some((device, _)) = compatible_devices
            .iter()
            .find(|(_, info)| info.input_sample_rates.contains(&12000))
        {
            return Ok(device);
        }

        // Try device with 48kHz support (common, good for conversion)
        if let Some((device, _)) = compatible_devices
            .iter()
            .find(|(_, info)| info.input_sample_rates.contains(&48000))
        {
            return Ok(device);
        }

        // Use any compatible device
        Ok(&compatible_devices[0].0)
    }

    /// Get the best output device for monitoring
    pub fn get_best_output_device(&self) -> AudioResult<&Device> {
        let compatible_devices: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, info)| info.supports_output && !info.output_channels.is_empty())
            .collect();

        if compatible_devices.is_empty() {
            return Err(AudioError::device("No compatible output devices found"));
        }

        // Prefer default output device
        if let Some((device, _)) = compatible_devices
            .iter()
            .find(|(_, info)| info.is_default_output)
        {
            return Ok(device);
        }

        // Use any compatible device
        Ok(&compatible_devices[0].0)
    }

    /// Find optimal configuration for a device
    pub fn find_optimal_config(
        &self,
        device: &Device,
        target_sample_rate: u32,
        target_channels: u16,
        is_input: bool,
    ) -> AudioResult<SupportedStreamConfig> {
        if is_input {
            self.find_optimal_input_config(device, target_sample_rate, target_channels)
        } else {
            self.find_optimal_output_config(device, target_sample_rate, target_channels)
        }
    }

    /// Find optimal input configuration for a device
    fn find_optimal_input_config(
        &self,
        device: &Device,
        target_sample_rate: u32,
        target_channels: u16,
    ) -> AudioResult<SupportedStreamConfig> {
        let configs: Vec<_> = device
            .supported_input_configs()
            .map_err(|e| AudioError::device(format!("Failed to get input configs: {}", e)))?
            .collect();

        tracing::info!(
            "Input device has {} supported configs (want {}Hz/{}ch)",
            configs.len(),
            target_sample_rate,
            target_channels,
        );
        for (i, config) in configs.iter().enumerate() {
            tracing::info!(
                "  config[{}]: {}ch, {}–{}Hz, {:?}",
                i,
                config.channels(),
                config.min_sample_rate().0,
                config.max_sample_rate().0,
                config.sample_format(),
            );
        }

        let mut best_config = None;
        let mut best_score: i32 = 0;

        for config in configs.iter() {
            // Every config that supports audio is usable — start at 1
            let mut score: i32 = 1;

            // Prefer exact sample rate match
            if config.min_sample_rate().0 <= target_sample_rate
                && config.max_sample_rate().0 >= target_sample_rate
            {
                score += 100;
            } else if config.max_sample_rate().0 > target_sample_rate {
                score += 50;
            } else {
                score += 10; // Still usable, DSP can resample
            }

            // Prefer matching channel count, but mono is fine for FT8
            if config.channels() == target_channels {
                score += 50;
            } else if config.channels() >= target_channels {
                score += 25; // Can downmix
            } else {
                score += 10; // Mono input is fine — FT8 is mono anyway
            }

            // Prefer f32 sample format for processing
            match config.sample_format() {
                SampleFormat::F32 => score += 30,
                SampleFormat::I32 => score += 20,
                SampleFormat::I16 => score += 10,
                _ => score += 1,
            }

            if score > best_score {
                best_score = score;
                // Use a sample rate that's within the supported range
                let sample_rate = if target_sample_rate >= config.min_sample_rate().0
                    && target_sample_rate <= config.max_sample_rate().0
                {
                    target_sample_rate
                } else if config.max_sample_rate().0 >= 48000 {
                    48000
                } else if config.max_sample_rate().0 >= 44100 {
                    44100
                } else {
                    config.max_sample_rate().0
                };
                best_config = Some(config.with_sample_rate(SampleRate(sample_rate)));
            }
        }

        if let Some(cfg) = best_config {
            if cfg.channels() != target_channels || cfg.sample_rate().0 != target_sample_rate {
                tracing::info!(
                    "Input device config: {}Hz/{}ch (requested {}Hz/{}ch)",
                    cfg.sample_rate().0,
                    cfg.channels(),
                    target_sample_rate,
                    target_channels,
                );
            }
            return Ok(cfg);
        }

        // Fallback: try the device's default input config
        if let Ok(default_cfg) = device.default_input_config() {
            tracing::warn!(
                "No scored input config — using device default ({}Hz/{}ch)",
                default_cfg.sample_rate().0,
                default_cfg.channels(),
            );
            return Ok(default_cfg);
        }

        Err(AudioError::configuration(format!(
            "No suitable input configuration found for {}Hz, {} channels",
            target_sample_rate, target_channels
        )))
    }

    /// Find optimal output configuration for a device
    fn find_optimal_output_config(
        &self,
        device: &Device,
        target_sample_rate: u32,
        target_channels: u16,
    ) -> AudioResult<SupportedStreamConfig> {
        let configs = device
            .supported_output_configs()
            .map_err(|e| AudioError::device(format!("Failed to get output configs: {}", e)))?;

        let mut best_config = None;
        let mut best_score: i32 = 0;

        for config in configs {
            let mut score: i32 = 1;

            if config.min_sample_rate().0 <= target_sample_rate
                && config.max_sample_rate().0 >= target_sample_rate
            {
                score += 100;
            } else if config.max_sample_rate().0 > target_sample_rate {
                score += 50;
            } else {
                score += 10;
            }

            if config.channels() == target_channels {
                score += 50;
            } else if config.channels() >= target_channels {
                score += 25;
            } else {
                score += 10;
            }

            match config.sample_format() {
                SampleFormat::F32 => score += 30,
                SampleFormat::I32 => score += 20,
                SampleFormat::I16 => score += 10,
                _ => score += 1,
            }

            if score > best_score {
                best_score = score;
                let sample_rate = if target_sample_rate >= config.min_sample_rate().0
                    && target_sample_rate <= config.max_sample_rate().0
                {
                    target_sample_rate
                } else if config.max_sample_rate().0 >= 48000 {
                    48000
                } else if config.max_sample_rate().0 >= 44100 {
                    44100
                } else {
                    config.max_sample_rate().0
                };
                best_config = Some(config.with_sample_rate(SampleRate(sample_rate)));
            }
        }

        if let Some(cfg) = best_config {
            if cfg.channels() != target_channels || cfg.sample_rate().0 != target_sample_rate {
                tracing::info!(
                    "Output device config: {}Hz/{}ch (requested {}Hz/{}ch)",
                    cfg.sample_rate().0,
                    cfg.channels(),
                    target_sample_rate,
                    target_channels,
                );
            }
            return Ok(cfg);
        }

        // Fallback: try the device's default output config
        if let Ok(default_cfg) = device.default_output_config() {
            tracing::warn!(
                "No scored output config — using device default ({}Hz/{}ch)",
                default_cfg.sample_rate().0,
                default_cfg.channels(),
            );
            return Ok(default_cfg);
        }

        Err(AudioError::configuration(format!(
            "No suitable output configuration found for {}Hz, {} channels",
            target_sample_rate, target_channels
        )))
    }

    /// Get device information for a specific device
    fn get_device_info(
        &self,
        device: &Device,
        default_input: &Option<Device>,
        default_output: &Option<Device>,
    ) -> AudioResult<AudioDeviceInfo> {
        let name = device
            .name()
            .unwrap_or_else(|_| "Unknown Device".to_string());

        // Check if this is a default device
        let is_default_input = default_input
            .as_ref()
            .map(|d| d.name().unwrap_or_default() == name)
            .unwrap_or(false);

        let is_default_output = default_output
            .as_ref()
            .map(|d| d.name().unwrap_or_default() == name)
            .unwrap_or(false);

        // Check input capabilities
        let (supports_input, input_sample_rates, input_channels) = match device
            .supported_input_configs()
        {
            Ok(configs) => {
                let mut sample_rates = Vec::new();
                let mut channels = Vec::new();

                for config in configs {
                    // Collect sample rate range
                    let min_rate = config.min_sample_rate().0;
                    let max_rate = config.max_sample_rate().0;

                    // Add common sample rates within range
                    for &rate in &[8000, 12000, 16000, 22050, 44100, 48000, 96000, 192000] {
                        if rate >= min_rate && rate <= max_rate && !sample_rates.contains(&rate) {
                            sample_rates.push(rate);
                        }
                    }

                    if !channels.contains(&config.channels()) {
                        channels.push(config.channels());
                    }
                }

                (true, sample_rates, channels)
            }
            Err(_) => (false, Vec::new(), Vec::new()),
        };

        // Check output capabilities
        let (supports_output, output_sample_rates, output_channels) = match device
            .supported_output_configs()
        {
            Ok(configs) => {
                let mut sample_rates = Vec::new();
                let mut channels = Vec::new();

                for config in configs {
                    let min_rate = config.min_sample_rate().0;
                    let max_rate = config.max_sample_rate().0;

                    for &rate in &[8000, 12000, 16000, 22050, 44100, 48000, 96000, 192000] {
                        if rate >= min_rate && rate <= max_rate && !sample_rates.contains(&rate) {
                            sample_rates.push(rate);
                        }
                    }

                    if !channels.contains(&config.channels()) {
                        channels.push(config.channels());
                    }
                }

                (true, sample_rates, channels)
            }
            Err(_) => (false, Vec::new(), Vec::new()),
        };

        // Get supported sample formats (simplified)
        let sample_formats = vec!["F32".to_string(), "I32".to_string(), "I16".to_string()];

        Ok(AudioDeviceInfo {
            name,
            supports_input,
            supports_output,
            input_sample_rates,
            output_sample_rates,
            input_channels,
            output_channels,
            sample_formats,
            is_default_input,
            is_default_output,
        })
    }
}

impl Default for AudioDeviceManager {
    fn default() -> Self {
        Self::new().expect("Failed to create audio device manager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_manager_creation() {
        let manager = AudioDeviceManager::new();
        assert!(manager.is_ok());
    }

    #[test]
    fn test_device_enumeration() {
        let manager = AudioDeviceManager::new().unwrap();
        let devices = manager.list_device_info();

        // Should have at least one device on any system with audio
        // Note: This might fail in CI environments without audio
        if !devices.is_empty() {
            println!("Found {} audio devices", devices.len());
            for device in devices {
                println!("  {}", device);
            }
        }
    }

    #[test]
    fn test_ft8_device_search() {
        let manager = AudioDeviceManager::new().unwrap();
        let ft8_devices = manager.find_ft8_compatible_devices();

        println!("Found {} FT8-compatible devices", ft8_devices.len());
        for device in ft8_devices {
            println!("  {}", device.name);
        }
    }
}
