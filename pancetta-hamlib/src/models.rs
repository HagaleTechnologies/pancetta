//! Supported rig models and their capabilities
//!
//! This module defines the amateur radio transceiver models supported by hamlib
//! and their specific capabilities for optimal integration.

use serde::{Deserialize, Serialize};

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
///
/// This is not the supported-rig list. The rigs Pancetta starts `rigctld`
/// for, and their Hamlib model numbers, live in [`SUPPORTED_RIGS`].
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

/// Whether this repository records a hardware test of one rig capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareTest {
    /// Used on a real station with this rig, with the evidence recorded in
    /// [`SupportedRig::evidence`].
    Tested,
    /// No hardware test of this capability is recorded in the repository.
    Untested,
}

impl HardwareTest {
    /// The cell text shown in `docs/SUPPORTED_RIGS.md`.
    fn label(self) -> &'static str {
        match self {
            HardwareTest::Tested => "Tested",
            HardwareTest::Untested => "Untested",
        }
    }
}

/// One rig Pancetta starts `rigctld` for, as listed in `docs/SUPPORTED_RIGS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedRig {
    /// Manufacturer, for example `"Icom"`.
    pub manufacturer: &'static str,
    /// Model name as the manufacturer writes it, for example `"IC-7300"`.
    pub model: &'static str,
    /// Accepted `rig.model` values, as shown on the page. Matching ignores
    /// case, hyphens and spaces (see [`normalize_rig_model`]).
    pub config_names: &'static [&'static str],
    /// Hamlib model number passed to `rigctld -m`.
    pub hamlib_id: u32,
    /// CAT (frequency, split, PTT through `rigctld`) hardware-test status.
    pub cat: HardwareTest,
    /// Rig USB sound device (receive decode and transmit audio) hardware-test status.
    pub audio: HardwareTest,
    /// On-air FT8 transmit hardware-test status.
    pub tx: HardwareTest,
    /// Where the repository records the hardware test. Required when any
    /// capability is [`HardwareTest::Tested`].
    pub evidence: Option<&'static str>,
}

impl SupportedRig {
    /// Display name, manufacturer and model, for example `"Icom IC-7300"`.
    fn display_name(&self) -> String {
        format!("{} {}", self.manufacturer, self.model)
    }

    fn has_any_test(&self) -> bool {
        [self.cat, self.audio, self.tx].contains(&HardwareTest::Tested)
    }
}

const fn untested_rig(
    manufacturer: &'static str,
    model: &'static str,
    config_names: &'static [&'static str],
    catalog_macro: &'static str,
) -> SupportedRig {
    SupportedRig {
        manufacturer,
        model,
        config_names,
        // The ID is looked up from the vendored Hamlib catalog at compile
        // time (PAN-340), so this curated metadata table cannot drift from
        // the authoritative ID source.
        hamlib_id: crate::model_catalog::catalog_id(catalog_macro),
        cat: HardwareTest::Untested,
        audio: HardwareTest::Untested,
        tx: HardwareTest::Untested,
        evidence: None,
    }
}

/// The one table of rigs Pancetta starts `rigctld` for.
///
/// The coordinator's `rigctld` spawner, `pancetta doctor`, the TUI rig
/// picker's model check (`model_recognized`), `docs/SUPPORTED_RIGS.md` and
/// the curated legacy aliases of [`crate::model_catalog::resolve_model`]
/// all read this table. Each `hamlib_id` is a compile-time lookup into the
/// vendored Hamlib catalog by its upstream `RIG_MODEL_*` macro
/// ([`crate::model_catalog::catalog_id`]). After adding or changing
/// an entry, regenerate the page with
/// `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-hamlib supported_rigs_doc_is_current`
/// and pin the upstream Hamlib number in `hamlib_ids_match_upstream_riglist`.
/// Mark a capability [`HardwareTest::Tested`] only when the repository records
/// a hardware test, and name that record in `evidence`.
pub static SUPPORTED_RIGS: &[SupportedRig] = &[
    untested_rig("Icom", "IC-7300", &["IC-7300"], "RIG_MODEL_IC7300"),
    untested_rig("Icom", "IC-705", &["IC-705"], "RIG_MODEL_IC705"),
    untested_rig("Icom", "IC-7610", &["IC-7610"], "RIG_MODEL_IC7610"),
    untested_rig("Icom", "IC-7851", &["IC-7851"], "RIG_MODEL_IC785x"),
    untested_rig("Icom", "IC-9700", &["IC-9700"], "RIG_MODEL_IC9700"),
    untested_rig(
        "Kenwood",
        "TS-590S",
        &["TS-590", "TS-590S"],
        "RIG_MODEL_TS590S",
    ),
    untested_rig("Kenwood", "TS-590SG", &["TS-590SG"], "RIG_MODEL_TS590SG"),
    untested_rig(
        "Kenwood",
        "TS-890S",
        &["TS-890", "TS-890S"],
        "RIG_MODEL_TS890S",
    ),
    untested_rig("Yaesu", "FT-710", &["FT-710"], "RIG_MODEL_FT710"),
    untested_rig(
        "Yaesu",
        "FT-817",
        &["FT-817", "FT-817ND"],
        "RIG_MODEL_FT817",
    ),
    untested_rig("Yaesu", "FT-857", &["FT-857", "FT-857D"], "RIG_MODEL_FT857"),
    untested_rig("Yaesu", "FT-891", &["FT-891"], "RIG_MODEL_FT891"),
    untested_rig("Yaesu", "FT-991", &["FT-991", "FT-991A"], "RIG_MODEL_FT991"),
    SupportedRig {
        manufacturer: "Yaesu",
        model: "FTdx10",
        config_names: &["FTdx10"],
        hamlib_id: crate::model_catalog::catalog_id("RIG_MODEL_FTDX10"),
        cat: HardwareTest::Tested,
        audio: HardwareTest::Tested,
        tx: HardwareTest::Tested,
        evidence: Some(
            "CAT and PTT radio-qualified; TX validated 2026-04-26 (clean ALC, \
             PSKReporter spots across NA and EU); manual on-air QSOs since 2026-09.",
        ),
    },
    untested_rig("Yaesu", "FTdx101D", &["FTdx101D"], "RIG_MODEL_FTDX101D"),
    untested_rig("Yaesu", "FTdx101MP", &["FTdx101MP"], "RIG_MODEL_FTDX101MP"),
];

/// Rigs listed first on the supported-rig page, by [`SupportedRig::model`]:
/// the most common remote-operation targets. The rest follow sorted by
/// manufacturer and model.
const LEAD_RIG_MODELS: &[&str] = &["IC-7300", "IC-705"];

/// Normalize a `rig.model` value for lookup: lowercase, with hyphens and
/// spaces removed, so `"IC-7300"`, `"ic7300"` and `"ic 7300"` match.
pub fn normalize_rig_model(name: &str) -> String {
    name.to_lowercase().replace(['-', ' '], "")
}

/// Look up a configured `rig.model` value in [`SUPPORTED_RIGS`].
///
/// This is the *curated metadata* lookup (capabilities, qualification,
/// page row), not the acceptance test: any Hamlib model — catalog name,
/// manufacturer-qualified name or numeric ID — resolves through
/// [`crate::model_catalog::resolve_model`].
pub fn find_supported_rig(name: &str) -> Option<&'static SupportedRig> {
    let wanted = normalize_rig_model(name);
    if wanted.is_empty() {
        return None;
    }
    SUPPORTED_RIGS.iter().find(|rig| {
        rig.config_names
            .iter()
            .any(|n| normalize_rig_model(n) == wanted)
    })
}

/// [`SUPPORTED_RIGS`] in page order: [`LEAD_RIG_MODELS`] first, then the
/// rest sorted by manufacturer and model.
fn supported_rigs_in_page_order() -> Vec<&'static SupportedRig> {
    let mut lead: Vec<&'static SupportedRig> = LEAD_RIG_MODELS
        .iter()
        .filter_map(|m| SUPPORTED_RIGS.iter().find(|r| r.model == *m))
        .collect();
    let mut rest: Vec<&'static SupportedRig> = SUPPORTED_RIGS
        .iter()
        .filter(|r| !LEAD_RIG_MODELS.contains(&r.model))
        .collect();
    rest.sort_by(|a, b| (a.manufacturer, a.model).cmp(&(b.manufacturer, b.model)));
    lead.append(&mut rest);
    lead
}

/// Render the generated `docs/SUPPORTED_RIGS.md` content from [`SUPPORTED_RIGS`].
pub fn render_supported_rigs_markdown() -> String {
    let rigs = supported_rigs_in_page_order();
    let tested: Vec<String> = rigs
        .iter()
        .filter(|r| r.has_any_test())
        .map(|r| r.display_name())
        .collect();

    let mut out = String::from(
        "<!-- GENERATED FILE - do not edit by hand.\n     \
         Source: pancetta-hamlib/src/models.rs (SUPPORTED_RIGS).\n     \
         Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-hamlib supported_rigs_doc_is_current -->\n\n\
         # Supported rigs\n\n\
         Pancetta controls a rig through Hamlib's `rigctld`.\n\
         Set `rig.model` in `~/.pancetta/pancetta.toml` to a model name from the bundled \
         Hamlib 4.5.5 catalog or a numeric Hamlib model ID, for example \"IC-7300\" or \"1045\".\n\
         Case, hyphens and spaces are ignored when matching names. Ambiguous names require a numeric ID.\n\
         Run `rigctl -l` to see the models your installed Hamlib supports. A numeric ID also \
         permits models newer than the bundled catalog.\n\
         For an all-digit model name, include the manufacturer, for example \"Barrett 950\".\n\
         The table below records selected rigs and their hardware test status. It is not the \
         full list of accepted models.\n\n",
    );
    match tested.as_slice() {
        [] => out.push_str("No rig has a recorded hardware test.\n"),
        [one] => {
            out.push_str(&format!(
                "The {one} is the only rig with a recorded hardware test.\n"
            ));
            out.push_str(
                "Every other rig is mapped to its Hamlib model, but nobody has recorded using it with Pancetta.\n",
            );
        }
        many => {
            out.push_str(&format!(
                "These rigs have a recorded hardware test: {}.\n",
                many.join(", ")
            ));
            out.push_str(
                "Every other rig is mapped to its Hamlib model, but nobody has recorded using it with Pancetta.\n",
            );
        }
    }

    out.push_str(
        "\n| Rig | `rig.model` | Hamlib model | CAT | Audio | TX |\n|---|---|---|---|---|---|\n",
    );
    for rig in &rigs {
        let names = rig
            .config_names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            rig.display_name(),
            names,
            rig.hamlib_id,
            rig.cat.label(),
            rig.audio.label(),
            rig.tx.label()
        ));
    }

    out.push_str(
        "\n## What the columns mean\n\n\
         - CAT: Pancetta reads and sets frequency and split, and keys PTT, through `rigctld`. Pancetta does not set the rig mode.\n\
         - Audio: Pancetta decodes from and transmits into the rig's USB sound device, which you pick as input and output in `pancetta setup`. Pancetta has no rig-specific audio code.\n\
         - TX: Pancetta keys the rig and sends FT8 on air.\n\
         - Tested: used on a real station with this rig, as recorded in the README [Capability status](../README.md#capability-status) table.\n\
         - Untested: this repository records no hardware test of this rig.\n\
         - Hamlib model: the number Pancetta passes to `rigctld -m`. `rigctl -l` lists the models your installed Hamlib knows; an older Hamlib may lack a newer rig.\n",
    );

    let records: Vec<String> = rigs
        .iter()
        .filter_map(|r| r.evidence.map(|e| format!("- {}: {e}\n", r.display_name())))
        .collect();
    if !records.is_empty() {
        out.push_str("\n## Test records\n\n");
        for record in records {
            out.push_str(&record);
        }
    }

    out.push_str(
        "\n## Remote operation\n\n\
         The station agent uses the same rig connection and the same audio devices as local operation, so a rig's row also applies to remote use.\n\
         No remote session on a real rig is recorded. Remote operation is integration-tested only.\n\
         \n## Rigs not in this list\n\n\
         If a `rigctld` is already listening on `RIGCTLD_HOST`:`RIGCTLD_PORT` (default `127.0.0.1:4532`), Pancetta connects to it and does not start its own.\n\
         A nonempty `rig.model` name outside the bundled catalog now fails validation, even for external-daemon users; use the model's numeric ID instead (`rigctl -l` lists it).\n\
         Empty and `Generic` keep their existing placeholder behavior.\n\
         \nSWR readings, attenuator steps and preamp steps differ between rigs. Pancetta reads them from `rigctld` when it connects, so this page does not list them.\n",
    );
    out
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
    fn test_rig_capabilities() {
        let ft991a = RigModelType::YaesuFT991A;
        let caps = ft991a.capabilities();
        assert!(caps.has_dual_vfo);
        assert!(caps.has_scanning);
        assert!(caps.modes.contains(&Mode::USB));
        // Remove this line since we simplified the mode list
    }

    /// Upstream Hamlib model numbers, hand-copied from
    /// https://github.com/Hamlib/Hamlib/blob/master/include/hamlib/riglist.h
    /// (master, checked 2026-11-08). The `RIG_MODEL_*` define is named per row.
    /// A new `SUPPORTED_RIGS` entry fails this test until its author checks
    /// the upstream number and pins it here.
    const UPSTREAM_RIGLIST: &[(&str, u32, &str)] = &[
        ("IC-7300", 3073, "RIG_MODEL_IC7300"),
        ("IC-705", 3085, "RIG_MODEL_IC705"),
        ("IC-7610", 3078, "RIG_MODEL_IC7610"),
        ("IC-7851", 3075, "RIG_MODEL_IC785x"),
        ("IC-9700", 3081, "RIG_MODEL_IC9700"),
        ("TS-590S", 2031, "RIG_MODEL_TS590S"),
        ("TS-590SG", 2037, "RIG_MODEL_TS590SG"),
        ("TS-890S", 2041, "RIG_MODEL_TS890S"),
        ("FT-710", 1049, "RIG_MODEL_FT710"),
        ("FT-817", 1020, "RIG_MODEL_FT817"),
        ("FT-857", 1022, "RIG_MODEL_FT857"),
        ("FT-891", 1036, "RIG_MODEL_FT891"),
        ("FT-991", 1035, "RIG_MODEL_FT991"),
        ("FTdx10", 1042, "RIG_MODEL_FTDX10"),
        ("FTdx101D", 1040, "RIG_MODEL_FTDX101D"),
        ("FTdx101MP", 1044, "RIG_MODEL_FTDX101MP"),
    ];

    #[test]
    fn hamlib_ids_match_upstream_riglist() {
        for rig in SUPPORTED_RIGS {
            let pinned = UPSTREAM_RIGLIST
                .iter()
                .find(|(model, _, _)| *model == rig.model)
                .unwrap_or_else(|| {
                    panic!(
                        "{} has no pinned upstream number: check Hamlib riglist.h and add it \
                         to UPSTREAM_RIGLIST",
                        rig.model
                    )
                });
            assert_eq!(
                rig.hamlib_id, pinned.1,
                "{} must use {} = {}",
                rig.model, pinned.2, pinned.1
            );
        }
        assert_eq!(
            SUPPORTED_RIGS.len(),
            UPSTREAM_RIGLIST.len(),
            "UPSTREAM_RIGLIST pins a rig that SUPPORTED_RIGS no longer lists"
        );
    }

    /// PAN-340: every curated ID comes from the vendored Hamlib catalog via
    /// its upstream `RIG_MODEL_*` macro, so the curated table cannot drift
    /// from the authoritative ID source. `hamlib_ids_match_upstream_riglist`
    /// stays as independent hand-copied regression evidence.
    #[test]
    fn curated_ids_come_from_catalog() {
        for (model, _, macro_name) in UPSTREAM_RIGLIST {
            let rig = SUPPORTED_RIGS
                .iter()
                .find(|r| r.model == *model)
                .unwrap_or_else(|| panic!("{model} is no longer curated"));
            assert_eq!(
                crate::model_catalog::catalog_id_by_macro(macro_name),
                Some(rig.hamlib_id),
                "{model}: SUPPORTED_RIGS id {} disagrees with the catalog macro {macro_name}",
                rig.hamlib_id
            );
        }
        // The compile-time lookup also refuses unknown macros outright.
        assert_eq!(
            crate::model_catalog::catalog_id_by_macro("RIG_MODEL_NOPE"),
            None
        );
    }

    #[test]
    fn todays_model_names_still_resolve() {
        // Every normalized name `hamlib_model_id` accepted before PAN-271.
        for name in [
            "ftdx10",
            "ftdx101d",
            "ftdx101mp",
            "ft991",
            "ft991a",
            "ft710",
            "ft891",
            "ft857",
            "ft857d",
            "ft817",
            "ft817nd",
            "ic7300",
            "ic7610",
            "ic7851",
            "ic705",
            "ic9700",
            "ts890",
            "ts890s",
            "ts590",
            "ts590s",
            "ts590sg",
        ] {
            assert!(find_supported_rig(name).is_some(), "{name} must resolve");
        }
        assert_eq!(
            find_supported_rig("IC-7300").map(|r| r.hamlib_id),
            Some(3073)
        );
        assert_eq!(
            find_supported_rig("FT-DX10").map(|r| r.hamlib_id),
            Some(1042)
        );
        assert_eq!(
            find_supported_rig("ts 590 sg").map(|r| r.hamlib_id),
            Some(2037)
        );
    }

    #[test]
    fn config_names_are_unique_after_normalization() {
        let mut seen = std::collections::HashSet::new();
        for rig in SUPPORTED_RIGS {
            assert!(
                !rig.config_names.is_empty(),
                "{} has no config name",
                rig.model
            );
            for name in rig.config_names {
                let normalized = normalize_rig_model(name);
                assert!(
                    !normalized.is_empty(),
                    "{} has an empty config name",
                    rig.model
                );
                assert!(
                    seen.insert(normalized.clone()),
                    "config name {name:?} ({normalized}) is listed twice"
                );
            }
        }
        for name in ["Generic", "", "K3S", " - "] {
            assert!(
                find_supported_rig(name).is_none(),
                "{name:?} must not resolve"
            );
        }
    }

    #[test]
    fn tested_requires_evidence() {
        for rig in SUPPORTED_RIGS {
            assert_eq!(
                rig.has_any_test(),
                rig.evidence.is_some(),
                "{}: a Tested capability needs evidence, and evidence needs a Tested capability",
                rig.model
            );
        }
    }

    /// Drift guard, same pattern as `keybindings_doc_is_current` in
    /// pancetta-tui/src/keymap.rs: a rig table change without a regenerated
    /// page fails a test, not a human review.
    #[test]
    fn supported_rigs_doc_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/SUPPORTED_RIGS.md");
        let expected = render_supported_rigs_markdown();
        if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
            std::fs::write(path, &expected).expect("write docs/SUPPORTED_RIGS.md");
            return;
        }
        // A Windows checkout with core.autocrlf=true has CRLF line endings.
        let actual = std::fs::read_to_string(path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            actual, expected,
            "docs/SUPPORTED_RIGS.md is stale. Regenerate with:\n  \
             PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-hamlib supported_rigs_doc_is_current"
        );
    }

    #[test]
    fn lead_rigs_render_first() {
        let page = render_supported_rigs_markdown();
        let rows: Vec<&str> = page
            .lines()
            .skip_while(|l| !l.starts_with("|---"))
            .skip(1)
            .take_while(|l| l.starts_with('|'))
            .collect();
        assert_eq!(rows.len(), SUPPORTED_RIGS.len());
        assert!(rows[0].starts_with("| Icom IC-7300 |"), "{}", rows[0]);
        assert!(rows[1].starts_with("| Icom IC-705 |"), "{}", rows[1]);
    }

    /// PAN-340: the page explains the expanded acceptance (bundled catalog
    /// names and numeric IDs) while never claiming extra hardware
    /// qualification — the hardware rows stay evidence-bound.
    #[test]
    fn supported_rigs_page_distinguishes_catalog_from_test_evidence() {
        let page = render_supported_rigs_markdown();
        assert!(
            page.contains(
                "model name from the bundled Hamlib 4.5.5 catalog or a numeric Hamlib \
                 model ID, for example \"IC-7300\" or \"1045\""
            ),
            "the intro must explain catalog names and numeric IDs"
        );
        assert!(
            page.contains("Ambiguous names require a numeric ID"),
            "the intro must explain ambiguity"
        );
        assert!(
            page.contains("For an all-digit model name, include the manufacturer"),
            "the intro must explain all-digit model names"
        );
        assert!(
            page.contains("It is not the full list of accepted models."),
            "the intro must not claim the table is the acceptance list"
        );
        // The catalog-vs-evidence distinction keeps the qualification row:
        // only FTdx10 carries a Test record today.
        assert!(page.contains("| Yaesu FTdx10 | `FTdx10` | 1042 | Tested | Tested | Tested |"));
        assert!(page.contains("No remote session on a real rig is recorded"));
        // Numeric IDs work for rigs beyond the bundled catalog.
        assert!(page.contains("permits models newer than the bundled catalog"));
        // External-daemon users keep the already-listening escape hatch.
        assert!(page.contains("Pancetta connects to it and does not start its own"));
        assert!(
            page.contains("fails validation, even for external-daemon users"),
            "unknown names failing validation must be documented for external-daemon users"
        );
    }
}
