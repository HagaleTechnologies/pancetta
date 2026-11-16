use super::*;

#[test]
fn in_band_frequency_is_permitted() {
    assert_eq!(
        tx_band_plan_mute_reason(
            (14_074_000 + 1_500) as f64,
            0.0,
            &pancetta_config::BandPlanConfig::default()
        ),
        None
    );
}

#[test]
fn between_bands_frequency_is_refused_with_the_rf_in_the_reason() {
    let reason = tx_band_plan_mute_reason(
        15_001_500.0,
        0.0,
        &pancetta_config::BandPlanConfig::default(),
    )
    .expect("15.0015 MHz sits between 20m and 17m — must refuse");
    assert!(
        reason.contains("TX frequency 15001500 Hz is outside the configured amateur band plan"),
        "reason names the absolute RF: {reason}"
    );
}

#[test]
fn edge_warnings_false_permits_everything() {
    let mut band_plan = pancetta_config::BandPlanConfig::default();
    band_plan.edge_warnings = false;
    assert_eq!(
        tx_band_plan_mute_reason(15_001_500.0, 0.0, &band_plan),
        None
    );
}

#[test]
fn custom_bands_override_is_respected() {
    use pancetta_config::{BandDefinition, BandType, FrequencyRange};
    let default_plan = pancetta_config::BandPlanConfig::default();
    // 14.340 MHz is inside 20m's region-2 range (14.000–14.350 MHz).
    assert_eq!(
        tx_band_plan_mute_reason(14_340_000.0, 0.0, &default_plan),
        None
    );
    // A custom "20m" entry narrows the allocation to 14.000–14.100 MHz —
    // the same override semantics as the TUI's own override test
    // (`tx_rf_out_of_band_plan_custom_bands_override_wins`): the entry
    // replaces the region table for frequencies the global band table
    // still resolves to that band. 14.340 MHz now falls outside it.
    let mut custom = default_plan.clone();
    custom.custom_bands.insert(
        "20m".to_string(),
        BandDefinition {
            name: "20m".to_string(),
            ranges: vec![FrequencyRange {
                start: 14_000_000,
                end: 14_100_000,
                modes: Vec::new(),
            }],
            default_mode: "FT8".to_string(),
            band_type: BandType::Hf,
        },
    );
    let reason = tx_band_plan_mute_reason(14_340_000.0, 0.0, &custom)
        .expect("custom 20m narrowed to 14.000-14.100 MHz must refuse 14.340 MHz");
    assert!(reason.contains("TX frequency 14340000 Hz"));
    assert_eq!(tx_band_plan_mute_reason(14_080_000.0, 0.0, &custom), None);
}

// PR #446 Codex review: the gate must check the OCCUPIED signal — the
// lowest tone (dial + audio offset) AND the highest tone (+ the
// protocol's (num_tones - 1) * tone_spacing) — not just the base tone.
// 20m's upper edge is 14.350 MHz in every IARU region.
const TWENTY_M_TOP: u64 = 14_350_000;

fn assert_upper_edge_gate(protocol: pancetta_ft8::Protocol, refused_low: u64, ok_low: u64) {
    let plan = pancetta_config::BandPlanConfig::default();
    let span = tx_signal_span_hz(protocol);
    // The base tone alone is in-band in both cases...
    assert_eq!(
        tx_band_plan_mute_reason(refused_low as f64, 0.0, &plan),
        None
    );
    // ...but here the top tone crosses the band edge: refuse.
    assert!(
        refused_low as f64 + span > TWENTY_M_TOP as f64,
        "fixture: {protocol} top tone must cross the edge"
    );
    let reason = tx_band_plan_mute_reason(refused_low as f64, span, &plan).unwrap_or_else(|| {
        panic!(
            "{protocol}: top tone at {} Hz is past 14.350 MHz — must refuse",
            refused_low as f64 + span
        )
    });
    assert!(
        reason.contains(&format!(
            "TX signal {refused_low}-{} Hz extends outside the configured amateur band plan",
            refused_low + span.ceil() as u64
        )),
        "{protocol}: reason names the occupied range: {reason}"
    );
    // Whole signal below the edge: permit.
    assert!(ok_low as f64 + span <= TWENTY_M_TOP as f64);
    assert_eq!(
        tx_band_plan_mute_reason(ok_low as f64, span, &plan),
        None,
        "{protocol}"
    );
}

#[test]
fn signal_span_matches_modulator_tone_geometry() {
    assert_eq!(tx_signal_span_hz(pancetta_ft8::Protocol::Ft8), 7.0 * 6.25);
    assert!((tx_signal_span_hz(pancetta_ft8::Protocol::Ft4) - 3.0 * 20.8333).abs() < 1e-9);
    #[cfg(feature = "ft2")]
    assert_eq!(tx_signal_span_hz(pancetta_ft8::Protocol::Ft2), 7.0 * 25.0);
}

#[test]
fn ft8_top_tone_past_upper_band_edge_is_refused() {
    // 14_349_970 + 43.75 = 14_350_013.75 > edge; 14_349_950 + 43.75 < edge.
    assert_upper_edge_gate(pancetta_ft8::Protocol::Ft8, 14_349_970, 14_349_950);
}

#[test]
fn ft4_top_tone_past_upper_band_edge_is_refused() {
    // 14_349_950 + 62.5 = 14_350_012.5 > edge; 14_349_930 + 62.5 < edge.
    assert_upper_edge_gate(pancetta_ft8::Protocol::Ft4, 14_349_950, 14_349_930);
}

#[cfg(feature = "ft2")]
#[test]
fn ft2_top_tone_past_upper_band_edge_is_refused() {
    // 14_349_850 + 175 = 14_350_025 > edge; 14_349_800 + 175 < edge.
    assert_upper_edge_gate(pancetta_ft8::Protocol::Ft2, 14_349_850, 14_349_800);
}

#[test]
fn low_edge_still_refused_with_a_signal_span() {
    // Base tone below 20m's lower edge (14.000 MHz), top tone inside it.
    let plan = pancetta_config::BandPlanConfig::default();
    let span = tx_signal_span_hz(pancetta_ft8::Protocol::Ft8);
    assert!(tx_band_plan_mute_reason(13_999_980.0, span, &plan).is_some());
    assert_eq!(tx_band_plan_mute_reason(14_000_000.0, span, &plan), None);
}

#[test]
fn edge_warnings_false_permits_a_spanning_signal() {
    let plan = pancetta_config::BandPlanConfig {
        edge_warnings: false,
        ..Default::default()
    };
    let span = tx_signal_span_hz(pancetta_ft8::Protocol::Ft8);
    assert_eq!(tx_band_plan_mute_reason(14_349_990.0, span, &plan), None);
}

// PR #446 Codex review: the gate must not round a fractional audio
// offset before comparing — a sub-Hz overshoot past a band edge is
// still out of band. Composite (resolver + gate), as every caller uses it.
fn gate(dial: u64, audio_offset_hz: f64, span_hz: f64) -> Option<String> {
    tx_rf_for_gate(
        &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(dial)),
        &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        audio_offset_hz,
    )
    .and_then(|rf| {
        tx_band_plan_mute_reason(rf, span_hz, &pancetta_config::BandPlanConfig::default())
    })
}

#[test]
fn fractional_top_tone_past_upper_edge_is_refused() {
    // 14_348_000 + 1_956.4 + 43.75 = 14_350_000.15 Hz > 14.350 MHz.
    let ft8 = tx_signal_span_hz(pancetta_ft8::Protocol::Ft8);
    assert!(
        gate(14_348_000, 1_956.4, ft8).is_some(),
        "top tone 0.15 Hz past the 20m upper edge must refuse"
    );
    // 14_348_000 + 1_956.25 + 43.75 = 14_350_000.0 exactly: in band.
    assert_eq!(gate(14_348_000, 1_956.25, ft8), None);
}

#[test]
fn fractional_base_tone_below_lower_edge_is_refused() {
    // 13_998_000 + 1_999.6 = 13_999_999.6 Hz < 14.000 MHz.
    let ft8 = tx_signal_span_hz(pancetta_ft8::Protocol::Ft8);
    assert!(
        gate(13_998_000, 1_999.6, ft8).is_some(),
        "base tone 0.4 Hz below the 20m lower edge must refuse"
    );
    // Single tone (tune / carrier) too.
    assert!(gate(13_998_000, 1_999.6, 0.0).is_some());
    assert_eq!(gate(13_998_000, 2_000.0, ft8), None);
}

#[test]
fn rf_gate_skips_when_dial_unknown() {
    assert_eq!(
        tx_rf_for_gate(
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            1_500.0,
        ),
        None
    );
}

#[test]
fn rf_gate_resolves_split_when_rx_dial_unknown() {
    // PR #446 Codex review: RX dial still 0 (initial CAT read failed)
    // but a split TX dial is known — the effective TX dial is the
    // split, so the gate must check it, not skip.
    assert_eq!(
        tx_rf_for_gate(
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(15_000_000)),
            1_500.0,
        ),
        Some(15_001_500.0)
    );
}

#[test]
fn rf_gate_uses_rx_dial_plus_offset_on_simplex() {
    assert_eq!(
        tx_rf_for_gate(
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            1_500.0,
        ),
        Some(14_075_500.0)
    );
    assert_eq!(
        tx_rf_for_gate(
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            1_499.6,
        ),
        // Exact, unrounded (PR #446 Codex review) — the gate floors/ceils.
        Some(14_075_499.6)
    );
}

#[test]
fn rf_gate_prefers_split_dial_when_active() {
    assert_eq!(
        tx_rf_for_gate(
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
            &std::sync::Arc::new(std::sync::atomic::AtomicU64::new(15_000_000)),
            1_500.0,
        ),
        Some(15_001_500.0)
    );
}
