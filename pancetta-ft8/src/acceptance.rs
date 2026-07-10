//! Signal-domain acceptance metric for CRC-valid decodes (Workstream 2, task
//! W2.1 of `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`).
//!
//! Section 4 of that design doc roots the OSD-disable decision in a real gap:
//! OSD (`osd.rs`) accepts the **first** CRC-14-passing candidate out of up to
//! 121,485 flip patterns with no distance/acceptance metric at all. At
//! CRC-14's 2⁻¹⁴ collision rate, false passes are a statistical certainty at
//! that trial volume. This module builds the metric — computed and attached
//! to every reachable CRC-valid decode's metadata — that later tasks
//! (W2.2-W2.5) will use to gate OSD's candidate selection and re-enable it
//! safely. **This task is bit-exact**: the metric is informational only here;
//! nothing consults it to accept/reject a decode yet.
//!
//! Design decision D2 (spec §4): build one signal-domain acceptance metric
//! combining (a) an LLR-domain weighted soft distance to the received
//! (channel) hard decisions, (b) a hard-error count, and (c) an optional
//! coherent re-encode correlation. Clean-room references:
//! `research/specs/spec-wsjtx-mainline-osd174.md` (dmin/soft-distance
//! semantics) and `research/specs/spec-wsjtx-improved-fdr.md`
//! (threshold-by-FDR calibration approach) — prose specs only, no GPL source
//! consulted.

use bitvec::prelude::*;

/// Per-decode acceptance evidence, computed once a codeword has passed
/// CRC-14. None of these fields gate anything in this task — see the module
/// doc — they exist to be logged and calibrated against.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AcceptanceScore {
    /// Fraction of `|LLR|` mass on bits where the accepted codeword
    /// disagrees with the channel hard decisions:
    /// `sum(|llr_i| where sign mismatch) / sum(|llr_i|)`.
    ///
    /// This is the WSJT-X-mainline-OSD-style weighted soft distance (see
    /// `spec-wsjtx-mainline-osd174.md`): a single confidently-wrong bit
    /// (large `|llr_i|`, sign disagrees with the codeword) can dominate this
    /// value even when many low-confidence bits also disagree — see the
    /// `soft_distance_disagrees_with_naive_hard_error_ordering` test below
    /// for why that distinction matters (a naive hard-error count alone
    /// would rank the two cases oppositely).
    ///
    /// Range `[0.0, 1.0]`. `0.0` means the codeword matches every channel
    /// hard decision. `1.0` means the codeword disagrees with every channel
    /// hard decision (all `|LLR|` mass on mismatches).
    pub soft_distance: f32,
    /// Count of hard-decision disagreements between the accepted codeword
    /// and the channel LLRs' hard decisions, over all 174 bits.
    pub hard_errors: u16,
    /// Optional coherent re-encode correlation against the candidate's
    /// spectrogram region, normalized to `0.0..=1.0` (higher = more
    /// coherent / better phase alignment). Reuses
    /// [`known_coherence_score`](crate::decoder) where the caller has the
    /// spectrogram context conveniently in scope; `None` when unavailable
    /// (no complex spectrogram retention, or the call site doesn't have
    /// per-candidate spectrogram coordinates handy — see decoder.rs wiring
    /// comments for exactly which call sites populate this).
    pub coherence: Option<f32>,
}

/// The number of bits in one FT8/FT4/FT2 LDPC codeword (payload + CRC +
/// parity). Fixed across all three pancetta-ft8 protocols.
const CODEWORD_BITS: usize = 174;

/// Compute the [`AcceptanceScore`] for a CRC-valid `codeword` against the
/// **channel** LLRs that produced it.
///
/// `channel_llrs` MUST be the pre-BP channel LLRs (the demapper/whitening
/// output fed into belief propagation), not BP posteriors and not OSD's
/// internal (possibly `bp_offset_subtract`-adjusted) working array. Feeding
/// posteriors in here would make `soft_distance` measure "does BP/OSD agree
/// with itself" rather than "does the codeword agree with the actual
/// received signal" — the latter is the point of a signal-domain acceptance
/// check (design spec §4, D2).
///
/// `coherence` is always `None` in the returned score — this function has no
/// spectrogram access. Callers that have the spectrogram/candidate
/// coordinates in scope may set it afterward.
pub fn score(codeword: &BitSlice, channel_llrs: &[f32; CODEWORD_BITS]) -> AcceptanceScore {
    let mut mismatch_mag_sum = 0.0f32;
    let mut total_mag_sum = 0.0f32;
    let mut hard_errors: u16 = 0;

    for (i, &llr) in channel_llrs.iter().enumerate() {
        let mag = llr.abs();
        total_mag_sum += mag;

        // Convention shared with `LdpcDecoder::llrs_to_bits`: negative LLR
        // means hard-decision bit = 1.
        let hard_bit = llr < 0.0;
        let codeword_bit = codeword.get(i).map(|b| *b).unwrap_or(false);

        if codeword_bit != hard_bit {
            mismatch_mag_sum += mag;
            hard_errors += 1;
        }
    }

    let soft_distance = if total_mag_sum > f32::EPSILON {
        (mismatch_mag_sum / total_mag_sum).clamp(0.0, 1.0)
    } else {
        // Degenerate all-zero-magnitude input: no evidence either way.
        // Treat as "no disagreement" rather than dividing by ~zero.
        0.0
    };

    AcceptanceScore {
        soft_distance,
        hard_errors,
        coherence: None,
    }
}

/// Convenience wrapper around [`score`] for callers holding channel LLRs in
/// a dynamically-sized slice (`Vec<f32>` / `&[f32]`) rather than a fixed
/// `[f32; 174]` array — the common shape at pancetta-ft8's many decode call
/// sites (LLR vectors are built with `Vec::with_capacity(174)` and passed
/// around as slices). Returns `None` if `channel_llrs.len() != 174`
/// (defensive; should not happen for a valid FT8/FT4/FT2 codeword, whose
/// LDPC codeword length is fixed at 174 bits regardless of protocol)
/// instead of panicking.
pub fn score_from_slice(codeword: &BitSlice, channel_llrs: &[f32]) -> Option<AcceptanceScore> {
    let arr: &[f32; CODEWORD_BITS] = channel_llrs.try_into().ok()?;
    Some(score(codeword, arr))
}

/// Normalize a [`known_coherence_score`](crate::decoder) raw value
/// (`-sum_delta_mag`, range `(-2*num_deltas, 0]`, higher/less-negative =
/// tighter phase alignment) into `0.0..=1.0` for storage in
/// [`AcceptanceScore::coherence`].
///
/// `num_deltas` is the number of symbol-to-symbol phase deltas the raw score
/// summed over (at most `num_symbols - 1`, fewer if some symbols were
/// skipped as silent/out-of-range). Each delta's magnitude is bounded in
/// `[0.0, 2.0]` (unit-vector phase difference), so `2 * num_deltas` is the
/// worst-case (maximally jittery) raw magnitude.
pub fn normalize_coherence(raw: f64, num_deltas: usize) -> f32 {
    if num_deltas == 0 {
        return 0.0;
    }
    let max_jitter = 2.0 * num_deltas as f64;
    let normalized = 1.0 + (raw / max_jitter);
    normalized.clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 174-bit codeword and channel-LLR array from a list of
    /// `(bit, llr_magnitude)` pairs where `bit` is the TRUE transmitted bit
    /// and the LLR sign is derived to agree with `bit` (i.e. the channel
    /// "hears" this bit correctly) unless `flip` is set, in which case the
    /// LLR sign is inverted (channel hard-decision disagrees with the
    /// codeword bit we'll test against).
    fn build_case(specs: &[(bool, f32, bool)]) -> (BitVec, [f32; CODEWORD_BITS]) {
        assert_eq!(specs.len(), CODEWORD_BITS);
        let mut codeword = BitVec::with_capacity(CODEWORD_BITS);
        let mut llrs = [0.0f32; CODEWORD_BITS];
        for (i, &(bit, mag, flip)) in specs.iter().enumerate() {
            codeword.push(bit);
            // Convention: negative LLR => hard bit = 1. To make the hard
            // decision AGREE with `bit`, llr sign must be negative when
            // bit=true. `flip` inverts that sign so the hard decision
            // disagrees with the codeword bit instead.
            let agreeing_sign = if bit { -1.0 } else { 1.0 };
            let sign = if flip { -agreeing_sign } else { agreeing_sign };
            llrs[i] = sign * mag;
        }
        (codeword, llrs)
    }

    #[test]
    fn zero_errors_yields_zero_soft_distance() {
        // Every bit's hard decision agrees with the codeword (no flips).
        let specs: Vec<(bool, f32, bool)> = (0..CODEWORD_BITS)
            .map(|i| (i % 2 == 0, 1.0 + (i as f32 % 5.0), false))
            .collect();
        let (codeword, llrs) = build_case(&specs);

        let result = score(&codeword, &llrs);

        assert_eq!(result.hard_errors, 0);
        assert_eq!(result.soft_distance, 0.0);
    }

    #[test]
    fn all_flipped_yields_soft_distance_one() {
        // Every bit's hard decision disagrees with the codeword.
        let specs: Vec<(bool, f32, bool)> = (0..CODEWORD_BITS)
            .map(|i| (i % 2 == 0, 1.0 + (i as f32 % 5.0), true))
            .collect();
        let (codeword, llrs) = build_case(&specs);

        let result = score(&codeword, &llrs);

        assert_eq!(result.hard_errors, CODEWORD_BITS as u16);
        assert_eq!(result.soft_distance, 1.0);
    }

    #[test]
    fn soft_distance_disagrees_with_naive_hard_error_ordering() {
        // Case A: exactly ONE flipped bit, but it carries a huge |LLR|
        // (a confidently-wrong bit). All other 173 bits agree at modest
        // |LLR| = 1.0.
        let mut specs_a = vec![(true, 1.0f32, false); CODEWORD_BITS];
        specs_a[0] = (true, 100.0, true);
        let (codeword_a, llrs_a) = build_case(&specs_a);
        let a = score(&codeword_a, &llrs_a);

        // Case B: TWENTY flipped bits, each carrying a tiny |LLR| = 0.01
        // (weak, low-confidence disagreements). The remaining 154 bits
        // agree at modest |LLR| = 1.0.
        let mut specs_b = vec![(true, 1.0f32, false); CODEWORD_BITS];
        for slot in specs_b.iter_mut().take(20) {
            *slot = (true, 0.01, true);
        }
        let (codeword_b, llrs_b) = build_case(&specs_b);
        let b = score(&codeword_b, &llrs_b);

        // A naive hard-error count ranks A as "cleaner" than B (1 error
        // vs 20 errors) ...
        assert_eq!(a.hard_errors, 1);
        assert_eq!(b.hard_errors, 20);
        assert!(a.hard_errors < b.hard_errors);

        // ... but soft_distance inverts that ranking: A's single
        // confidently-wrong bit dominates the |LLR| mass (100 out of
        // 100 + 173*1.0 = 273), while B's twenty weak disagreements are
        // a tiny fraction of its mass (20*0.01 out of 20*0.01 + 154*1.0
        // = 154.2). This is exactly the distinction a pure hard-error
        // count cannot make.
        let expected_a = 100.0 / (100.0 + 173.0);
        let expected_b = 0.2 / 154.2;
        assert!((a.soft_distance - expected_a).abs() < 1e-5);
        assert!((b.soft_distance - expected_b).abs() < 1e-5);
        assert!(
            a.soft_distance > b.soft_distance,
            "expected A's soft_distance ({}) > B's ({}) despite A having \
             fewer hard errors (1 vs 20) — a single high-|LLR| disagreement \
             should dominate many low-|LLR| ones",
            a.soft_distance,
            b.soft_distance
        );
    }

    #[test]
    fn zero_magnitude_llrs_do_not_panic_or_divide_by_zero() {
        let codeword = BitVec::repeat(false, CODEWORD_BITS);
        let llrs = [0.0f32; CODEWORD_BITS];
        let result = score(&codeword, &llrs);
        assert_eq!(result.soft_distance, 0.0);
        // Every hard-decision here is `false` (llr < 0.0 is false when
        // llr == 0.0), agreeing with the all-false codeword.
        assert_eq!(result.hard_errors, 0);
    }

    #[test]
    fn score_from_slice_rejects_wrong_length() {
        let codeword = BitVec::repeat(false, CODEWORD_BITS);
        let too_short = vec![1.0f32; 173];
        assert_eq!(score_from_slice(&codeword, &too_short), None);
    }

    #[test]
    fn score_from_slice_matches_score_for_correct_length() {
        let specs: Vec<(bool, f32, bool)> = (0..CODEWORD_BITS)
            .map(|i| (i % 2 == 0, 1.0 + (i as f32 % 5.0), false))
            .collect();
        let (codeword, llrs) = build_case(&specs);
        let via_array = score(&codeword, &llrs);
        let via_slice = score_from_slice(&codeword, &llrs[..]).unwrap();
        assert_eq!(via_array, via_slice);
    }

    #[test]
    fn coherence_field_defaults_to_none() {
        let codeword = BitVec::repeat(false, CODEWORD_BITS);
        let llrs = [1.0f32; CODEWORD_BITS];
        let result = score(&codeword, &llrs);
        assert_eq!(result.coherence, None);
    }

    #[test]
    fn normalize_coherence_perfect_alignment_is_one() {
        // raw = 0.0 (no phase jitter at all) => normalized 1.0.
        assert_eq!(normalize_coherence(0.0, 78), 1.0);
    }

    #[test]
    fn normalize_coherence_worst_case_is_zero() {
        // raw = -(2 * num_deltas) is the worst possible jitter => 0.0.
        let num_deltas = 78;
        let raw = -2.0 * num_deltas as f64;
        assert_eq!(normalize_coherence(raw, num_deltas), 0.0);
    }

    #[test]
    fn normalize_coherence_clamps_beyond_worst_case() {
        // Defensive: a raw value worse than the theoretical bound (shouldn't
        // happen, but float accumulation could nudge past it) still clamps
        // to 0.0 rather than going negative.
        let num_deltas = 78;
        let raw = -3.0 * num_deltas as f64;
        assert_eq!(normalize_coherence(raw, num_deltas), 0.0);
    }

    #[test]
    fn normalize_coherence_zero_deltas_is_zero() {
        assert_eq!(normalize_coherence(0.0, 0), 0.0);
    }
}
