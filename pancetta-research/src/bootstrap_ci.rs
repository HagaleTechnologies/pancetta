//! Nonparametric bootstrap confidence intervals for per-tier scorecard deltas.
//!
//! Phase B of the 2026-06-01 engineering audit: per-batch "+5 hard-200
//! recall" wins were being celebrated without any noise-floor measurement.
//! Two eval runs at the same config can differ by tens of decodes purely
//! from Rayon thread-scheduling nondeterminism, OSD tie-breaking, or
//! incremental corpus drift (e.g. deleted WAVs between runs).
//!
//! This module computes a 95 % bootstrap CI over the per-WAV delta in
//! recovered (or novel) decodes. If 0 is inside the CI, the headline delta
//! is not distinguishable from same-config noise.
//!
//! ## Method
//!
//! For two scorecards A and B on the same tier, build a per-WAV table
//! `(rec_a[w], rec_b[w])` (and analogously for novel). Resample WAVs
//! with replacement N times (default N=1000). Each resample yields one
//! bootstrap statistic `Σ rec_b − Σ rec_a` over the resampled WAV set.
//! Report mean, 2.5th / 97.5th percentiles, and "significant" (= 0 not
//! in CI).
//!
//! The procedure is deterministic given the seed — same inputs and seed
//! always reproduce the same CI. We use `rand::rngs::StdRng::seed_from_u64`
//! (the convention used elsewhere in this crate).
//!
//! ## Limitations on truncated per-WAV data
//!
//! The current scorecard format only stores the top-20 worst per-WAV
//! failures (`per_wav_top_failures`). Bootstrapping over only those 20
//! WAVs is biased — it characterizes the worst tail, not the whole tier.
//! The companion `per_wav_records` field on `TierResult` (landed with
//! this module) emits ALL per-WAV (truth, recovered, novel) records,
//! which is the supported input for an unbiased bootstrap. The compare
//! binary falls back to top-20 with a warning when the full records are
//! missing (e.g. older archived scorecards).

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Output of a bootstrap CI on a per-tier delta.
///
/// Units match the input: if the inputs are `(recovered_count, truth_count)`
/// pairs and the statistic is `Σ recovered`, then `mean`, `ci_low`, and
/// `ci_high` are in decode units (e.g. +5.2 means "B recovered 5.2 more
/// decodes than A on average across bootstrap resamples of the WAV set").
#[derive(Clone, Debug, PartialEq)]
pub struct DeltaCi {
    /// Bootstrap mean of the per-resample delta statistic (= mean over
    /// N resamples of `Σ_w stat_b[w] − Σ_w stat_a[w]`). Approximately
    /// equal to the headline tier-level delta when N is large.
    pub mean: f64,
    /// 2.5th percentile of the bootstrap distribution (lower 95 % bound).
    pub ci_low: f64,
    /// 97.5th percentile of the bootstrap distribution (upper 95 % bound).
    pub ci_high: f64,
    /// `true` iff 0 falls outside `[ci_low, ci_high]` — i.e. the delta
    /// is distinguishable from zero at the 95 % level under the bootstrap.
    pub significant: bool,
    /// Number of bootstrap resamples taken. Recorded so report consumers
    /// can disambiguate quick-and-loose runs from canonical N=1000 runs.
    pub n_bootstrap: usize,
}

/// Bootstrap a 95 % CI on the per-tier delta in recovered truth decodes.
///
/// `a_per_wav` and `b_per_wav` must have the same length and represent
/// the SAME WAV ordering on both sides — element `i` is the same WAV in
/// A and B. Each element is `(recovered, truth)`; only `recovered` is
/// used here (the truth count is accepted for API symmetry and so the
/// caller can also feed the same table to `bootstrap_novel_delta`).
///
/// Returns a `DeltaCi` reporting mean, 95 % CI, and significance flag.
/// `n_bootstrap` should be 1000 for the canonical workflow; lower values
/// (e.g. 200) are fine for tests but produce noisier CI endpoints.
///
/// Determinism: identical `a_per_wav`, `b_per_wav`, `n_bootstrap`, and
/// `seed` always reproduce the same `DeltaCi`.
///
/// Edge case: empty inputs return a zero-CI (mean=0, low=0, high=0,
/// significant=false). Mismatched lengths panic — callers must align
/// the two scorecards by WAV before invoking this.
pub fn bootstrap_recall_delta(
    a_per_wav: &[(u32, u32)],
    b_per_wav: &[(u32, u32)],
    n_bootstrap: usize,
    seed: u64,
) -> DeltaCi {
    bootstrap_delta_impl(a_per_wav, b_per_wav, n_bootstrap, seed, |(rec, _truth)| {
        *rec as f64
    })
}

/// Bootstrap a 95 % CI on the per-tier delta in novel decodes.
///
/// Inputs are `(novel, _truth)` pairs per WAV; only `novel` is read. The
/// `_truth` slot is kept identical to `bootstrap_recall_delta` so callers
/// can reuse one table layout. See `bootstrap_recall_delta` for semantics.
pub fn bootstrap_novel_delta(
    a_per_wav: &[(u32, u32)],
    b_per_wav: &[(u32, u32)],
    n_bootstrap: usize,
    seed: u64,
) -> DeltaCi {
    bootstrap_delta_impl(a_per_wav, b_per_wav, n_bootstrap, seed, |(novel, _t)| {
        *novel as f64
    })
}

fn bootstrap_delta_impl<F>(
    a_per_wav: &[(u32, u32)],
    b_per_wav: &[(u32, u32)],
    n_bootstrap: usize,
    seed: u64,
    project: F,
) -> DeltaCi
where
    F: Fn(&(u32, u32)) -> f64,
{
    assert_eq!(
        a_per_wav.len(),
        b_per_wav.len(),
        "bootstrap_delta requires aligned per-WAV inputs (same length, same WAV order)",
    );
    let n = a_per_wav.len();
    if n == 0 || n_bootstrap == 0 {
        return DeltaCi {
            mean: 0.0,
            ci_low: 0.0,
            ci_high: 0.0,
            significant: false,
            n_bootstrap,
        };
    }

    let a_vals: Vec<f64> = a_per_wav.iter().map(&project).collect();
    let b_vals: Vec<f64> = b_per_wav.iter().map(&project).collect();

    let mut rng = StdRng::seed_from_u64(seed);
    let mut stats: Vec<f64> = Vec::with_capacity(n_bootstrap);
    for _ in 0..n_bootstrap {
        let mut sum_a = 0.0_f64;
        let mut sum_b = 0.0_f64;
        for _ in 0..n {
            let i = rng.gen_range(0..n);
            sum_a += a_vals[i];
            sum_b += b_vals[i];
        }
        stats.push(sum_b - sum_a);
    }

    stats.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let mean = stats.iter().sum::<f64>() / n_bootstrap as f64;
    let ci_low = percentile_sorted(&stats, 2.5);
    let ci_high = percentile_sorted(&stats, 97.5);
    let significant = ci_low > 0.0 || ci_high < 0.0;

    DeltaCi {
        mean,
        ci_low,
        ci_high,
        significant,
        n_bootstrap,
    }
}

/// Linear-interpolated percentile on a pre-sorted (ascending) vector.
///
/// Matches the common "type 7" definition (R default): for a sorted vector
/// of length N, the p-percentile lives at fractional index `(p/100)*(N-1)`.
/// We linearly interpolate between the two bracketing samples.
fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    debug_assert!((0.0..=100.0).contains(&p));
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted[0];
    }
    let pos = (p / 100.0) * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = pos - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_ci_contains_zero() {
        // 200 WAVs, identical recovered counts on both sides → all
        // resamples produce delta = 0 exactly. Mean, ci_low, ci_high all 0.
        let data: Vec<(u32, u32)> = (0..200).map(|i| (i as u32 % 50, 70)).collect();
        let ci = bootstrap_recall_delta(&data, &data, 1000, 42);
        assert_eq!(ci.mean, 0.0);
        assert_eq!(ci.ci_low, 0.0);
        assert_eq!(ci.ci_high, 0.0);
        assert!(!ci.significant, "identical inputs cannot be significant");
        assert!(0.0 >= ci.ci_low && 0.0 <= ci.ci_high, "0 must be in CI");
        assert_eq!(ci.n_bootstrap, 1000);
    }

    #[test]
    fn b_recovers_more_everywhere_ci_excludes_zero() {
        // B beats A by +1 decode on every one of 200 WAVs. Every bootstrap
        // resample sums to exactly +200 → the CI is a degenerate [+200, +200].
        let a: Vec<(u32, u32)> = (0..200).map(|_| (5, 70)).collect();
        let b: Vec<(u32, u32)> = (0..200).map(|_| (6, 70)).collect();
        let ci = bootstrap_recall_delta(&a, &b, 1000, 7);
        assert_eq!(ci.mean, 200.0);
        assert_eq!(ci.ci_low, 200.0);
        assert_eq!(ci.ci_high, 200.0);
        assert!(ci.significant, "uniform +1 on all WAVs must be significant");
    }

    #[test]
    fn b_recovers_more_in_aggregate_with_noise_ci_excludes_zero() {
        // B recovers +20 on every WAV → uniformly positive; CI low must
        // be > 0. Adds a heterogeneous truth column to confirm bootstrap
        // ignores the truth count.
        let a: Vec<(u32, u32)> = (0..200)
            .map(|i| (i as u32 % 50, 50 + (i as u32 % 30)))
            .collect();
        let b: Vec<(u32, u32)> = a.iter().map(|(r, t)| (r + 20, *t)).collect();
        let ci = bootstrap_recall_delta(&a, &b, 1000, 11);
        // Each bootstrap sums to +20 * 200 = +4000 exactly (uniform shift).
        assert_eq!(ci.mean, 4000.0);
        assert_eq!(ci.ci_low, 4000.0);
        assert_eq!(ci.ci_high, 4000.0);
        assert!(ci.significant);
    }

    #[test]
    fn small_delta_with_high_variance_ci_includes_zero() {
        // Realistic noise scenario: mean delta is small (+1 per WAV) but
        // per-WAV deltas swing widely (-10 .. +12). Bootstrap CI should
        // include 0 — i.e. the headline "+200 total" is NOT significant
        // given that scatter.
        let mut a: Vec<(u32, u32)> = Vec::new();
        let mut b: Vec<(u32, u32)> = Vec::new();
        // Pattern: 100 WAVs at b - a = -10, 100 WAVs at b - a = +12. Net = +200.
        for _ in 0..100 {
            a.push((30, 60));
            b.push((20, 60));
        }
        for _ in 0..100 {
            a.push((20, 60));
            b.push((32, 60));
        }
        let ci = bootstrap_recall_delta(&a, &b, 1000, 99);
        // Sanity: mean is approximately +200 (= net delta).
        assert!(
            (ci.mean - 200.0).abs() < 100.0,
            "bootstrap mean {} should be near +200",
            ci.mean,
        );
        // The CI should comfortably straddle 0 given the scatter (std ~
        // sqrt(200 * 11^2 / 4) ≈ 78). Concretely, ci_low must be < 0.
        assert!(
            ci.ci_low < 0.0,
            "expected ci_low < 0 (insignificant), got [{}, {}]",
            ci.ci_low,
            ci.ci_high,
        );
        assert!(!ci.significant);
    }

    #[test]
    fn seed_determinism() {
        // Heterogeneous per-WAV deltas (some +5, some -3, some 0) so that
        // different bootstrap draws actually produce different sums.
        let a: Vec<(u32, u32)> = (0..50).map(|i| (i as u32, 70)).collect();
        let b: Vec<(u32, u32)> = (0..50)
            .map(|i| {
                let rec = match i % 3 {
                    0 => i as u32 + 5,
                    1 => (i as u32).saturating_sub(3),
                    _ => i as u32,
                };
                (rec, 70)
            })
            .collect();
        let ci1 = bootstrap_recall_delta(&a, &b, 500, 12345);
        let ci2 = bootstrap_recall_delta(&a, &b, 500, 12345);
        assert_eq!(ci1, ci2, "same seed must reproduce CI exactly");
        let ci3 = bootstrap_recall_delta(&a, &b, 500, 12346);
        // Different seed → different bootstrap draws. Mean stays near the
        // true delta but the CI endpoints will (generally) differ.
        assert_ne!(ci1, ci3, "different seeds should produce different draws");
    }

    #[test]
    fn novel_delta_uses_first_field() {
        // bootstrap_novel_delta projects the first tuple slot, identical
        // to recall — but the field's semantic is "novel" in the caller's
        // table layout. Verify it agrees with recall when the same numbers
        // are passed.
        let a: Vec<(u32, u32)> = (0..100).map(|i| (i as u32, 0)).collect();
        let b: Vec<(u32, u32)> = (0..100).map(|i| (i as u32 + 3, 0)).collect();
        let ci_rec = bootstrap_recall_delta(&a, &b, 500, 1);
        let ci_novel = bootstrap_novel_delta(&a, &b, 500, 1);
        assert_eq!(ci_rec, ci_novel, "both projectors share the same field");
    }

    #[test]
    fn empty_inputs_return_zero_ci() {
        let a: Vec<(u32, u32)> = Vec::new();
        let b: Vec<(u32, u32)> = Vec::new();
        let ci = bootstrap_recall_delta(&a, &b, 1000, 0);
        assert_eq!(ci.mean, 0.0);
        assert_eq!(ci.ci_low, 0.0);
        assert_eq!(ci.ci_high, 0.0);
        assert!(!ci.significant);
        assert_eq!(ci.n_bootstrap, 1000);
    }

    #[test]
    #[should_panic(expected = "aligned per-WAV inputs")]
    fn mismatched_lengths_panic() {
        let a: Vec<(u32, u32)> = vec![(1, 10); 10];
        let b: Vec<(u32, u32)> = vec![(1, 10); 11];
        bootstrap_recall_delta(&a, &b, 100, 0);
    }

    #[test]
    fn percentile_sorted_linear_interp() {
        let xs: Vec<f64> = (1..=11).map(|i| i as f64).collect(); // 1..=11
                                                                 // n=11 → indices 0..10. 50th percentile → pos = 5 → xs[5] = 6.
        assert!((percentile_sorted(&xs, 50.0) - 6.0).abs() < 1e-12);
        // 2.5th → pos = 0.25 → 1 + 0.25*(2-1) = 1.25.
        assert!((percentile_sorted(&xs, 2.5) - 1.25).abs() < 1e-12);
        // 97.5th → pos = 9.75 → 10 + 0.75*(11-10) = 10.75.
        assert!((percentile_sorted(&xs, 97.5) - 10.75).abs() < 1e-12);
        // Edge: 0th and 100th
        assert!((percentile_sorted(&xs, 0.0) - 1.0).abs() < 1e-12);
        assert!((percentile_sorted(&xs, 100.0) - 11.0).abs() < 1e-12);
    }
}
