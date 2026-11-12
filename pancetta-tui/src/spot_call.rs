//! PAN-287: turn a DX Hunter network spot (DX cluster / cqdx row never heard
//! locally) into a call target: the standard dial for the active mode on the
//! band that contains the spot, plus the spot's audio offset on that dial.
//!
//! No spot source carries an explicit audio offset, so the offset is derived
//! from the spot's radio frequency: `offset = spot_rf − standard_dial`. It is
//! usable when it lies in the same 200-2900 Hz band the QSO engine accepts
//! for an active QSO (`ACTIVE_QSO_TX_OFFSET_{MIN,MAX}_HZ`); a spot on the dial
//! itself (or within the passband edges) has "no offset" and the coordinator
//! asks the frequency allocator instead.
//!
//! Frequency alone settles the mode question: the FT8 and FT4 windows
//! (`[dial, dial + 3000 Hz]`) are disjoint, so the cluster's unreliable mode
//! label is never trusted. A spot outside the active mode's window is refused
//! with a status line naming why.

use crate::app::AUDIO_PASSBAND_MAX_HZ;
use crate::config::Band;
use pancetta_qso::qso_manager::{ACTIVE_QSO_TX_OFFSET_MAX_HZ, ACTIVE_QSO_TX_OFFSET_MIN_HZ};

/// The standard dial for a TUI band in a given mode.
#[derive(Debug, Clone, PartialEq)]
pub struct StandardDial {
    /// Dial in Hz.
    pub dial_hz: u64,
    /// The value `App::apply_band_selection` assigns to
    /// `station_info.operating_frequency` (MHz).
    pub operating_mhz: f64,
    /// `Some` when the mode has no standard dial on this band and the FT8
    /// dial is used instead; the text is the band-key status note.
    pub fallback_note: Option<String>,
}

/// Mode-aware dial resolution, shared by the band keys
/// (`App::apply_band_selection`) and the Space spot-call path, so both land
/// on identical dials.
///
/// - FT8 (and any unknown mode string) → the TUI's own band table, assigning
///   `band.ft8_frequency` directly (no u64-Hz round-trip), byte-identical to
///   the pre-Task-11 band keys for any config, including a non-round-MHz
///   custom band value.
/// - FT4 → `pancetta_core::Band::dial_for(true)`, the same call the
///   autonomous ChangeBand handler uses; falls back to the FT8 frequency
///   (with a status note) on a band with no standard FT4 sub-band.
/// - FT2 → no standard dial-frequency table exists yet (FT2 remains blocked
///   on the operator resolving two incompatible spec candidates); falls back
///   to FT8 with a status note.
pub fn standard_dial(band: &Band, mode: &str) -> StandardDial {
    let ft8_dial = (band.ft8_frequency * 1_000_000.0) as u64;
    match mode {
        "FT4" => {
            let core_band = pancetta_core::Band::from_frequency(ft8_dial);
            match core_band.and_then(|b| b.dial_for(true)) {
                Some(hz) => StandardDial {
                    dial_hz: hz,
                    operating_mhz: hz as f64 / 1_000_000.0,
                    fallback_note: None,
                },
                None => StandardDial {
                    dial_hz: ft8_dial,
                    operating_mhz: ft8_dial as f64 / 1_000_000.0,
                    fallback_note: Some(format!(
                        "{} has no standard FT4 frequency — using FT8 dial",
                        band.name
                    )),
                },
            }
        }
        "FT2" => StandardDial {
            dial_hz: ft8_dial,
            operating_mhz: ft8_dial as f64 / 1_000_000.0,
            fallback_note: Some(
                "FT2 dial frequencies not yet defined — using FT8 dial".to_string(),
            ),
        },
        _ => StandardDial {
            dial_hz: ft8_dial,
            operating_mhz: band.ft8_frequency,
            fallback_note: None,
        },
    }
}

/// Where Space on a network spot should call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotCallTarget {
    /// Index into the operator's band list.
    pub band_index: usize,
    /// Band name from the operator's band list (e.g. `20M`).
    pub band_name: String,
    /// Standard dial for the active mode, in Hz.
    pub dial_hz: u64,
    /// `spot_rf − dial` when it lies in 200-2900 Hz; `None` = let the
    /// frequency allocator pick.
    pub audio_offset_hz: Option<u64>,
}

/// Why Space on a network spot does nothing. Nothing is sent to the rig.
#[derive(Debug, Clone, PartialEq)]
pub enum SpotCallRefusal {
    /// FT2 has no standard dial table.
    Ft2NoDial,
    /// The spot RF is not inside any band of the operator's band list.
    NotInBandList { rf_hz: u64 },
    /// FT4 is active and this band has no standard FT4 dial.
    NoFt4Dial { band: String },
    /// The spot lies in the other FT mode's window on this band.
    OtherModeWindow {
        rf_hz: u64,
        band: String,
        other_mode: &'static str,
    },
    /// The spot is on the band but outside the active mode's window
    /// (typically a CW/SSB cluster spot).
    OffSubBand {
        rf_hz: u64,
        band: String,
        mode: String,
    },
}

fn mhz4(rf_hz: u64) -> String {
    format!("{:.4}", rf_hz as f64 / 1_000_000.0)
}

impl SpotCallRefusal {
    /// The exact status-bar line for this refusal.
    pub fn status_line(&self, callsign: &str) -> String {
        match self {
            Self::Ft2NoDial => format!(
                "Can't call {callsign} — FT2 has no standard dial frequency (Shift+M to change mode)"
            ),
            Self::NotInBandList { rf_hz } => format!(
                "Can't call {callsign} — {} MHz is not on a band in your band list",
                mhz4(*rf_hz)
            ),
            Self::NoFt4Dial { band } => {
                format!("Can't call {callsign} — {band} has no standard FT4 frequency")
            }
            Self::OtherModeWindow {
                rf_hz,
                band,
                other_mode,
            } => format!(
                "Can't call {callsign} — spot {} MHz is in the {band} {other_mode} sub-band \
                 (Shift+M to change mode)",
                mhz4(*rf_hz)
            ),
            Self::OffSubBand { rf_hz, band, mode } => format!(
                "Can't call {callsign} — spot {} MHz is not on the {band} {mode} sub-band",
                mhz4(*rf_hz)
            ),
        }
    }
}

/// `rf` inside `[dial, dial + AUDIO_PASSBAND_MAX_HZ]`.
fn in_window(rf_hz: u64, dial_hz: u64) -> bool {
    rf_hz >= dial_hz && rf_hz <= dial_hz.saturating_add(AUDIO_PASSBAND_MAX_HZ)
}

/// Resolve a network spot's RF into a dial + offset for the active mode, or a
/// refusal. Pure: reads only its arguments.
pub fn resolve_spot_call(
    spot_rf_hz: u64,
    active_mode: &str,
    bands: &[Band],
) -> Result<SpotCallTarget, SpotCallRefusal> {
    if active_mode == "FT2" {
        return Err(SpotCallRefusal::Ft2NoDial);
    }
    let is_ft4 = active_mode == "FT4";
    let mode_label = if is_ft4 { "FT4" } else { "FT8" };
    let mhz = spot_rf_hz as f64 / 1_000_000.0;
    let band_index = bands
        .iter()
        .position(|b| b.frequency_range.0 <= mhz && mhz <= b.frequency_range.1)
        .ok_or(SpotCallRefusal::NotInBandList { rf_hz: spot_rf_hz })?;
    let band = &bands[band_index];
    let dial = standard_dial(band, mode_label);
    if is_ft4 && dial.fallback_note.is_some() {
        return Err(SpotCallRefusal::NoFt4Dial {
            band: band.name.clone(),
        });
    }
    if in_window(spot_rf_hz, dial.dial_hz) {
        let offset = spot_rf_hz - dial.dial_hz;
        let usable = (offset as f64) >= ACTIVE_QSO_TX_OFFSET_MIN_HZ
            && (offset as f64) <= ACTIVE_QSO_TX_OFFSET_MAX_HZ;
        return Ok(SpotCallTarget {
            band_index,
            band_name: band.name.clone(),
            dial_hz: dial.dial_hz,
            audio_offset_hz: usable.then_some(offset),
        });
    }
    let other_mode: &'static str = if is_ft4 { "FT8" } else { "FT4" };
    let other = standard_dial(band, other_mode);
    let other_has_dial = other_mode == "FT8" || other.fallback_note.is_none();
    if other_has_dial && in_window(spot_rf_hz, other.dial_hz) {
        return Err(SpotCallRefusal::OtherModeWindow {
            rf_hz: spot_rf_hz,
            band: band.name.clone(),
            other_mode,
        });
    }
    Err(SpotCallRefusal::OffSubBand {
        rf_hz: spot_rf_hz,
        band: band.name.clone(),
        mode: mode_label.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn bands() -> Vec<Band> {
        Config::default().bands.bands
    }

    fn ft4_20m_dial() -> u64 {
        pancetta_core::Band::Band20m.dial_for(true).unwrap()
    }

    #[test]
    fn ticket_example_rf_14_0746_derives_600_hz() {
        let t = resolve_spot_call(14_074_600, "FT8", &bands()).unwrap();
        assert_eq!(t.dial_hz, 14_074_000);
        assert_eq!(t.band_name, "20M");
        assert_eq!(t.audio_offset_hz, Some(600));
    }

    #[test]
    fn rf_14_075_210_derives_1210_hz() {
        let t = resolve_spot_call(14_075_210, "FT8", &bands()).unwrap();
        assert_eq!(t.dial_hz, 14_074_000);
        assert_eq!(t.audio_offset_hz, Some(1210));
        assert_eq!(bands()[t.band_index].name, "20M");
    }

    #[test]
    fn dial_only_spot_has_no_offset() {
        let t = resolve_spot_call(14_074_000, "FT8", &bands()).unwrap();
        assert_eq!(t.dial_hz, 14_074_000);
        assert_eq!(t.audio_offset_hz, None);
    }

    #[test]
    fn spot_below_200_hz_has_no_offset() {
        let t = resolve_spot_call(14_074_150, "FT8", &bands()).unwrap();
        assert_eq!(t.audio_offset_hz, None);
    }

    #[test]
    fn spot_above_2900_hz_inside_window_has_no_offset() {
        let t = resolve_spot_call(14_076_950, "FT8", &bands()).unwrap();
        assert_eq!(t.dial_hz, 14_074_000);
        assert_eq!(t.audio_offset_hz, None);
    }

    #[test]
    fn forty_metre_spot_resolves_forty_metre_dial() {
        let t = resolve_spot_call(7_075_210, "FT8", &bands()).unwrap();
        assert_eq!(t.dial_hz, 7_074_000);
        assert_eq!(t.band_name, "40M");
        assert_eq!(t.audio_offset_hz, Some(1210));
    }

    #[test]
    fn ft4_spot_uses_ft4_dial() {
        let dial = ft4_20m_dial();
        let t = resolve_spot_call(dial + 1000, "FT4", &bands()).unwrap();
        assert_eq!(t.dial_hz, dial);
        assert_eq!(t.band_name, "20M");
        assert_eq!(t.audio_offset_hz, Some(1000));
    }

    #[test]
    fn ft8_mode_refuses_rf_in_ft4_window() {
        let rf = ft4_20m_dial() + 1200;
        let r = resolve_spot_call(rf, "FT8", &bands()).unwrap_err();
        assert_eq!(
            r,
            SpotCallRefusal::OtherModeWindow {
                rf_hz: rf,
                band: "20M".to_string(),
                other_mode: "FT4",
            }
        );
        assert_eq!(
            r.status_line("ZL1ABC"),
            "Can't call ZL1ABC — spot 14.0812 MHz is in the 20M FT4 sub-band (Shift+M to change mode)"
        );
    }

    #[test]
    fn cw_spot_is_refused() {
        let r = resolve_spot_call(14_025_000, "FT8", &bands()).unwrap_err();
        assert!(matches!(r, SpotCallRefusal::OffSubBand { .. }));
        assert_eq!(
            r.status_line("ZL1ABC"),
            "Can't call ZL1ABC — spot 14.0250 MHz is not on the 20M FT8 sub-band"
        );
    }

    #[test]
    fn ft2_mode_is_refused() {
        let r = resolve_spot_call(14_075_210, "FT2", &bands()).unwrap_err();
        assert_eq!(r, SpotCallRefusal::Ft2NoDial);
        assert_eq!(
            r.status_line("ZL1ABC"),
            "Can't call ZL1ABC — FT2 has no standard dial frequency (Shift+M to change mode)"
        );
    }

    #[test]
    fn rf_off_band_list_is_refused() {
        let list: Vec<Band> = bands().into_iter().filter(|b| b.name != "20M").collect();
        let r = resolve_spot_call(14_075_200, "FT8", &list).unwrap_err();
        assert_eq!(r, SpotCallRefusal::NotInBandList { rf_hz: 14_075_200 });
        assert_eq!(
            r.status_line("ZL1ABC"),
            "Can't call ZL1ABC — 14.0752 MHz is not on a band in your band list"
        );
    }

    #[test]
    fn ft4_on_band_without_ft4_dial_is_refused() {
        let r = resolve_spot_call(5_358_000, "FT4", &bands()).unwrap_err();
        assert_eq!(
            r,
            SpotCallRefusal::NoFt4Dial {
                band: "60M".to_string()
            }
        );
        assert_eq!(
            r.status_line("ZL1ABC"),
            "Can't call ZL1ABC — 60M has no standard FT4 frequency"
        );
    }

    #[test]
    fn standard_dial_ft8_assigns_band_frequency_directly() {
        let b = &bands()[5];
        let d = standard_dial(b, "FT8");
        assert_eq!(d.dial_hz, 14_074_000);
        assert_eq!(d.operating_mhz, b.ft8_frequency);
        assert_eq!(d.fallback_note, None);
    }
}
