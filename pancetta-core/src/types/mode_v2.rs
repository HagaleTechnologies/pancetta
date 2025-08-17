//! Thread-safe mode implementation with custom variant support
//!
//! This module provides a thread-safe Mode type that supports both standard
//! amateur radio modes and custom mode strings while maintaining Send+Sync traits.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;

/// Thread-safe mode value that supports both standard and custom modes
#[derive(Clone)]
pub struct ModeValue {
    inner: Arc<ModeKind>,
}

/// Internal mode representation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModeKind {
    Standard(StandardMode),
    Custom(String),
}

/// Standard amateur radio operating modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum StandardMode {
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

impl ModeValue {
    /// Create a new standard mode
    pub fn standard(mode: StandardMode) -> Self {
        Self {
            inner: Arc::new(ModeKind::Standard(mode)),
        }
    }

    /// Create a new custom mode
    pub fn custom<S: Into<String>>(name: S) -> Self {
        Self {
            inner: Arc::new(ModeKind::Custom(name.into())),
        }
    }

    /// Get the mode as a standard mode if it is one
    pub fn as_standard(&self) -> Option<StandardMode> {
        match &*self.inner {
            ModeKind::Standard(mode) => Some(*mode),
            _ => None,
        }
    }

    /// Get the mode as a custom string if it is one
    pub fn as_custom(&self) -> Option<&str> {
        match &*self.inner {
            ModeKind::Custom(name) => Some(name.as_str()),
            _ => None,
        }
    }

    /// Check if this is a standard mode
    pub fn is_standard(&self) -> bool {
        matches!(&*self.inner, ModeKind::Standard(_))
    }

    /// Check if this is a custom mode
    pub fn is_custom(&self) -> bool {
        matches!(&*self.inner, ModeKind::Custom(_))
    }

    /// Get the mode name as a string
    pub fn name(&self) -> String {
        match &*self.inner {
            ModeKind::Standard(mode) => format!("{:?}", mode),
            ModeKind::Custom(name) => name.clone(),
        }
    }

    /// Get the default bandwidth for this mode in Hz
    pub fn default_bandwidth(&self) -> Option<u32> {
        match &*self.inner {
            ModeKind::Standard(mode) => mode.default_bandwidth(),
            ModeKind::Custom(name) => {
                // Heuristic for custom modes based on name
                let upper = name.to_uppercase();
                if upper.contains("OLIVIA") {
                    Some(500)
                } else if upper.contains("VARA") {
                    Some(2300)
                } else if upper.contains("MFSK") {
                    Some(500)
                } else if upper.contains("THOR") {
                    Some(250)
                } else if upper.contains("CONTESTIA") {
                    Some(500)
                } else {
                    None
                }
            }
        }
    }

    /// Check if this is a digital mode
    pub fn is_digital(&self) -> bool {
        match &*self.inner {
            ModeKind::Standard(mode) => mode.is_digital(),
            ModeKind::Custom(name) => {
                // Heuristic for custom modes
                let upper = name.to_uppercase();
                upper.contains("OLIVIA")
                    || upper.contains("VARA")
                    || upper.contains("MFSK")
                    || upper.contains("THOR")
                    || upper.contains("CONTESTIA")
                    || upper.contains("DOMINO")
                    || upper.contains("ROS")
            }
        }
    }

    /// Check if this is a voice mode
    pub fn is_voice(&self) -> bool {
        match &*self.inner {
            ModeKind::Standard(mode) => mode.is_voice(),
            ModeKind::Custom(name) => {
                let upper = name.to_uppercase();
                upper.contains("SSB")
                    || upper.contains("VOICE")
                    || upper.contains("PHONE")
                    || upper.contains("FREEDV")
            }
        }
    }

    /// Check if this is a CW mode
    pub fn is_cw(&self) -> bool {
        match &*self.inner {
            ModeKind::Standard(mode) => mode.is_cw(),
            ModeKind::Custom(name) => {
                let upper = name.to_uppercase();
                upper == "CW" || upper.contains("MORSE")
            }
        }
    }

    /// Get all standard modes
    pub fn all_standard() -> Vec<ModeValue> {
        StandardMode::all()
            .into_iter()
            .map(ModeValue::standard)
            .collect()
    }
}

impl StandardMode {
    /// Get the default bandwidth for this mode in Hz
    pub fn default_bandwidth(&self) -> Option<u32> {
        match self {
            StandardMode::CW | StandardMode::CWR => Some(500),
            StandardMode::USB | StandardMode::LSB => Some(2700),
            StandardMode::AM => Some(6000),
            StandardMode::FM => Some(15000),
            StandardMode::WFM => Some(150000),
            StandardMode::RTTY | StandardMode::RTTYR => Some(850),
            StandardMode::PSK31 => Some(31),
            StandardMode::PSK63 => Some(63),
            StandardMode::PSK125 => Some(125),
            StandardMode::FT8 => Some(50),
            StandardMode::FT4 => Some(90),
            StandardMode::JS8 => Some(50),
            StandardMode::WSPR => Some(6),
            StandardMode::PACKET => Some(20000),
            StandardMode::DMR | StandardMode::DSTAR | StandardMode::YSF => Some(12500),
        }
    }

    /// Check if this is a digital mode
    pub fn is_digital(&self) -> bool {
        matches!(
            self,
            StandardMode::RTTY
                | StandardMode::RTTYR
                | StandardMode::PSK31
                | StandardMode::PSK63
                | StandardMode::PSK125
                | StandardMode::FT8
                | StandardMode::FT4
                | StandardMode::JS8
                | StandardMode::WSPR
                | StandardMode::PACKET
                | StandardMode::DMR
                | StandardMode::DSTAR
                | StandardMode::YSF
        )
    }

    /// Check if this is a voice mode
    pub fn is_voice(&self) -> bool {
        matches!(
            self,
            StandardMode::USB
                | StandardMode::LSB
                | StandardMode::AM
                | StandardMode::FM
                | StandardMode::WFM
                | StandardMode::DMR
                | StandardMode::DSTAR
                | StandardMode::YSF
        )
    }

    /// Check if this is a CW mode
    pub fn is_cw(&self) -> bool {
        matches!(self, StandardMode::CW | StandardMode::CWR)
    }

    /// Get all available standard modes
    pub fn all() -> Vec<StandardMode> {
        vec![
            StandardMode::USB,
            StandardMode::LSB,
            StandardMode::CW,
            StandardMode::CWR,
            StandardMode::AM,
            StandardMode::FM,
            StandardMode::RTTY,
            StandardMode::PSK31,
            StandardMode::FT8,
            StandardMode::FT4,
            StandardMode::JS8,
            StandardMode::WSPR,
        ]
    }
}

// Trait implementations for ModeValue

impl fmt::Debug for ModeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.inner {
            ModeKind::Standard(mode) => write!(f, "Mode::{:?}", mode),
            ModeKind::Custom(name) => write!(f, "Mode::Custom(\"{}\")", name),
        }
    }
}

impl fmt::Display for ModeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl PartialEq for ModeValue {
    fn eq(&self, other: &Self) -> bool {
        // First try pointer equality for efficiency
        Arc::ptr_eq(&self.inner, &other.inner) || *self.inner == *other.inner
    }
}

impl Eq for ModeValue {}

impl Hash for ModeValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

impl FromStr for ModeValue {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Try to parse as standard mode first
        match s.to_uppercase().as_str() {
            "USB" => Ok(ModeValue::standard(StandardMode::USB)),
            "LSB" => Ok(ModeValue::standard(StandardMode::LSB)),
            "CW" => Ok(ModeValue::standard(StandardMode::CW)),
            "CWR" => Ok(ModeValue::standard(StandardMode::CWR)),
            "AM" => Ok(ModeValue::standard(StandardMode::AM)),
            "FM" => Ok(ModeValue::standard(StandardMode::FM)),
            "WFM" => Ok(ModeValue::standard(StandardMode::WFM)),
            "RTTY" => Ok(ModeValue::standard(StandardMode::RTTY)),
            "RTTYR" => Ok(ModeValue::standard(StandardMode::RTTYR)),
            "PSK31" | "PSK" => Ok(ModeValue::standard(StandardMode::PSK31)),
            "PSK63" => Ok(ModeValue::standard(StandardMode::PSK63)),
            "PSK125" => Ok(ModeValue::standard(StandardMode::PSK125)),
            "FT8" => Ok(ModeValue::standard(StandardMode::FT8)),
            "FT4" => Ok(ModeValue::standard(StandardMode::FT4)),
            "JS8" | "JS8CALL" => Ok(ModeValue::standard(StandardMode::JS8)),
            "WSPR" => Ok(ModeValue::standard(StandardMode::WSPR)),
            "PACKET" | "PKT" => Ok(ModeValue::standard(StandardMode::PACKET)),
            "DMR" => Ok(ModeValue::standard(StandardMode::DMR)),
            "DSTAR" | "D-STAR" => Ok(ModeValue::standard(StandardMode::DSTAR)),
            "YSF" | "C4FM" => Ok(ModeValue::standard(StandardMode::YSF)),
            _ => {
                // Accept as custom mode
                Ok(ModeValue::custom(s))
            }
        }
    }
}

impl Default for ModeValue {
    fn default() -> Self {
        ModeValue::standard(StandardMode::USB)
    }
}

// Serialization support
impl Serialize for ModeValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.name())
    }
}

impl<'de> Deserialize<'de> for ModeValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ModeValue::from_str(&s).map_err(serde::de::Error::custom)
    }
}

// Migration support from old Mode enum
impl From<super::mode::Mode> for ModeValue {
    fn from(mode: super::mode::Mode) -> Self {
        use super::mode::Mode;
        match mode {
            Mode::USB => ModeValue::standard(StandardMode::USB),
            Mode::LSB => ModeValue::standard(StandardMode::LSB),
            Mode::CW => ModeValue::standard(StandardMode::CW),
            Mode::CWR => ModeValue::standard(StandardMode::CWR),
            Mode::AM => ModeValue::standard(StandardMode::AM),
            Mode::FM => ModeValue::standard(StandardMode::FM),
            Mode::WFM => ModeValue::standard(StandardMode::WFM),
            Mode::RTTY => ModeValue::standard(StandardMode::RTTY),
            Mode::RTTYR => ModeValue::standard(StandardMode::RTTYR),
            Mode::PSK31 => ModeValue::standard(StandardMode::PSK31),
            Mode::PSK63 => ModeValue::standard(StandardMode::PSK63),
            Mode::PSK125 => ModeValue::standard(StandardMode::PSK125),
            Mode::FT8 => ModeValue::standard(StandardMode::FT8),
            Mode::FT4 => ModeValue::standard(StandardMode::FT4),
            Mode::JS8 => ModeValue::standard(StandardMode::JS8),
            Mode::WSPR => ModeValue::standard(StandardMode::WSPR),
            Mode::PACKET => ModeValue::standard(StandardMode::PACKET),
            Mode::DMR => ModeValue::standard(StandardMode::DMR),
            Mode::DSTAR => ModeValue::standard(StandardMode::DSTAR),
            Mode::YSF => ModeValue::standard(StandardMode::YSF),
        }
    }
}

// Safety: ModeValue is safe to send between threads
unsafe impl Send for ModeValue {}
unsafe impl Sync for ModeValue {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_mode_creation() {
        let mode = ModeValue::standard(StandardMode::FT8);
        assert!(mode.is_standard());
        assert!(!mode.is_custom());
        assert_eq!(mode.as_standard(), Some(StandardMode::FT8));
        assert_eq!(mode.name(), "FT8");
    }

    #[test]
    fn test_custom_mode_creation() {
        let mode = ModeValue::custom("OLIVIA-16/500");
        assert!(!mode.is_standard());
        assert!(mode.is_custom());
        assert_eq!(mode.as_custom(), Some("OLIVIA-16/500"));
        assert_eq!(mode.name(), "OLIVIA-16/500");
    }

    #[test]
    fn test_mode_parsing() {
        assert_eq!(
            "USB".parse::<ModeValue>().unwrap(),
            ModeValue::standard(StandardMode::USB)
        );
        assert_eq!(
            "ft8".parse::<ModeValue>().unwrap(),
            ModeValue::standard(StandardMode::FT8)
        );
        assert_eq!(
            "OLIVIA".parse::<ModeValue>().unwrap(),
            ModeValue::custom("OLIVIA")
        );
    }

    #[test]
    fn test_mode_properties() {
        let ft8 = ModeValue::standard(StandardMode::FT8);
        assert!(ft8.is_digital());
        assert!(!ft8.is_voice());

        let usb = ModeValue::standard(StandardMode::USB);
        assert!(usb.is_voice());
        assert!(!usb.is_digital());

        let olivia = ModeValue::custom("OLIVIA-16/500");
        assert!(olivia.is_digital());
        assert!(!olivia.is_voice());
    }

    #[test]
    fn test_mode_bandwidth() {
        assert_eq!(
            ModeValue::standard(StandardMode::FT8).default_bandwidth(),
            Some(50)
        );
        assert_eq!(
            ModeValue::standard(StandardMode::USB).default_bandwidth(),
            Some(2700)
        );
        assert_eq!(
            ModeValue::custom("OLIVIA").default_bandwidth(),
            Some(500)
        );
    }

    #[test]
    fn test_mode_serialization() {
        let mode = ModeValue::standard(StandardMode::FT8);
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"FT8\"");

        let decoded: ModeValue = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mode);

        let custom = ModeValue::custom("OLIVIA-16/500");
        let json = serde_json::to_string(&custom).unwrap();
        assert_eq!(json, "\"OLIVIA-16/500\"");

        let decoded: ModeValue = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, custom);
    }

    #[test]
    fn test_thread_safety() {
        use std::thread;

        let mode = ModeValue::custom("VARA-HF");
        let mode_clone = mode.clone();

        let handle = thread::spawn(move || {
            assert_eq!(mode_clone.name(), "VARA-HF");
            mode_clone
        });

        let result = handle.join().unwrap();
        assert_eq!(result, mode);
    }

    #[test]
    fn test_arc_efficiency() {
        let mode1 = ModeValue::custom("OLIVIA-16/500");
        let mode2 = mode1.clone();

        // Should share the same Arc
        assert_eq!(mode1, mode2);

        // Cloning should be cheap
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = mode1.clone();
        }
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 10); // Should be very fast
    }

    #[test]
    fn test_migration_from_old_mode() {
        let old_mode = super::mode::Mode::FT8;
        let new_mode: ModeValue = old_mode.into();
        assert_eq!(new_mode, ModeValue::standard(StandardMode::FT8));
    }
}