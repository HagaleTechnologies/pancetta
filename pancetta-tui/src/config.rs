use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use ratatui::style::Color;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub station: StationConfig,
    pub ui: UiConfig,
    pub audio: AudioConfig,
    pub decoder: DecoderConfig,
    pub bands: BandConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationConfig {
    pub call_sign: String,
    pub grid_square: String,
    pub power: u32,
    pub antenna: String,
    pub rig: String,
    pub default_frequency: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: Theme,
    pub refresh_rate: u64,
    pub max_messages: usize,
    pub show_waterfall: bool,
    pub show_coordinates: bool,
    pub time_format: TimeFormat,
    pub frequency_format: FrequencyFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub device: Option<String>,
    pub sample_rate: u32,
    pub buffer_size: usize,
    pub auto_gain: bool,
    pub gain_level: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecoderConfig {
    pub enabled_modes: Vec<String>,
    pub minimum_snr: i32,
    pub decode_depth: u8,
    pub aggressive_decode: bool,
    pub enable_averaging: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandConfig {
    pub bands: Vec<Band>,
    pub default_band: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    pub name: String,
    pub frequency_range: (f64, f64),
    pub default_mode: String,
    pub power_limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Theme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TimeFormat {
    UTC12,
    UTC24,
    Local12,
    Local24,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FrequencyFormat {
    MHz,
    KHz,
    Hz,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            station: StationConfig {
                call_sign: "N0CALL".to_string(),
                grid_square: "FN20".to_string(),
                power: 5,
                antenna: "Dipole".to_string(),
                rig: "IC-7300".to_string(),
                default_frequency: 14.074,
            },
            ui: UiConfig {
                theme: Theme::Dark,
                refresh_rate: 250,
                max_messages: 1000,
                show_waterfall: true,
                show_coordinates: true,
                time_format: TimeFormat::UTC24,
                frequency_format: FrequencyFormat::MHz,
            },
            audio: AudioConfig {
                device: None,
                sample_rate: 48000,
                buffer_size: 1024,
                auto_gain: true,
                gain_level: 1.0,
            },
            decoder: DecoderConfig {
                enabled_modes: vec!["FT8".to_string(), "FT4".to_string()],
                minimum_snr: -24,
                decode_depth: 3,
                aggressive_decode: false,
                enable_averaging: true,
            },
            bands: BandConfig {
                default_band: "20M".to_string(),
                bands: vec![
                    Band {
                        name: "80M".to_string(),
                        frequency_range: (3.573, 3.573),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "40M".to_string(),
                        frequency_range: (7.074, 7.074),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "20M".to_string(),
                        frequency_range: (14.074, 14.074),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "17M".to_string(),
                        frequency_range: (18.100, 18.100),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "15M".to_string(),
                        frequency_range: (21.074, 21.074),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "12M".to_string(),
                        frequency_range: (24.915, 24.915),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "10M".to_string(),
                        frequency_range: (28.074, 28.074),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                    Band {
                        name: "6M".to_string(),
                        frequency_range: (50.313, 50.313),
                        default_mode: "FT8".to_string(),
                        power_limit: None,
                    },
                ],
            },
        }
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        
        // Expand tilde in path
        let expanded_path = if path.starts_with("~") {
            if let Some(home) = dirs::home_dir() {
                home.join(path.strip_prefix("~").unwrap_or(path))
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        };

        if !expanded_path.exists() {
            // Create default config file
            let default_config = Config::default();
            default_config.save(&expanded_path)?;
            return Ok(default_config);
        }

        let contents = fs::read_to_string(&expanded_path)
            .with_context(|| format!("Failed to read config file: {}", expanded_path.display()))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", expanded_path.display()))?;

        Ok(config)
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        
        // Expand tilde in path
        let expanded_path = if path.starts_with("~") {
            if let Some(home) = dirs::home_dir() {
                home.join(path.strip_prefix("~").unwrap_or(path))
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        };

        // Create directory if it doesn't exist
        if let Some(parent) = expanded_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }

        let contents = toml::to_string_pretty(self)
            .context("Failed to serialize config")?;

        fs::write(&expanded_path, contents)
            .with_context(|| format!("Failed to write config file: {}", expanded_path.display()))?;

        Ok(())
    }

    pub fn get_band(&self, name: &str) -> Option<&Band> {
        self.bands.bands.iter().find(|b| b.name == name)
    }

    pub fn get_current_band(&self, frequency: f64) -> Option<&Band> {
        self.bands.bands.iter().find(|b| {
            frequency >= b.frequency_range.0 && frequency <= b.frequency_range.1
        })
    }
}

// Color schemes for different themes
impl Theme {
    pub fn background_color(&self) -> Color {
        match self {
            Theme::Dark => Color::Black,
            Theme::Light => Color::White,
        }
    }

    pub fn foreground_color(&self) -> Color {
        match self {
            Theme::Dark => Color::White,
            Theme::Light => Color::Black,
        }
    }

    pub fn accent_color(&self) -> Color {
        match self {
            Theme::Dark => Color::Cyan,
            Theme::Light => Color::Blue,
        }
    }

    pub fn border_color(&self) -> Color {
        match self {
            Theme::Dark => Color::Gray,
            Theme::Light => Color::DarkGray,
        }
    }

    pub fn selected_color(&self) -> Color {
        match self {
            Theme::Dark => Color::Yellow,
            Theme::Light => Color::Red,
        }
    }

    pub fn success_color(&self) -> Color {
        Color::Green
    }

    pub fn warning_color(&self) -> Color {
        Color::Yellow
    }

    pub fn error_color(&self) -> Color {
        Color::Red
    }

    pub fn muted_color(&self) -> Color {
        match self {
            Theme::Dark => Color::DarkGray,
            Theme::Light => Color::Gray,
        }
    }
}

impl TimeFormat {
    pub fn format_time(&self, time: chrono::DateTime<chrono::Utc>) -> String {
        match self {
            TimeFormat::UTC12 => time.format("%I:%M:%S %p UTC").to_string(),
            TimeFormat::UTC24 => time.format("%H:%M:%S UTC").to_string(),
            TimeFormat::Local12 => {
                let local = time.with_timezone(&chrono::Local);
                local.format("%I:%M:%S %p").to_string()
            }
            TimeFormat::Local24 => {
                let local = time.with_timezone(&chrono::Local);
                local.format("%H:%M:%S").to_string()
            }
        }
    }
}

impl FrequencyFormat {
    pub fn format_frequency(&self, freq: f64) -> String {
        match self {
            FrequencyFormat::MHz => format!("{:.3} MHz", freq),
            FrequencyFormat::KHz => format!("{:.1} kHz", freq * 1000.0),
            FrequencyFormat::Hz => format!("{:.0} Hz", freq * 1_000_000.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_config_save_and_load() {
        let temp_file = NamedTempFile::new().unwrap();
        let config = Config::default();
        
        config.save(temp_file.path()).unwrap();
        let loaded_config = Config::load(temp_file.path()).unwrap();
        
        assert_eq!(config.station.call_sign, loaded_config.station.call_sign);
        assert_eq!(config.ui.theme as u8, loaded_config.ui.theme as u8);
    }

    #[test]
    fn test_theme_colors() {
        let dark_theme = Theme::Dark;
        let light_theme = Theme::Light;
        
        assert_eq!(dark_theme.background_color(), Color::Black);
        assert_eq!(light_theme.background_color(), Color::White);
        assert_eq!(dark_theme.foreground_color(), Color::White);
        assert_eq!(light_theme.foreground_color(), Color::Black);
    }

    #[test]
    fn test_frequency_formatting() {
        let freq = 14.074;
        
        assert_eq!(FrequencyFormat::MHz.format_frequency(freq), "14.074 MHz");
        assert_eq!(FrequencyFormat::KHz.format_frequency(freq), "14074.0 kHz");
        assert_eq!(FrequencyFormat::Hz.format_frequency(freq), "14074000 Hz");
    }
}