//! Unified amateur radio band definition

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Amateur radio band enumeration - centralized definition
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Band {
    /// 160 meters (1.8-2.0 MHz)
    #[serde(rename = "160m")]
    Band160m,
    /// 80 meters (3.5-4.0 MHz)
    #[serde(rename = "80m")]
    Band80m,
    /// 60 meters (5 MHz channels)
    #[serde(rename = "60m")]
    Band60m,
    /// 40 meters (7.0-7.3 MHz)
    #[serde(rename = "40m")]
    Band40m,
    /// 30 meters (10.1-10.15 MHz)
    #[serde(rename = "30m")]
    Band30m,
    /// 20 meters (14.0-14.35 MHz)
    #[default]
    #[serde(rename = "20m")]
    Band20m,
    /// 17 meters (18.068-18.168 MHz)
    #[serde(rename = "17m")]
    Band17m,
    /// 15 meters (21.0-21.45 MHz)
    #[serde(rename = "15m")]
    Band15m,
    /// 12 meters (24.89-24.99 MHz)
    #[serde(rename = "12m")]
    Band12m,
    /// 10 meters (28.0-29.7 MHz)
    #[serde(rename = "10m")]
    Band10m,
    /// 6 meters (50-54 MHz)
    #[serde(rename = "6m")]
    Band6m,
    /// 2 meters (144-148 MHz)
    #[serde(rename = "2m")]
    Band2m,
    /// 70 centimeters (420-450 MHz)
    #[serde(rename = "70cm")]
    Band70cm,
    /// Custom band with frequency range
    #[serde(rename = "custom")]
    Custom(u64, u64),
}

impl Band {
    /// Get frequency range in Hz (low, high)
    pub fn frequency_range(&self) -> (u64, u64) {
        match self {
            Band::Band160m => (1_800_000, 2_000_000),
            Band::Band80m => (3_500_000, 4_000_000),
            Band::Band60m => (5_330_000, 5_405_000),
            Band::Band40m => (7_000_000, 7_300_000),
            Band::Band30m => (10_100_000, 10_150_000),
            Band::Band20m => (14_000_000, 14_350_000),
            Band::Band17m => (18_068_000, 18_168_000),
            Band::Band15m => (21_000_000, 21_450_000),
            Band::Band12m => (24_890_000, 24_990_000),
            Band::Band10m => (28_000_000, 29_700_000),
            Band::Band6m => (50_000_000, 54_000_000),
            Band::Band2m => (144_000_000, 148_000_000),
            Band::Band70cm => (420_000_000, 450_000_000),
            Band::Custom(low, high) => (*low, *high),
        }
    }

    /// Get band from frequency in Hz
    pub fn from_frequency(freq: u64) -> Option<Band> {
        let bands = [
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

        for band in bands {
            let (low, high) = band.frequency_range();
            if freq >= low && freq <= high {
                return Some(band);
            }
        }
        None
    }

    /// Get wavelength in meters
    pub fn wavelength(&self) -> f64 {
        let (low, high) = self.frequency_range();
        let center_freq = (low + high) / 2;
        299_792_458.0 / center_freq as f64
    }

    /// Check if this is an HF band (< 30 MHz)
    pub fn is_hf(&self) -> bool {
        let (_, high) = self.frequency_range();
        high < 30_000_000
    }

    /// Check if this is a VHF band (30-300 MHz)
    pub fn is_vhf(&self) -> bool {
        let (low, high) = self.frequency_range();
        low >= 30_000_000 && high <= 300_000_000
    }

    /// Check if this is a UHF band (> 300 MHz)
    pub fn is_uhf(&self) -> bool {
        let (low, _) = self.frequency_range();
        low > 300_000_000
    }

    /// Get all standard bands
    pub fn all() -> &'static [Band] {
        &[
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
        ]
    }

    /// Check if frequency is within this band
    pub fn contains_frequency(&self, freq: u64) -> bool {
        let (low, high) = self.frequency_range();
        freq >= low && freq <= high
    }

    /// Get FT8 frequency for this band (if applicable)
    pub fn ft8_frequency(&self) -> Option<u64> {
        match self {
            Band::Band160m => Some(1_840_000),
            Band::Band80m => Some(3_573_000),
            Band::Band60m => Some(5_357_000),
            Band::Band40m => Some(7_074_000),
            Band::Band30m => Some(10_136_000),
            Band::Band20m => Some(14_074_000),
            Band::Band17m => Some(18_100_000),
            Band::Band15m => Some(21_074_000),
            Band::Band12m => Some(24_915_000),
            Band::Band10m => Some(28_074_000),
            Band::Band6m => Some(50_313_000),
            Band::Band2m => Some(144_174_000),
            _ => None,
        }
    }

    /// Get the standard FT4 dial frequency for this band (if applicable).
    ///
    /// FT4 uses different sub-band dial frequencies than FT8 and is not used
    /// on every band (e.g. 60m, 160m, 2m). Bands without a standard FT4
    /// frequency return `None`.
    pub fn ft4_frequency(&self) -> Option<u64> {
        match self {
            Band::Band80m => Some(3_575_000),
            Band::Band40m => Some(7_047_500),
            Band::Band30m => Some(10_140_000),
            Band::Band20m => Some(14_080_000),
            Band::Band17m => Some(18_104_000),
            Band::Band15m => Some(21_140_000),
            Band::Band12m => Some(24_919_000),
            Band::Band10m => Some(28_180_000),
            Band::Band6m => Some(50_318_000),
            _ => None,
        }
    }

    /// Get the dial frequency for this band given the active mode.
    ///
    /// Callers pass `is_ft4 = (active mode == FT4)`. When `is_ft4` is true the
    /// FT4 dial table is consulted; otherwise the FT8 table is used. This keeps
    /// `pancetta-core` dependency-free (no knowledge of the config/ft8 crates)
    /// while letting band selection be mode-aware at the point of use.
    pub fn dial_for(&self, is_ft4: bool) -> Option<u64> {
        if is_ft4 {
            self.ft4_frequency()
        } else {
            self.ft8_frequency()
        }
    }

    /// Get numeric ID for this band (for hashing/indexing purposes)
    pub fn to_id(&self) -> u8 {
        match self {
            Band::Band160m => 0,
            Band::Band80m => 1,
            Band::Band60m => 2,
            Band::Band40m => 3,
            Band::Band30m => 4,
            Band::Band20m => 5,
            Band::Band17m => 6,
            Band::Band15m => 7,
            Band::Band12m => 8,
            Band::Band10m => 9,
            Band::Band6m => 10,
            Band::Band2m => 11,
            Band::Band70cm => 12,
            Band::Custom(low, _) => (low / 1_000_000) as u8 % 100, // Use frequency as hash
        }
    }
}

impl fmt::Display for Band {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Band::Band160m => write!(f, "160m"),
            Band::Band80m => write!(f, "80m"),
            Band::Band60m => write!(f, "60m"),
            Band::Band40m => write!(f, "40m"),
            Band::Band30m => write!(f, "30m"),
            Band::Band20m => write!(f, "20m"),
            Band::Band17m => write!(f, "17m"),
            Band::Band15m => write!(f, "15m"),
            Band::Band12m => write!(f, "12m"),
            Band::Band10m => write!(f, "10m"),
            Band::Band6m => write!(f, "6m"),
            Band::Band2m => write!(f, "2m"),
            Band::Band70cm => write!(f, "70cm"),
            Band::Custom(low, high) => write!(f, "{}-{} MHz", low / 1_000_000, high / 1_000_000),
        }
    }
}

impl FromStr for Band {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "160m" => Ok(Band::Band160m),
            "80m" => Ok(Band::Band80m),
            "60m" => Ok(Band::Band60m),
            "40m" => Ok(Band::Band40m),
            "30m" => Ok(Band::Band30m),
            "20m" => Ok(Band::Band20m),
            "17m" => Ok(Band::Band17m),
            "15m" => Ok(Band::Band15m),
            "12m" => Ok(Band::Band12m),
            "10m" => Ok(Band::Band10m),
            "6m" => Ok(Band::Band6m),
            "2m" => Ok(Band::Band2m),
            "70cm" => Ok(Band::Band70cm),
            _ => Err(format!("Unknown band: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_band_from_frequency() {
        assert_eq!(Band::from_frequency(14_074_000), Some(Band::Band20m));
        assert_eq!(Band::from_frequency(7_074_000), Some(Band::Band40m));
        assert_eq!(Band::from_frequency(144_200_000), Some(Band::Band2m));
        assert_eq!(Band::from_frequency(5_000_000), None);
    }

    #[test]
    fn test_band_properties() {
        assert!(Band::Band20m.is_hf());
        assert!(!Band::Band20m.is_vhf());
        assert!(Band::Band2m.is_vhf());
        assert!(Band::Band70cm.is_uhf());
    }

    #[test]
    fn test_ft8_frequencies() {
        assert_eq!(Band::Band20m.ft8_frequency(), Some(14_074_000));
        assert_eq!(Band::Band40m.ft8_frequency(), Some(7_074_000));
    }

    #[test]
    fn test_ft4_frequencies() {
        assert_eq!(Band::Band20m.ft4_frequency(), Some(14_080_000));
        assert_eq!(Band::Band40m.ft4_frequency(), Some(7_047_500));
        assert_eq!(Band::Band6m.ft4_frequency(), Some(50_318_000));
        // 60m has no standard FT4 frequency.
        assert_eq!(Band::Band60m.ft4_frequency(), None);
        // 160m / 2m / 70cm are not FT4 bands either.
        assert_eq!(Band::Band160m.ft4_frequency(), None);
        assert_eq!(Band::Band2m.ft4_frequency(), None);
    }

    #[test]
    fn test_dial_for_mode() {
        // FT4 mode picks the FT4 table.
        assert_eq!(Band::Band20m.dial_for(true), Some(14_080_000));
        assert_eq!(Band::Band40m.dial_for(true), Some(7_047_500));
        // FT8 mode picks the FT8 table (regression: unchanged).
        assert_eq!(Band::Band20m.dial_for(false), Some(14_074_000));
        assert_eq!(Band::Band40m.dial_for(false), Some(7_074_000));
        // dial_for(false) is identical to ft8_frequency for every band.
        for band in Band::all() {
            assert_eq!(band.dial_for(false), band.ft8_frequency());
            assert_eq!(band.dial_for(true), band.ft4_frequency());
        }
    }

    #[test]
    fn test_band_serialization() {
        let band = Band::Band20m;
        let json = serde_json::to_string(&band).unwrap();
        assert_eq!(json, "\"20m\"");

        let decoded: Band = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, band);
    }
}
