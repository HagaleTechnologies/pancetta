//! Decoder effort/budget configuration module.
//!
//! Defines the `[decoder]` TOML section that controls the decode-effort
//! preset and an optional explicit per-window time budget. This section is
//! currently parsed/validated/merged only — nothing reads it yet (that is a
//! follow-on task that seeds the `decode_effort_budget_ms` atomic from it).

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Decode-effort preset. `Auto` (the default) maps from the hardware tier
/// probe; the others are explicit operator overrides trading decode
/// thoroughness for latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DecodeEffort {
    /// Derive the effort from the probed hardware tier (Fast/Moderate/Slow).
    #[default]
    Auto,
    /// Minimal effort — fastest decode, lowest recall.
    Eco,
    /// Baseline effort.
    Standard,
    /// Higher effort — more decode passes/candidates for better recall.
    Deep,
    /// Maximum effort — slowest decode, highest recall.
    Max,
}

impl DecodeEffort {
    /// Stable `u8` encoding for atomic storage (decoder-speed-overhaul
    /// Task 15, TUI live effort cycling). The mapping is fixed and MUST NOT
    /// change (`0` = Eco, `1` = Standard, `2` = Deep, `3` = Max, `4` = Auto).
    pub fn as_u8(&self) -> u8 {
        match self {
            DecodeEffort::Eco => 0,
            DecodeEffort::Standard => 1,
            DecodeEffort::Deep => 2,
            DecodeEffort::Max => 3,
            DecodeEffort::Auto => 4,
        }
    }

    /// Decode a [`DecodeEffort`] from its stable `u8` encoding (see
    /// [`DecodeEffort::as_u8`]). Any unrecognized value decodes to the safe
    /// default [`DecodeEffort::Auto`] — callers writing the atomic only ever
    /// store values produced by `as_u8`, so this branch is defensive.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DecodeEffort::Eco,
            1 => DecodeEffort::Standard,
            2 => DecodeEffort::Deep,
            3 => DecodeEffort::Max,
            4 => DecodeEffort::Auto,
            _ => DecodeEffort::Auto,
        }
    }

    /// Cycle to the next preset in the Eco → Standard → Deep → Max → Auto →
    /// Eco order. Drives the operator's live decode-effort keybinding (`e`
    /// in the TUI).
    pub fn cycle(&self) -> Self {
        match self {
            DecodeEffort::Eco => DecodeEffort::Standard,
            DecodeEffort::Standard => DecodeEffort::Deep,
            DecodeEffort::Deep => DecodeEffort::Max,
            DecodeEffort::Max => DecodeEffort::Auto,
            DecodeEffort::Auto => DecodeEffort::Eco,
        }
    }

    /// Upper-case label for the TUI status chip (`DECODE: <PRESET> ...`).
    pub fn label(&self) -> &'static str {
        match self {
            DecodeEffort::Eco => "ECO",
            DecodeEffort::Standard => "STANDARD",
            DecodeEffort::Deep => "DEEP",
            DecodeEffort::Max => "MAX",
            DecodeEffort::Auto => "AUTO",
        }
    }
}

/// Decoder effort/budget configuration.
///
/// Corresponds to the `[decoder]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderConfig {
    /// Effort preset; `auto` maps from the hardware tier probe.
    pub effort: DecodeEffort,
    /// Explicit per-window budget in ms; overrides `effort` when Some.
    pub budget_ms: Option<u64>,
    /// Task W5.3 (decoder-tp-sensitivity plan) [A/B]: widen the DSP capture
    /// window's lead-in from the default `WINDOW_LEAD_SECS` (0.5 s) to
    /// `EXTENDED_WINDOW_LEAD_SECS` (1.0 s, `pancetta/src/coordinator/mod.rs`).
    /// Default OFF — byte-identical capture window when unset.
    ///
    /// Root cause this addresses: the FT8 Costas sync sweep's time-step
    /// search is structurally non-negative (`for t0 in 0..=max_time_step` in
    /// `pancetta_ft8::decoder`) — it can never find a candidate earlier than
    /// sample 0 of whatever buffer it is handed. A station transmitting
    /// before the nominal slot boundary (negative DT, propagation delay /
    /// clock drift) is only recoverable if the buffer's sample 0 genuinely
    /// contains real audio from before the boundary. The coordinator
    /// already prepends `WINDOW_LEAD_SECS` of real (not synthetic/zero-pad)
    /// audio for exactly this reason; this flag widens that real lead-in
    /// further, extending the reachable negative-DT floor from about
    /// -0.66 s to about -1.16 s (`-(lead + 0.16 s)` structural correction).
    /// Verified empirically (2026-07-09 investigation): a synthetic dt=-0.8s
    /// signal fails to decode with the default 0.5s lead and decodes once
    /// widened to 1.0s; a synthetic dt=+2.2s signal ALREADY decodes today
    /// with the default lead (real trailing audio + LDPC redundancy already
    /// cover it), so this flag intentionally does not touch the trailing
    /// edge / `decode_phase` (that is coupled to DX-slot-aware TX scheduling
    /// elsewhere in the coordinator and was judged out of proportion to a
    /// benefit this task could not demonstrate a need for).
    ///
    /// Cost: the emitted decode window grows by `EXTENDED_WINDOW_LEAD_SECS -
    /// WINDOW_LEAD_SECS` (0.5 s) of real retained audio (already present in
    /// the DSP ring buffer's overlap retention — no extra capture latency),
    /// at a modest extra per-window decode cost (more Costas sync-search
    /// steps over a longer spectrogram). See the Task W5.3 experiment log
    /// for the measured elapsed-time delta.
    #[serde(default)]
    pub extended_capture_window_enabled: bool,
    /// 2026-07-17 operator finding: AP (a priori) decoding biases the LDPC
    /// solver toward finding *our own callsign* in a signal — by design, so
    /// weak genuine calls to us decode — but the same bias can converge on
    /// pure noise and produce a phantom "someone is calling us" message
    /// (`pancetta-ft8/src/decoder.rs`'s own comment: "AP injection biases
    /// the LDPC solver toward our callsign, producing phantom messages...
    /// from noise"). Observed live: repeated decodes of other stations
    /// calling a `/P`-suffixed variant of the operator's callsign at very
    /// weak SNR (-15 to -17 dB), triggering the always-answer-callers path
    /// (#39) to reply mid-QSO to calls that were never actually made.
    ///
    /// When `true`: every decode (AP and non-AP) is still logged with its
    /// `ap_level`, but any decode with `ap_level > 0` is filtered out
    /// immediately after the FP filter — before it reaches the TUI, the
    /// QSO engine (so it can never trigger always-answer-callers or be
    /// pounced on by the autonomous operator), cross-slot state, or any
    /// other consumer. Non-AP decodes (`ap_level == 0`, i.e. plain ft8_lib
    /// / native LDPC decodes with no callsign-hypothesis injection) are
    /// completely unaffected. This is a data-collection mode: it lets an
    /// operator see what AP WOULD have decoded, without letting it drive
    /// real QSO behavior, while the false-positive rate is investigated.
    ///
    /// Default `false` (AP decodes participate normally, matching behavior
    /// before this flag existed).
    #[serde(default)]
    pub ap_eval_mode: bool,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            effort: DecodeEffort::Auto,
            budget_ms: None,
            extended_capture_window_enabled: false,
            ap_eval_mode: false,
        }
    }
}

impl ConfigSection for DecoderConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.budget_ms == Some(0) {
            return Err(ConfigError::InvalidValue {
                field: "decoder.budget_ms".to_string(),
                value: "0".to_string(),
            });
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.effort = other.effort;
        self.budget_ms = other.budget_ms;
        self.extended_capture_window_enabled = other.extended_capture_window_enabled;
        self.ap_eval_mode = other.ap_eval_mode;
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_are_auto_and_none() {
        let cfg = DecoderConfig::default();
        assert_eq!(cfg.effort, DecodeEffort::Auto);
        assert_eq!(cfg.budget_ms, None);
        assert!(!cfg.extended_capture_window_enabled);
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = DecoderConfig::default();
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn config_without_decoder_section_uses_defaults() {
        // A TOML with NO [decoder] section must deserialize to defaults.
        let toml = "[station]\ncallsign = \"K5ARH\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        assert_eq!(parsed.decoder.effort, DecodeEffort::Auto);
        assert_eq!(parsed.decoder.budget_ms, None);
        assert!(!parsed.decoder.extended_capture_window_enabled);
    }

    /// Task W5.3: `extended_capture_window_enabled` must round-trip through
    /// TOML and default to `false` when the key is absent.
    #[test]
    fn extended_capture_window_enabled_parses_and_defaults_false() {
        let toml_str = "[decoder]\nextended_capture_window_enabled = true\n";
        let parsed: Config = toml::from_str(toml_str).expect("must parse");
        assert!(parsed.decoder.extended_capture_window_enabled);

        let rendered = toml::to_string(&parsed).expect("must serialize");
        let reparsed: Config = toml::from_str(&rendered).expect("must reparse");
        assert!(reparsed.decoder.extended_capture_window_enabled);

        let toml_no_key = "[decoder]\neffort = \"deep\"\n";
        let parsed_no_key: Config = toml::from_str(toml_no_key).expect("must parse");
        assert!(!parsed_no_key.decoder.extended_capture_window_enabled);
    }

    #[test]
    fn parse_round_trip_including_deep_string_form() {
        let toml_str = "[decoder]\neffort = \"deep\"\nbudget_ms = 5000\n";
        let parsed: Config = toml::from_str(toml_str).expect("must parse");
        assert_eq!(parsed.decoder.effort, DecodeEffort::Deep);
        assert_eq!(parsed.decoder.budget_ms, Some(5000));

        // Round-trip: serialize back out and re-parse, values must be stable.
        let rendered = toml::to_string(&parsed).expect("must serialize");
        let reparsed: Config = toml::from_str(&rendered).expect("must reparse");
        assert_eq!(reparsed.decoder.effort, DecodeEffort::Deep);
        assert_eq!(reparsed.decoder.budget_ms, Some(5000));
    }

    #[test]
    fn parse_all_effort_string_forms() {
        for (s, expected) in [
            ("auto", DecodeEffort::Auto),
            ("eco", DecodeEffort::Eco),
            ("standard", DecodeEffort::Standard),
            ("deep", DecodeEffort::Deep),
            ("max", DecodeEffort::Max),
        ] {
            let toml_str = format!("[decoder]\neffort = \"{s}\"\n");
            let parsed: Config = toml::from_str(&toml_str).unwrap_or_else(|e| {
                panic!("effort = \"{s}\" must parse, got error: {e}");
            });
            assert_eq!(parsed.decoder.effort, expected, "effort string {s}");
        }
    }

    #[test]
    fn validate_rejects_budget_ms_zero() {
        let mut cfg = DecoderConfig::default();
        cfg.budget_ms = Some(0);
        assert!(
            cfg.validate_section().is_err(),
            "budget_ms == Some(0) must be invalid"
        );
    }

    #[test]
    fn validate_accepts_budget_ms_none_and_nonzero() {
        let mut cfg = DecoderConfig::default();
        cfg.budget_ms = None;
        assert!(cfg.validate_section().is_ok(), "None must be valid");

        cfg.budget_ms = Some(1);
        assert!(cfg.validate_section().is_ok(), "Some(1) must be valid");

        cfg.budget_ms = Some(12_640);
        assert!(cfg.validate_section().is_ok(), "Some(12640) must be valid");
    }

    /// 2026-07-05 bug-class regression test: a hand-written `merge_with` is a
    /// manually-maintained list of `self.field = other.field` lines, so a
    /// field added after the impl was written is trivially forgotten — and
    /// the bug is invisible because both parsing AND validation still pass;
    /// only the merge silently drops the field back to its compiled-in
    /// default. This test constructs a base config (defaults) and an
    /// override config with NON-DEFAULT values for BOTH `effort` and
    /// `budget_ms`, merges, and asserts BOTH survive — a test that would
    /// fail if either field were accidentally omitted from `merge_with`.
    #[test]
    fn merge_with_carries_over_decoder_section() {
        let mut base = DecoderConfig::default();
        assert_eq!(base.effort, DecodeEffort::Auto);
        assert_eq!(base.budget_ms, None);

        let mut override_cfg = DecoderConfig::default();
        override_cfg.effort = DecodeEffort::Max;
        override_cfg.budget_ms = Some(9_999);
        override_cfg.extended_capture_window_enabled = true;

        base.merge_with(override_cfg);

        assert_eq!(
            base.effort,
            DecodeEffort::Max,
            "effort must survive merge_with"
        );
        assert!(
            base.extended_capture_window_enabled,
            "extended_capture_window_enabled must survive merge_with (Task W5.3)"
        );
        assert_eq!(
            base.budget_ms,
            Some(9_999),
            "budget_ms must survive merge_with"
        );
    }

    // ------------------------------------------------------------------
    // decoder-speed-overhaul Task 15: TUI live effort cycling
    // ------------------------------------------------------------------

    #[test]
    fn cycle_follows_eco_standard_deep_max_auto_eco_order() {
        assert_eq!(DecodeEffort::Eco.cycle(), DecodeEffort::Standard);
        assert_eq!(DecodeEffort::Standard.cycle(), DecodeEffort::Deep);
        assert_eq!(DecodeEffort::Deep.cycle(), DecodeEffort::Max);
        assert_eq!(DecodeEffort::Max.cycle(), DecodeEffort::Auto);
        assert_eq!(DecodeEffort::Auto.cycle(), DecodeEffort::Eco);
    }

    #[test]
    fn as_u8_from_u8_round_trip_for_every_variant() {
        for effort in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
            DecodeEffort::Auto,
        ] {
            assert_eq!(DecodeEffort::from_u8(effort.as_u8()), effort);
        }
    }

    #[test]
    fn from_u8_unrecognized_value_defaults_to_auto() {
        assert_eq!(DecodeEffort::from_u8(255), DecodeEffort::Auto);
    }

    #[test]
    fn label_is_upper_case_preset_name() {
        assert_eq!(DecodeEffort::Eco.label(), "ECO");
        assert_eq!(DecodeEffort::Standard.label(), "STANDARD");
        assert_eq!(DecodeEffort::Deep.label(), "DEEP");
        assert_eq!(DecodeEffort::Max.label(), "MAX");
        assert_eq!(DecodeEffort::Auto.label(), "AUTO");
    }
}
