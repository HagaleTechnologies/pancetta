//! hb-229 — QSO partner band-collapse.
//!
//! Inspired by `spec-jtdx-qso-partner-filter.md`. When a QSO is in flight
//! the partner's audio frequency is already known to within a few hertz
//! and the only message that matters in this slot is the partner's reply
//! at that frequency. Restricting the native decoder's Costas search band
//! to ±60 Hz around the partner frees CPU for downstream work
//! (multi-stream-per-QSO, three-method sweep, etc.) at essentially zero
//! recall cost in the target band.
//!
//! This module is the **pure** half of the mechanism: it owns the
//! freq → bin-range conversion and a tiny QSO-aware observer that maps
//! `Option<partner_freq_hz>` to `Option<RangeInclusive<usize>>`. The
//! FT8 decoder thread (`coordinator/ft8.rs`) consumes the bin-range and
//! plumbs it into `Ft8Decoder::decode_window_with_ap_scoped` (the hook
//! hb-091 already added).
//!
//! Operator override: `PANCETTA_QSO_FILTER_OFF=1` disables the band
//! collapse and the main decode runs at full bandwidth. Any other value
//! (or the env var unset) leaves the filter on.
//!
//! **Hound-mode band-collapse wiring** (Task 3 status): The *relevance gate*
//! in `pancetta_qso::QsoManager::is_message_relevant` now keys on
//! `QsoMetadata::partner_freq` when set — Fox frames are matched against
//! the Fox's RX offset, not our TX offset. The band-collapse *here*, however,
//! is a pure observer that receives `Option<f64>` from the coordinator's
//! `active_qso_freq_hz` shared state; that shared state is currently
//! populated with `new_state.frequency()` (our TX offset) in `coordinator/qso.rs`.
//! For Hound, it should instead use `metadata.partner_freq` (the Fox's RX
//! offset) so the ±60 Hz collapse window centres on the Fox's reply —
//! the ±290 Hz Hound-mode window described in the spec is a future
//! refinement of that wiring, not a change to this module itself.

use std::ops::RangeInclusive;

use pancetta_ft8::TONE_SPACING;

/// Default half-window applied around the partner audio frequency. Per
/// `spec-jtdx-qso-partner-filter.md` the JTDX HF default is 60 Hz; this
/// matches the JTDX "Filter" toggle in non-Hound mode.
pub const DEFAULT_HALF_WINDOW_HZ: f64 = 60.0;

/// Environment variable name for the operator override. When set to `"1"`
/// the band-collapse is disabled and the main decode runs at full
/// bandwidth.
pub const QSO_FILTER_OFF_ENV: &str = "PANCETTA_QSO_FILTER_OFF";

/// Pure helper: translate a partner audio frequency in Hz into an
/// inclusive `freq_bin` range that brackets the partner with a
/// `±half_window_hz` margin.
///
/// `bin_spacing_hz` is the spectrogram bin spacing — for pancetta's FT8
/// decoder this is `TONE_SPACING = 6.25 Hz` (the `costas_sync_search`
/// loop iterates `freq_bin` at that granularity; the FREQ_OSR=2
/// sub-bins are searched separately via `freq_sub`).
///
/// Returns `None` when the partner frequency is non-positive or the bin
/// spacing is non-positive — neither input has a sensible interpretation
/// and the caller should fall back to a full-band search.
pub fn partner_freq_to_bin_range(
    partner_freq_hz: f64,
    bin_spacing_hz: f64,
    half_window_hz: f64,
) -> Option<RangeInclusive<usize>> {
    if !partner_freq_hz.is_finite()
        || !bin_spacing_hz.is_finite()
        || !half_window_hz.is_finite()
        || partner_freq_hz <= 0.0
        || bin_spacing_hz <= 0.0
        || half_window_hz < 0.0
    {
        return None;
    }
    let center_bin = (partner_freq_hz / bin_spacing_hz).round() as i64;
    let half_bins = (half_window_hz / bin_spacing_hz).ceil() as i64;
    let lo = center_bin.saturating_sub(half_bins).max(0) as usize;
    let hi = center_bin.saturating_add(half_bins).max(0) as usize;
    Some(lo..=hi)
}

/// Read the override env var. Returns `true` when the operator has
/// disabled the filter (i.e. `PANCETTA_QSO_FILTER_OFF=1`).
///
/// Any other value — unset, empty, `"0"`, garbage — leaves the filter
/// **enabled**. The default is "filter on" because the win is large
/// and the recall cost in the target band is zero by construction
/// (the partner's freq is known to the QSO state machine).
pub fn filter_disabled_by_env() -> bool {
    matches!(std::env::var(QSO_FILTER_OFF_ENV).as_deref(), Ok("1"))
}

/// Observer: given the current QSO-partner frequency state and the
/// operator override, decide whether to narrow the next decode call.
///
/// Returns `Some(range)` to narrow, `None` to leave the main decode at
/// full bandwidth. Pure function — no I/O, no locks — so it composes
/// cleanly with the FT8 hot loop that already holds the RwLock guard
/// for `active_qso_freq_hz`.
pub fn compute_narrow_filter_bins(
    partner_freq_hz: Option<f64>,
    half_window_hz: f64,
    bin_spacing_hz: f64,
    override_off: bool,
) -> Option<RangeInclusive<usize>> {
    if override_off {
        return None;
    }
    let freq = partner_freq_hz?;
    partner_freq_to_bin_range(freq, bin_spacing_hz, half_window_hz)
}

/// Convenience wrapper that uses pancetta-ft8's `TONE_SPACING` as the
/// bin spacing and the default 60 Hz half-window. The FT8 hot loop
/// calls this for the common case.
pub fn compute_narrow_filter_bins_default(
    partner_freq_hz: Option<f64>,
    override_off: bool,
) -> Option<RangeInclusive<usize>> {
    compute_narrow_filter_bins(
        partner_freq_hz,
        DEFAULT_HALF_WINDOW_HZ,
        TONE_SPACING,
        override_off,
    )
}

/// PAN-72 round-8 redesign (fix 2): dual-center variant of
/// [`compute_narrow_filter_bins`]. Takes a PRIMARY center frequency (today's
/// single hint) plus an optional SECONDARY center frequency — the offset a
/// `CallingCq` QSO recently vacated, while a reply to the last frame
/// transmitted there is still credible (see
/// `pancetta_qso::qso_manager::pre_switch_offset_grace_from_slot_ns`).
///
/// Computes each present frequency's own `±half_window_hz` range via
/// [`partner_freq_to_bin_range`] and returns their UNION (the smallest
/// inclusive range covering both), not their intersection — a caller
/// answering EITHER offset must decode. With only one frequency present
/// (the other `None`), this is byte-identical to
/// [`compute_narrow_filter_bins`] called with that single frequency — the
/// existing single-frequency callers and tests are unaffected.
pub fn compute_narrow_filter_bins_dual(
    primary_freq_hz: Option<f64>,
    secondary_freq_hz: Option<f64>,
    half_window_hz: f64,
    bin_spacing_hz: f64,
    override_off: bool,
) -> Option<RangeInclusive<usize>> {
    if override_off {
        return None;
    }
    let primary =
        primary_freq_hz.and_then(|f| partner_freq_to_bin_range(f, bin_spacing_hz, half_window_hz));
    let secondary = secondary_freq_hz
        .and_then(|f| partner_freq_to_bin_range(f, bin_spacing_hz, half_window_hz));
    match (primary, secondary) {
        (Some(a), Some(b)) => {
            let lo = *a.start().min(b.start());
            let hi = *a.end().max(b.end());
            Some(lo..=hi)
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Convenience wrapper for [`compute_narrow_filter_bins_dual`] using
/// pancetta-ft8's `TONE_SPACING` and the default 60 Hz half-window — the
/// dual-center sibling of [`compute_narrow_filter_bins_default`].
pub fn compute_narrow_filter_bins_default_dual(
    primary_freq_hz: Option<f64>,
    secondary_freq_hz: Option<f64>,
    override_off: bool,
) -> Option<RangeInclusive<usize>> {
    compute_narrow_filter_bins_dual(
        primary_freq_hz,
        secondary_freq_hz,
        DEFAULT_HALF_WINDOW_HZ,
        TONE_SPACING,
        override_off,
    )
}

/// hb-230 — partner-aware observer for the decoder's relaxed-sync window.
///
/// Returns `Some(partner_freq_hz)` when a QSO is active AND the operator
/// hasn't overridden the QSO filter; `None` otherwise. Symmetric in shape
/// to `compute_narrow_filter_bins` so the FT8 hot loop can consume both
/// outputs from the same `Option<partner_freq_hz>` read.
///
/// The decoder uses the returned value to apply a relaxed Costas-sync
/// threshold within `±relaxed_sync_near_partner_hz_radius` of the
/// partner — see `Ft8Config::relaxed_sync_near_partner_hz_radius`. When
/// the override is set or no QSO is active, returns `None`, which makes
/// the relaxed-threshold branch a no-op (byte-identical to historical
/// behaviour).
///
/// Note the override semantics intentionally piggyback on the hb-229
/// `PANCETTA_QSO_FILTER_OFF` env var: an operator who wants the wide
/// decode back also wants the global-threshold sync back. They are the
/// same conceptual switch ("treat the QSO partner as just another
/// signal in the band").
pub fn partner_freq_for_relaxed_sync(
    partner_freq_hz: Option<f64>,
    override_off: bool,
) -> Option<f64> {
    if override_off {
        return None;
    }
    partner_freq_hz.filter(|p| p.is_finite() && *p > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- partner_freq_to_bin_range -----------------------------

    #[test]
    fn partner_freq_to_bin_range_centers_on_partner() {
        // 1500 Hz partner, 60 Hz window, 6.25 Hz/bin →
        //   center = round(1500/6.25) = 240
        //   half_bins = ceil(60/6.25) = 10
        //   range = 230..=250
        let range = partner_freq_to_bin_range(1500.0, 6.25, 60.0).unwrap();
        assert_eq!(*range.start(), 230);
        assert_eq!(*range.end(), 250);
    }

    #[test]
    fn partner_freq_to_bin_range_clamps_low_end_to_zero() {
        // Partner at 12.5 Hz with 60 Hz window — center bin would be 2,
        // half-bins 10 → lo would be -8 → clamps to 0.
        let range = partner_freq_to_bin_range(12.5, 6.25, 60.0).unwrap();
        assert_eq!(*range.start(), 0);
        assert!(*range.end() >= 10);
    }

    #[test]
    fn partner_freq_to_bin_range_default_window_is_about_twenty_bins_wide() {
        // The spec's implementation note says "±60 Hz maps to roughly
        // 20 frequency bins at pancetta's coarse-search bin spacing".
        // 2*ceil(60/6.25) = 2*10 = 20 → width = 20 bins on either side
        // → total 21 bins inclusive (2*10 + 1 for the center).
        let range = partner_freq_to_bin_range(2000.0, 6.25, 60.0).unwrap();
        let width = range.end() - range.start() + 1;
        assert_eq!(width, 21, "expected ±10 bins around center, got {width}");
    }

    #[test]
    fn partner_freq_to_bin_range_rejects_non_positive_freq() {
        assert!(partner_freq_to_bin_range(0.0, 6.25, 60.0).is_none());
        assert!(partner_freq_to_bin_range(-100.0, 6.25, 60.0).is_none());
    }

    #[test]
    fn partner_freq_to_bin_range_rejects_non_positive_spacing() {
        assert!(partner_freq_to_bin_range(1500.0, 0.0, 60.0).is_none());
        assert!(partner_freq_to_bin_range(1500.0, -1.0, 60.0).is_none());
    }

    #[test]
    fn partner_freq_to_bin_range_rejects_nan_and_inf() {
        assert!(partner_freq_to_bin_range(f64::NAN, 6.25, 60.0).is_none());
        assert!(partner_freq_to_bin_range(f64::INFINITY, 6.25, 60.0).is_none());
        assert!(partner_freq_to_bin_range(1500.0, f64::NAN, 60.0).is_none());
        assert!(partner_freq_to_bin_range(1500.0, 6.25, f64::INFINITY).is_none());
    }

    #[test]
    fn partner_freq_to_bin_range_zero_window_is_one_bin_wide() {
        // half_bins = ceil(0/6.25) = 0 → range = [center, center]
        let range = partner_freq_to_bin_range(1500.0, 6.25, 0.0).unwrap();
        assert_eq!(*range.start(), 240);
        assert_eq!(*range.end(), 240);
    }

    // ---------- compute_narrow_filter_bins (observer) -----------------

    #[test]
    fn observer_returns_none_when_no_qso_active() {
        // No partner freq → nothing to narrow → main decode at full
        // bandwidth.
        let result = compute_narrow_filter_bins(None, 60.0, 6.25, false);
        assert!(result.is_none());
    }

    #[test]
    fn observer_returns_band_when_qso_active() {
        // Partner at 1500 Hz with QSO active and override off → narrow.
        let result = compute_narrow_filter_bins(Some(1500.0), 60.0, 6.25, false);
        assert!(result.is_some());
        let range = result.unwrap();
        assert_eq!(*range.start(), 230);
        assert_eq!(*range.end(), 250);
    }

    #[test]
    fn observer_clears_band_when_operator_override_set() {
        // Even with an active QSO, the override forces None — full
        // bandwidth decode — for parity with JTDX's "Filter off".
        let result = compute_narrow_filter_bins(Some(1500.0), 60.0, 6.25, true);
        assert!(result.is_none());
    }

    #[test]
    fn observer_qso_ends_clears_band() {
        // Simulate a QSO ending: the QSO component sets the shared
        // `active_qso_freq_hz` to None. The observer then returns None
        // and the main decode reverts to full bandwidth on the next
        // window.
        let active = compute_narrow_filter_bins(Some(2000.0), 60.0, 6.25, false);
        assert!(active.is_some(), "narrow during active QSO");
        let after = compute_narrow_filter_bins(None, 60.0, 6.25, false);
        assert!(after.is_none(), "wide after QSO end");
    }

    #[test]
    fn observer_default_helper_uses_60hz_window_and_tone_spacing() {
        // The default helper must match the explicit-args form with
        // (60 Hz, TONE_SPACING).
        let explicit = compute_narrow_filter_bins(Some(1500.0), 60.0, TONE_SPACING, false);
        let default = compute_narrow_filter_bins_default(Some(1500.0), false);
        assert_eq!(explicit, default);
    }

    #[test]
    fn observer_default_helper_respects_override() {
        let default = compute_narrow_filter_bins_default(Some(1500.0), true);
        assert!(default.is_none());
    }

    // ---------- compute_narrow_filter_bins_dual (PAN-72 fix 2) --------

    #[test]
    fn dual_with_only_primary_matches_single_frequency_behavior() {
        let single = compute_narrow_filter_bins(Some(1500.0), 60.0, 6.25, false);
        let dual = compute_narrow_filter_bins_dual(Some(1500.0), None, 60.0, 6.25, false);
        assert_eq!(single, dual);
    }

    #[test]
    fn dual_with_only_secondary_matches_single_frequency_behavior() {
        let single = compute_narrow_filter_bins(Some(1500.0), 60.0, 6.25, false);
        let dual = compute_narrow_filter_bins_dual(None, Some(1500.0), 60.0, 6.25, false);
        assert_eq!(single, dual);
    }

    #[test]
    fn dual_with_neither_frequency_is_none() {
        let dual = compute_narrow_filter_bins_dual(None, None, 60.0, 6.25, false);
        assert!(dual.is_none());
    }

    #[test]
    fn dual_unions_two_adjacent_frequencies() {
        // Primary 1500 Hz -> 230..=250; secondary 1500+400=1900 Hz ->
        // center = round(1900/6.25) = 304, half_bins = 10 -> 294..=314.
        // Union must span the low end of the primary through the high end
        // of the secondary.
        let dual = compute_narrow_filter_bins_dual(Some(1500.0), Some(1900.0), 60.0, 6.25, false)
            .unwrap();
        assert_eq!(*dual.start(), 230);
        assert_eq!(*dual.end(), 314);
    }

    #[test]
    fn dual_unions_two_far_apart_frequencies_without_bridging_the_gap() {
        // Two disjoint windows far apart: the union is still a single
        // inclusive range spanning both (the caller narrows the decoder to
        // one contiguous band, not two disjoint ones) but must not include
        // anything closer than each side's own window.
        let dual = compute_narrow_filter_bins_dual(Some(500.0), Some(3000.0), 60.0, 6.25, false)
            .unwrap();
        let expected_low = partner_freq_to_bin_range(500.0, 6.25, 60.0).unwrap();
        let expected_high = partner_freq_to_bin_range(3000.0, 6.25, 60.0).unwrap();
        assert_eq!(*dual.start(), *expected_low.start());
        assert_eq!(*dual.end(), *expected_high.end());
    }

    #[test]
    fn dual_respects_override() {
        let dual = compute_narrow_filter_bins_dual(Some(1500.0), Some(1900.0), 60.0, 6.25, true);
        assert!(dual.is_none());
    }

    #[test]
    fn default_dual_matches_explicit_dual_with_tone_spacing_and_default_window() {
        let explicit =
            compute_narrow_filter_bins_dual(Some(1500.0), Some(1900.0), 60.0, TONE_SPACING, false);
        let default = compute_narrow_filter_bins_default_dual(Some(1500.0), Some(1900.0), false);
        assert_eq!(explicit, default);
    }

    #[test]
    fn default_dual_with_no_secondary_matches_default_single() {
        let single = compute_narrow_filter_bins_default(Some(1500.0), false);
        let dual = compute_narrow_filter_bins_default_dual(Some(1500.0), None, false);
        assert_eq!(single, dual);
    }

    // ---------- partner_freq_for_relaxed_sync (hb-230) ----------------

    #[test]
    fn relaxed_sync_observer_returns_partner_when_active() {
        let result = partner_freq_for_relaxed_sync(Some(1500.0), false);
        assert_eq!(result, Some(1500.0));
    }

    #[test]
    fn relaxed_sync_observer_returns_none_when_no_qso() {
        let result = partner_freq_for_relaxed_sync(None, false);
        assert!(result.is_none());
    }

    #[test]
    fn relaxed_sync_observer_returns_none_under_override() {
        let result = partner_freq_for_relaxed_sync(Some(1500.0), true);
        assert!(result.is_none());
    }

    #[test]
    fn relaxed_sync_observer_rejects_non_finite_or_negative() {
        assert!(partner_freq_for_relaxed_sync(Some(f64::NAN), false).is_none());
        assert!(partner_freq_for_relaxed_sync(Some(f64::INFINITY), false).is_none());
        assert!(partner_freq_for_relaxed_sync(Some(0.0), false).is_none());
        assert!(partner_freq_for_relaxed_sync(Some(-100.0), false).is_none());
    }
}
