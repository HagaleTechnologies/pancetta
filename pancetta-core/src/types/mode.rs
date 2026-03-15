//! Unified operating mode definition for amateur radio

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Operating mode enumeration - centralized definition
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Mode {
    /// Upper Sideband
    USB,
    /// Lower Sideband
    LSB,
    /// Continuous Wave (Morse Code)
    CW,
    /// CW Reverse
    CWR,
    /// Amplitude Modulation
    AM,
    /// Frequency Modulation
    FM,
    /// Wide FM
    WFM,
    /// Radio Teletype
    RTTY,
    /// RTTY Reverse
    RTTYR,
    /// Phase Shift Keying 31 baud
    PSK31,
    /// Phase Shift Keying 63 baud
    PSK63,
    /// Phase Shift Keying 125 baud
    PSK125,
    /// FT8 Digital Mode
    FT8,
    /// FT4 Digital Mode
    FT4,
    /// JS8 Digital Mode
    JS8,
    /// WSPR Beacon Mode
    WSPR,
    /// Packet Radio
    PACKET,
    /// Digital Mobile Radio
    DMR,
    /// D-STAR Digital Voice
    DSTAR,
    /// System Fusion Digital Voice
    YSF,
}

impl Mode {
    /// Get the default bandwidth for this mode in Hz
    pub fn default_bandwidth(&self) -> Option<u32> {
        match self {
            Mode::CW | Mode::CWR => Some(500),
            Mode::USB | Mode::LSB => Some(2700),
            Mode::AM => Some(6000),
            Mode::FM => Some(15000),
            Mode::WFM => Some(150000),
            Mode::RTTY | Mode::RTTYR => Some(850),
            Mode::PSK31 => Some(31),
            Mode::PSK63 => Some(63),
            Mode::PSK125 => Some(125),
            Mode::FT8 => Some(50),
            Mode::FT4 => Some(90),
            Mode::JS8 => Some(50),
            Mode::WSPR => Some(6),
            Mode::PACKET => Some(20000),
            Mode::DMR | Mode::DSTAR | Mode::YSF => Some(12500),
        }
    }

    /// Check if this is a digital mode
    pub fn is_digital(&self) -> bool {
        matches!(
            self,
            Mode::RTTY
                | Mode::RTTYR
                | Mode::PSK31
                | Mode::PSK63
                | Mode::PSK125
                | Mode::FT8
                | Mode::FT4
                | Mode::JS8
                | Mode::WSPR
                | Mode::PACKET
                | Mode::DMR
                | Mode::DSTAR
                | Mode::YSF
        )
    }

    /// Check if this is a voice mode
    pub fn is_voice(&self) -> bool {
        matches!(
            self,
            Mode::USB
                | Mode::LSB
                | Mode::AM
                | Mode::FM
                | Mode::WFM
                | Mode::DMR
                | Mode::DSTAR
                | Mode::YSF
        )
    }

    /// Check if this is a CW mode
    pub fn is_cw(&self) -> bool {
        matches!(self, Mode::CW | Mode::CWR)
    }

    /// Get all available modes
    pub fn all() -> Vec<Mode> {
        vec![
            Mode::USB,
            Mode::LSB,
            Mode::CW,
            Mode::CWR,
            Mode::AM,
            Mode::FM,
            Mode::RTTY,
            Mode::PSK31,
            Mode::FT8,
            Mode::FT4,
            Mode::JS8,
            Mode::WSPR,
        ]
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "USB" => Ok(Mode::USB),
            "LSB" => Ok(Mode::LSB),
            "CW" => Ok(Mode::CW),
            "CWR" => Ok(Mode::CWR),
            "AM" => Ok(Mode::AM),
            "FM" => Ok(Mode::FM),
            "WFM" => Ok(Mode::WFM),
            "RTTY" => Ok(Mode::RTTY),
            "RTTYR" => Ok(Mode::RTTYR),
            "PSK31" | "PSK" => Ok(Mode::PSK31),
            "PSK63" => Ok(Mode::PSK63),
            "PSK125" => Ok(Mode::PSK125),
            "FT8" => Ok(Mode::FT8),
            "FT4" => Ok(Mode::FT4),
            "JS8" | "JS8CALL" => Ok(Mode::JS8),
            "WSPR" => Ok(Mode::WSPR),
            "PACKET" | "PKT" => Ok(Mode::PACKET),
            "DMR" => Ok(Mode::DMR),
            "DSTAR" | "D-STAR" => Ok(Mode::DSTAR),
            "YSF" | "C4FM" => Ok(Mode::YSF),
            _ => Err(format!("Unknown mode: {}", s)),
        }
    }
}

impl Default for Mode {
    fn default() -> Self {
        Mode::USB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_parsing() {
        assert_eq!("USB".parse::<Mode>().unwrap(), Mode::USB);
        assert_eq!("ft8".parse::<Mode>().unwrap(), Mode::FT8);
        assert_eq!("PSK".parse::<Mode>().unwrap(), Mode::PSK31);
    }

    #[test]
    fn test_mode_properties() {
        assert!(Mode::FT8.is_digital());
        assert!(!Mode::FT8.is_voice());
        assert!(Mode::USB.is_voice());
        assert!(Mode::CW.is_cw());
    }

    #[test]
    fn test_mode_bandwidth() {
        assert_eq!(Mode::FT8.default_bandwidth(), Some(50));
        assert_eq!(Mode::USB.default_bandwidth(), Some(2700));
        assert_eq!(Mode::CW.default_bandwidth(), Some(500));
    }

    #[test]
    fn test_mode_serialization() {
        let mode = Mode::FT8;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"FT8\"");

        let decoded: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mode);
    }
}
