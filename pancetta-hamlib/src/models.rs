//! Supported rig models and their capabilities
//!
//! This module defines the amateur radio transceiver models supported by hamlib
//! and their specific capabilities for optimal integration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use string_cache::DefaultAtom as Atom;

// Re-export unified types from pancetta-core
pub use pancetta_core::{Band, Mode};

// Band type is imported from pancetta-core
/// Define a trait to extend Band functionality for hamlib
pub trait BandExt {
    /// Get the name of the band for hamlib display
    fn name(&self) -> &'static str;
}

impl BandExt for Band {
    fn name(&self) -> &'static str {
        match self {
            Band::Band160m => "160m",
            Band::Band80m => "80m",
            Band::Band60m => "60m",
            Band::Band40m => "40m",
            Band::Band30m => "30m",
            Band::Band20m => "20m",
            Band::Band17m => "17m",
            Band::Band15m => "15m",
            Band::Band12m => "12m",
            Band::Band10m => "10m",
            Band::Band6m => "6m",
            Band::Band2m => "2m",
            Band::Band70cm => "70cm",
            Band::Custom(_, _) => "custom",
        }
    }
}

/// Mode type is imported from pancetta-core
/// Define hamlib-specific mode for internal use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HamlibMode {
    /// Amplitude Modulation
    AM,
    /// Continuous Wave (Morse Code)
    CW,
    /// Upper Sideband
    USB,
    /// Lower Sideband
    LSB,
    /// Radio Teletype
    RTTY,
    /// Frequency Modulation
    FM,
    /// Wideband FM
    WFM,
    /// CW Reverse
    CWR,
    /// RTTY Reverse
    RTTYR,
    /// AM Synchronous
    AMS,
    /// Packet LSB
    PKTLSB,
    /// Packet USB
    PKTUSB,
    /// Packet FM
    PKTFM,
    /// Enhanced Carrier Single Sideband USB
    ECSSUSB,
    /// Enhanced Carrier Single Sideband LSB
    ECSSLSB,
    /// FT8 digital mode
    FT8,
    /// FT4 digital mode
    FT4,
    /// Unknown/custom mode
    Unknown,
}

impl HamlibMode {
    /// Convert HamlibMode to unified Mode
    pub fn to_unified_mode(&self) -> Mode {
        match self {
            HamlibMode::AM => Mode::AM,
            HamlibMode::CW => Mode::CW,
            HamlibMode::USB => Mode::USB,
            HamlibMode::LSB => Mode::LSB,
            HamlibMode::RTTY => Mode::RTTY,
            HamlibMode::FM => Mode::FM,
            HamlibMode::WFM => Mode::WFM,
            HamlibMode::CWR => Mode::CWR,
            HamlibMode::RTTYR => Mode::RTTYR,
            HamlibMode::FT8 => Mode::FT8,
            HamlibMode::FT4 => Mode::FT4,
            // Map hamlib-specific modes to closest unified equivalent or Custom
            HamlibMode::AMS => Mode::AM,
            HamlibMode::PKTLSB => Mode::LSB,
            HamlibMode::PKTUSB => Mode::USB,
            HamlibMode::PKTFM => Mode::FM,
            HamlibMode::ECSSUSB => Mode::USB,
            HamlibMode::ECSSLSB => Mode::LSB,
            HamlibMode::Unknown => Mode::USB, // Default to USB for unknown modes
        }
    }
}

/// Define a trait to extend Mode functionality for hamlib
pub trait ModeExt {
    /// Convert unified Mode to HamlibMode (best match)
    fn to_hamlib_mode(&self) -> HamlibMode;
    /// Get default passband width for this mode in Hz
    fn default_width(&self) -> Option<i32>;
    /// Get mode name
    fn name(&self) -> &'static str;
}

impl ModeExt for Mode {
    /// Convert unified Mode to HamlibMode (best match)
    fn to_hamlib_mode(&self) -> HamlibMode {
        match self {
            Mode::AM => HamlibMode::AM,
            Mode::CW => HamlibMode::CW,
            Mode::USB => HamlibMode::USB,
            Mode::LSB => HamlibMode::LSB,
            Mode::RTTY => HamlibMode::RTTY,
            Mode::FM => HamlibMode::FM,
            Mode::WFM => HamlibMode::WFM,
            Mode::CWR => HamlibMode::CWR,
            Mode::RTTYR => HamlibMode::RTTYR,
            Mode::FT8 => HamlibMode::FT8,
            Mode::FT4 => HamlibMode::FT4,
            Mode::PSK31 | Mode::PSK63 | Mode::PSK125 => HamlibMode::PKTUSB,
            Mode::PACKET => HamlibMode::PKTFM,
            Mode::JS8 => HamlibMode::USB,  // JS8 is USB-based
            Mode::WSPR => HamlibMode::USB, // WSPR is USB-based
            // No Custom variant in Mode enum, handle all specific cases above
            // Map other digital modes to USB (most common)
            _ => HamlibMode::USB,
        }
    }

    /// Get default passband width for this mode in Hz
    fn default_width(&self) -> Option<i32> {
        match self {
            Mode::CW | Mode::CWR => Some(500),
            Mode::USB | Mode::LSB => Some(2400),
            Mode::AM => Some(6000),
            Mode::FM => Some(12000),
            Mode::WFM => Some(150000),
            Mode::RTTY | Mode::RTTYR => Some(250),
            Mode::PSK31 | Mode::PSK63 | Mode::PSK125 => Some(2400),
            Mode::PACKET => Some(12000),
            Mode::FT8 | Mode::FT4 => Some(3000),
            _ => None,
        }
    }

    /// Get mode name
    fn name(&self) -> &'static str {
        match self {
            Mode::AM => "AM",
            Mode::CW => "CW",
            Mode::USB => "USB",
            Mode::LSB => "LSB",
            Mode::RTTY => "RTTY",
            Mode::FM => "FM",
            Mode::WFM => "WFM",
            Mode::CWR => "CWR",
            Mode::RTTYR => "RTTYR",
            Mode::PSK31 => "PSK31",
            Mode::PSK63 => "PSK63",
            Mode::PSK125 => "PSK125",
            Mode::FT8 => "FT8",
            Mode::FT4 => "FT4",
            Mode::JS8 => "JS8",
            Mode::WSPR => "WSPR",
            Mode::PACKET => "PACKET",
            Mode::DMR => "DMR",
            Mode::DSTAR => "DSTAR",
            Mode::YSF => "YSF",
        }
    }
}

/// VFO (Variable Frequency Oscillator) designation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Vfo {
    /// Current VFO
    Current,
    /// VFO A
    A,
    /// VFO B
    B,
    /// Memory channel
    Memory,
}

/// Rig capabilities and features
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigCapabilities {
    /// Supported modes
    pub modes: Vec<Mode>,
    /// Supported frequency ranges
    pub frequency_ranges: Vec<(u64, u64)>,
    /// Has dual VFOs
    pub has_dual_vfo: bool,
    /// Has memory channels
    pub has_memory: bool,
    /// Number of memory channels
    pub memory_channels: Option<u32>,
    /// Has scanning capability
    pub has_scanning: bool,
    /// Has S-meter reading
    pub has_smeter: bool,
    /// Has SWR reading
    pub has_swr: bool,
    /// Has power level control
    pub has_power_control: bool,
    /// Has antenna switching
    pub has_antenna_switch: bool,
    /// Has IF shift
    pub has_if_shift: bool,
    /// Has noise reduction
    pub has_noise_reduction: bool,
    /// Supported bands
    pub bands: Vec<Band>,
    /// Default baud rate for CAT control
    pub default_baud_rate: u32,
    /// Connection timeout in milliseconds
    pub default_timeout: u32,
}

impl Default for RigCapabilities {
    fn default() -> Self {
        Self {
            modes: vec![Mode::USB, Mode::LSB, Mode::CW, Mode::FM],
            frequency_ranges: vec![(1_800_000, 30_000_000)],
            has_dual_vfo: true,
            has_memory: true,
            memory_channels: Some(100),
            has_scanning: false,
            has_smeter: true,
            has_swr: false,
            has_power_control: true,
            has_antenna_switch: false,
            has_if_shift: false,
            has_noise_reduction: false,
            bands: vec![
                Band::Band160m,
                Band::Band80m,
                Band::Band40m,
                Band::Band20m,
                Band::Band15m,
                Band::Band10m,
            ],
            default_baud_rate: 9600,
            default_timeout: 2000,
        }
    }
}

/// Popular amateur radio transceiver models
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RigModelType {
    /// Dummy rig for testing
    Dummy,
    /// Network rigctl daemon
    NetRigctl,
    /// Yaesu FT-991A
    YaesuFT991A,
    /// Yaesu FT-dx10
    YaesuFTdx10,
    /// Yaesu FT-818
    YaesuFT818,
    /// Icom IC-7300
    IcomIC7300,
    /// Icom IC-7610
    IcomIC7610,
    /// Icom IC-705
    IcomIC705,
    /// Kenwood TS-590SG
    KenwoodTS590SG,
    /// Kenwood TS-890S
    KenwoodTS890S,
    /// Elecraft K3S
    ElecraftK3S,
    /// Elecraft KX3
    ElecraftKX3,
    /// FlexRadio 6000 series
    FlexRadio6000,
    /// Unknown/unsupported model
    Unknown,
}

impl RigModelType {
    /// Get the hamlib numeric model ID (used to spawn rigctld -m).
    ///
    /// Returns `None` for `Unknown`, since the unknown variant doesn't
    /// know which model it is. Callers (the coordinator) treat that as
    /// "fall back to manual rigctld config" rather than a hard failure.
    pub fn hamlib_id(&self) -> Option<u32> {
        Some(match self {
            RigModelType::Dummy => 1,
            RigModelType::NetRigctl => 2,
            RigModelType::YaesuFT991A => 1035,
            RigModelType::YaesuFTdx10 => 1045,
            RigModelType::YaesuFT818 => 1049,
            RigModelType::IcomIC7300 => 3074,
            RigModelType::IcomIC7610 => 3076,
            RigModelType::IcomIC705 => 3085,
            RigModelType::KenwoodTS590SG => 2030,
            RigModelType::KenwoodTS890S => 2042,
            RigModelType::ElecraftK3S => 2073,
            RigModelType::ElecraftKX3 => 2074,
            RigModelType::FlexRadio6000 => 2025,
            RigModelType::Unknown => return None,
        })
    }

    /// Get manufacturer name
    pub fn manufacturer(&self) -> &'static str {
        match self {
            RigModelType::Dummy => "Hamlib",
            RigModelType::NetRigctl => "Hamlib",
            RigModelType::YaesuFT991A | RigModelType::YaesuFTdx10 | RigModelType::YaesuFT818 => {
                "Yaesu"
            }
            RigModelType::IcomIC7300 | RigModelType::IcomIC7610 | RigModelType::IcomIC705 => "Icom",
            RigModelType::KenwoodTS590SG | RigModelType::KenwoodTS890S => "Kenwood",
            RigModelType::ElecraftK3S | RigModelType::ElecraftKX3 => "Elecraft",
            RigModelType::FlexRadio6000 => "FlexRadio",
            RigModelType::Unknown => "Unknown",
        }
    }

    /// Get model name
    pub fn model_name(&self) -> &'static str {
        match self {
            RigModelType::Dummy => "Dummy",
            RigModelType::NetRigctl => "Network rigctl",
            RigModelType::YaesuFT991A => "FT-991A",
            RigModelType::YaesuFTdx10 => "FT-dx10",
            RigModelType::YaesuFT818 => "FT-818",
            RigModelType::IcomIC7300 => "IC-7300",
            RigModelType::IcomIC7610 => "IC-7610",
            RigModelType::IcomIC705 => "IC-705",
            RigModelType::KenwoodTS590SG => "TS-590SG",
            RigModelType::KenwoodTS890S => "TS-890S",
            RigModelType::ElecraftK3S => "K3S",
            RigModelType::ElecraftKX3 => "KX3",
            RigModelType::FlexRadio6000 => "6000 Series",
            RigModelType::Unknown => "Unknown",
        }
    }

    /// Get rig capabilities
    pub fn capabilities(&self) -> RigCapabilities {
        match self {
            RigModelType::Dummy => RigCapabilities::default(),
            RigModelType::NetRigctl => RigCapabilities::default(),

            RigModelType::YaesuFT991A => RigCapabilities {
                modes: vec![
                    Mode::LSB,
                    Mode::USB,
                    Mode::CW,
                    Mode::FM,
                    Mode::AM,
                    Mode::RTTY,
                    Mode::PACKET,
                ], // Use Mode::PACKET instead of PKT variants
                frequency_ranges: vec![
                    (30000, 56000000),
                    (76000000, 108000000),
                    (118000000, 164000000),
                    (420000000, 450000000),
                ],
                has_dual_vfo: true,
                has_memory: true,
                memory_channels: Some(500),
                has_scanning: true,
                has_smeter: true,
                has_swr: true,
                has_power_control: true,
                has_antenna_switch: true,
                has_if_shift: true,
                has_noise_reduction: true,
                bands: vec![
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
                ],
                default_baud_rate: 38400,
                default_timeout: 2000,
            },

            RigModelType::IcomIC7300 => RigCapabilities {
                modes: vec![
                    Mode::LSB,
                    Mode::USB,
                    Mode::CW,
                    Mode::RTTY,
                    Mode::AM,
                    Mode::FM,
                    Mode::PACKET,
                ], // Use Mode::PACKET instead of PKT variants
                frequency_ranges: vec![(30000, 74800000)],
                has_dual_vfo: true,
                has_memory: true,
                memory_channels: Some(101),
                has_scanning: true,
                has_smeter: true,
                has_swr: true,
                has_power_control: true,
                has_antenna_switch: false,
                has_if_shift: true,
                has_noise_reduction: true,
                bands: vec![
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
                ],
                default_baud_rate: 19200,
                default_timeout: 2000,
            },

            _ => RigCapabilities::default(),
        }
    }
}

/// Registry of supported rig models
pub struct RigModelRegistry {
    models: HashMap<Atom, RigModelType>,
}

impl RigModelRegistry {
    /// Create new registry with common models
    pub fn new() -> Self {
        let mut models = HashMap::new();

        // Add popular models
        models.insert(Atom::from("dummy"), RigModelType::Dummy);
        models.insert(Atom::from("netrigctl"), RigModelType::NetRigctl);
        models.insert(Atom::from("ft991a"), RigModelType::YaesuFT991A);
        models.insert(Atom::from("ftdx10"), RigModelType::YaesuFTdx10);
        models.insert(Atom::from("ft818"), RigModelType::YaesuFT818);
        models.insert(Atom::from("ic7300"), RigModelType::IcomIC7300);
        models.insert(Atom::from("ic7610"), RigModelType::IcomIC7610);
        models.insert(Atom::from("ic705"), RigModelType::IcomIC705);
        models.insert(Atom::from("ts590sg"), RigModelType::KenwoodTS590SG);
        models.insert(Atom::from("ts890s"), RigModelType::KenwoodTS890S);
        models.insert(Atom::from("k3s"), RigModelType::ElecraftK3S);
        models.insert(Atom::from("kx3"), RigModelType::ElecraftKX3);
        models.insert(Atom::from("flex6000"), RigModelType::FlexRadio6000);

        Self { models }
    }

    /// Get model by name
    pub fn get_model(&self, name: &str) -> Option<&RigModelType> {
        self.models.get(&Atom::from(name.to_lowercase()))
    }

    /// List all available models
    pub fn list_models(&self) -> Vec<(&str, &RigModelType)> {
        self.models
            .iter()
            .map(|(name, model)| (name.as_ref(), model))
            .collect()
    }

    /// Add custom model
    pub fn add_model(&mut self, name: &str, model: RigModelType) {
        self.models.insert(Atom::from(name.to_lowercase()), model);
    }
}

impl Default for RigModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_band_frequency_detection() {
        // TODO: Add band frequency detection methods to BandExt trait
    }

    // Mode/VFO round-trip-through-hamlib tests removed alongside the
    // FFI bindings module they exercised. The rigctld TCP path uses
    // string-form modes ("USB", "FT8", ...) and VFO names directly,
    // not numeric hamlib constants.

    #[test]
    fn test_rig_model_registry() {
        let registry = RigModelRegistry::new();
        assert!(registry.get_model("ic7300").is_some());
        assert!(registry.get_model("ft991a").is_some());
        assert!(registry.get_model("nonexistent").is_none());
    }

    #[test]
    fn test_rig_capabilities() {
        let ft991a = RigModelType::YaesuFT991A;
        let caps = ft991a.capabilities();
        assert!(caps.has_dual_vfo);
        assert!(caps.has_scanning);
        assert!(caps.modes.contains(&Mode::USB));
        // Remove this line since we simplified the mode list
    }
}
