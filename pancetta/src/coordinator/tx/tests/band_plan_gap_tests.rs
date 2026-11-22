// PAN-208. Formerly the inline module `pan208_band_plan_gap_tests`.
use super::*;

const FT8_SPAN_HZ: f64 = 43.75;

/// The ticket's plan: 20m carved into 14,070,000–14,071,000 Hz and
/// 14,071,020–14,072,000 Hz, with 14,071,000–14,071,020 excluded.
pub(crate) fn gapped_plan() -> pancetta_config::BandPlanConfig {
    use pancetta_config::{BandDefinition, BandType, FrequencyRange};
    let mut plan = pancetta_config::BandPlanConfig::default();
    plan.custom_bands.insert(
        "20m".to_string(),
        BandDefinition {
            name: "20m".to_string(),
            ranges: vec![
                FrequencyRange {
                    start: 14_070_000,
                    end: 14_071_000,
                    modes: Vec::new(),
                },
                FrequencyRange {
                    start: 14_071_020,
                    end: 14_072_000,
                    modes: Vec::new(),
                },
            ],
            default_mode: "FT8".to_string(),
            band_type: BandType::Hf,
        },
    );
    plan
}

#[test]
fn gap_straddling_signal_is_refused_and_names_the_occupied_range() {
    // Base tone 14,070,990 Hz is in the first range; the top tone
    // 14,071,033.75 Hz is in the second. Each edge alone is allowed.
    let reason = tx_band_plan_mute_reason(14_070_990.0, FT8_SPAN_HZ, &gapped_plan())
        .expect("a frame straddling the 14,071,000-14,071,020 Hz gap must refuse");
    assert!(
        reason.contains(
            "TX signal 14070990-14071034 Hz extends outside the configured amateur band plan"
        ),
        "reason names the occupied range: {reason}"
    );
}

#[test]
fn fully_inside_one_range_signal_still_keys() {
    // 14,071,100 + 43.75 = 14,071,143.75 Hz: wholly inside the second range.
    assert_eq!(
        tx_band_plan_mute_reason(14_071_100.0, FT8_SPAN_HZ, &gapped_plan()),
        None
    );
}

#[test]
fn zero_span_base_tone_in_first_range_still_keys() {
    // A tune tone or raw manual-PTT carrier occupies one frequency.
    assert_eq!(
        tx_band_plan_mute_reason(14_070_990.0, 0.0, &gapped_plan()),
        None
    );
}

#[test]
fn gap_plan_edge_warnings_false_permits_the_gap_straddling_signal() {
    let mut plan = gapped_plan();
    plan.edge_warnings = false;
    assert_eq!(
        tx_band_plan_mute_reason(14_070_990.0, FT8_SPAN_HZ, &plan),
        None
    );
}

#[test]
fn gap_plan_low_edge_in_no_range_keeps_low_frequency_message() {
    // 14,069,900 Hz is in global 20m but below the first custom range.
    let reason = tx_band_plan_mute_reason(14_069_900.0, FT8_SPAN_HZ, &gapped_plan())
        .expect("a base tone below every custom range must refuse");
    assert!(
        reason.contains("TX frequency 14069900 Hz is outside the configured amateur band plan"),
        "reason names the base tone: {reason}"
    );
}

#[test]
fn gap_plan_top_past_last_range_uses_extends_message() {
    // 14,071,990 + 43.75 = 14,072,033.75 Hz: past the second range's top.
    let reason = tx_band_plan_mute_reason(14_071_990.0, FT8_SPAN_HZ, &gapped_plan())
        .expect("a top tone past the last custom range must refuse");
    assert!(
        reason.contains("TX signal 14071990-14072034 Hz extends outside"),
        "reason names the occupied range: {reason}"
    );
}

#[test]
fn gap_plan_inclusive_edges_are_contained() {
    // Top tone exactly on the first range's upper edge.
    assert_eq!(
        tx_band_plan_mute_reason(14_070_990.0, 10.0, &gapped_plan()),
        None
    );
    // Base tone exactly on the second range's lower edge.
    assert_eq!(
        tx_band_plan_mute_reason(14_071_020.0, FT8_SPAN_HZ, &gapped_plan()),
        None
    );
}
