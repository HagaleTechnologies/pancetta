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
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};

/// Output of a bootstrap CI on a per-tier delta.
///
/// Units match the input: if the inputs are `(recovered_count, truth_count)`
/// pairs and the statistic is `Σ recovered`, then `mean`, `ci_low`, and
/// `ci_high` are in decode units (e.g. +5.2 means "B recovered 5.2 more
/// decodes than A on average across bootstrap resamples of the WAV set").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
            let i = rng.random_range(0..n);
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

/// SNR at which a decode-rate curve first crosses 50%, scanning from the
/// lowest SNR upward and linearly interpolating between the bracketing
/// points (ported from batch101's `threshold_50`). `rates[0] >= 0.5`
/// returns `snrs[0]`; a curve that never reaches 50% (or is empty)
/// returns `None`. `snrs` must be ascending and the same length as `rates`.
pub fn threshold_50(snrs: &[f64], rates: &[f64]) -> Option<f64> {
    assert_eq!(
        snrs.len(),
        rates.len(),
        "threshold_50: snrs/rates length mismatch"
    );
    if rates.is_empty() {
        return None;
    }
    if rates[0] >= 0.5 {
        return Some(snrs[0]);
    }
    for k in 1..rates.len() {
        if rates[k] >= 0.5 && rates[k - 1] < 0.5 {
            let span = snrs[k] - snrs[k - 1];
            let frac = (0.5 - rates[k - 1]) / (rates[k] - rates[k - 1]);
            return Some(snrs[k - 1] + frac * span);
        }
    }
    None
}

/// Paired-bootstrap CI on a 50%-threshold shift
/// `threshold_50(off) − threshold_50(on)` (dB; positive = `on` improves).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThresholdShiftCi {
    /// Shift on the observed data; `None` when either threshold is undefined.
    pub point: Option<f64>,
    /// Type-7 2.5th percentile over resamples with a defined shift.
    pub ci_low: Option<f64>,
    /// Type-7 97.5th percentile over resamples with a defined shift.
    pub ci_high: Option<f64>,
    /// `ci_low > 0 || ci_high < 0` (false when the CI is undefined).
    pub significant: bool,
    pub n_bootstrap: usize,
    /// Resamples where either resampled threshold was undefined.
    pub undefined_resamples: usize,
}

fn rates_of(rows: &[Vec<bool>]) -> Vec<f64> {
    rows.iter()
        .map(|r| {
            if r.is_empty() {
                0.0
            } else {
                r.iter().filter(|&&b| b).count() as f64 / r.len() as f64
            }
        })
        .collect()
}

/// Paired bootstrap of the 50%-threshold shift between two per-trial
/// success matrices (`off[point][trial]`, `on[point][trial]`). At each SNR
/// point, trial indices are resampled independently with replacement and
/// the SAME indices are applied to `off` and `on`. The CI is the type-7
/// 2.5/97.5 percentile range over resamples whose shift is defined;
/// undefined resamples are counted. Deterministic for a given seed.
///
/// Panics when `snrs`, `off` and `on` differ in length or when `off[i]`
/// and `on[i]` hold different trial counts (callers must align them),
/// matching `bootstrap_recall_delta`'s contract.
pub fn bootstrap_threshold_shift(
    snrs: &[f64],
    off: &[Vec<bool>],
    on: &[Vec<bool>],
    n_bootstrap: usize,
    seed: u64,
) -> ThresholdShiftCi {
    assert_eq!(
        snrs.len(),
        off.len(),
        "bootstrap_threshold_shift requires aligned inputs (snrs vs off points)"
    );
    assert_eq!(
        off.len(),
        on.len(),
        "bootstrap_threshold_shift requires aligned inputs (off vs on points)"
    );
    for (i, (a, b)) in off.iter().zip(on).enumerate() {
        assert_eq!(
            a.len(),
            b.len(),
            "bootstrap_threshold_shift requires aligned inputs (trial count at point {i})"
        );
    }
    let shift = |a: &[f64], b: &[f64]| -> Option<f64> {
        Some(threshold_50(snrs, a)? - threshold_50(snrs, b)?)
    };
    let point = shift(&rates_of(off), &rates_of(on));

    let mut rng = StdRng::seed_from_u64(seed);
    let mut stats: Vec<f64> = Vec::with_capacity(n_bootstrap);
    let mut undefined = 0usize;
    let mut r_off = vec![0.0f64; snrs.len()];
    let mut r_on = vec![0.0f64; snrs.len()];
    for _ in 0..n_bootstrap {
        for (p, (a, b)) in off.iter().zip(on).enumerate() {
            let n = a.len();
            if n == 0 {
                r_off[p] = 0.0;
                r_on[p] = 0.0;
                continue;
            }
            let (mut sa, mut sb) = (0usize, 0usize);
            for _ in 0..n {
                let i = rng.random_range(0..n);
                sa += a[i] as usize;
                sb += b[i] as usize;
            }
            r_off[p] = sa as f64 / n as f64;
            r_on[p] = sb as f64 / n as f64;
        }
        match shift(&r_off, &r_on) {
            Some(s) => stats.push(s),
            None => undefined += 1,
        }
    }
    stats.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let (ci_low, ci_high) = if stats.is_empty() {
        (None, None)
    } else {
        (
            Some(percentile_sorted(&stats, 2.5)),
            Some(percentile_sorted(&stats, 97.5)),
        )
    };
    let significant = matches!(ci_low, Some(l) if l > 0.0) || matches!(ci_high, Some(h) if h < 0.0);
    ThresholdShiftCi {
        point,
        ci_low,
        ci_high,
        significant,
        n_bootstrap,
        undefined_resamples: undefined,
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

    /// PAN-1: the resample index draw (`bootstrap_delta_impl`) must cover the
    /// full half-open `[0, n)` range. This is the only assertion in the module
    /// that would catch a half-open→inclusive slip (which panics on index `n`)
    /// or a truncated range (which biases the CI) — every other test above
    /// feeds a uniform per-WAV shift, so its resample sum is constant no
    /// matter which indices get drawn.
    ///
    /// Deliberately not a stream assertion: it pins the *distribution* the
    /// draw induces, so it holds across a `rand` major bump.
    #[test]
    fn bootstrap_resampling_covers_the_full_index_range() {
        // Per-WAV deltas are 0..=7 (mean 3.5). The statistic is a SUM over n=8
        // resampled indices, so the bootstrap mean must approach 8 * 3.5 = 28.
        let a: Vec<(u32, u32)> = vec![(0, 8); 8];
        let b: Vec<(u32, u32)> = (0..8).map(|i| (i as u32, 8)).collect();
        let ci = bootstrap_recall_delta(&a, &b, 5_000, 20_260_729);

        // One draw has variance (8^2 - 1)/12 = 5.25, so a resample sum of 8
        // draws has sd sqrt(8 * 5.25) ≈ 6.48 and the mean over 5000 resamples
        // has std err ≈ 0.092. The 0.5 bound is ~5.4σ. Discriminating power:
        // dropping index 7 from the range would give 8 * 3.0 = 24.0, and an
        // inclusive `0..=n` would panic indexing an 8-element slice.
        assert!(
            (ci.mean - 28.0).abs() < 0.5,
            "bootstrap mean {} far from the expected resample-sum mean 28.0 \
             (a truncated index range biases this low)",
            ci.mean
        );
        assert!(
            ci.ci_low < ci.mean && ci.mean < ci.ci_high,
            "mean {} not bracketed by CI [{}, {}]",
            ci.mean,
            ci.ci_low,
            ci.ci_high
        );
    }

    fn grid(lo: f64, n: usize) -> Vec<f64> {
        (0..n).map(|k| lo + 0.5 * k as f64).collect()
    }

    fn over50(xs: &[u32]) -> Vec<f64> {
        xs.iter().map(|&x| x as f64 / 50.0).collect()
    }

    #[test]
    fn threshold_50_batch101_awgn_off() {
        let snrs = grid(-21.0, 11);
        let rates = over50(&[0, 0, 1, 5, 16, 33, 40, 49, 49, 50, 50]);
        let t = threshold_50(&snrs, &rates).unwrap();
        assert_eq!(format!("{t:.2}"), "-18.74", "{t}");
    }

    #[test]
    fn threshold_50_batch101_impulsive_k6_shift() {
        let snrs = grid(-21.0, 11);
        let off = over50(&[0, 0, 0, 3, 8, 19, 26, 35, 42, 49, 49]);
        let k6 = over50(&[0, 0, 0, 4, 8, 22, 28, 42, 48, 50, 50]);
        let shift = threshold_50(&snrs, &off).unwrap() - threshold_50(&snrs, &k6).unwrap();
        assert_eq!(format!("{shift:+.3}"), "+0.179", "{shift}");
    }

    #[test]
    fn threshold_50_edges() {
        let snrs = grid(-21.0, 4);
        assert_eq!(threshold_50(&snrs, &[0.0; 4]), None);
        assert_eq!(threshold_50(&snrs, &[0.5, 0.2, 0.9, 1.0]), Some(-21.0));
        assert_eq!(threshold_50(&snrs, &[0.6, 0.7, 0.9, 1.0]), Some(-21.0));
        assert_eq!(threshold_50(&[], &[]), None);
        // Exact interpolation: 0.25 -> 0.75 over 0.5 dB crosses at the midpoint.
        assert_eq!(threshold_50(&snrs, &[0.0, 0.25, 0.75, 1.0]), Some(-20.25));
    }

    fn rows(counts: &[usize], n: usize) -> Vec<Vec<bool>> {
        counts
            .iter()
            .map(|&c| (0..n).map(|i| i < c).collect())
            .collect()
    }

    #[test]
    fn threshold_shift_identical_inputs_zero() {
        let snrs = grid(-21.0, 6);
        let off = rows(&[0, 3, 10, 25, 38, 50], 50);
        let ci = bootstrap_threshold_shift(&snrs, &off, &off, 1000, 0xb007);
        assert_eq!(ci.point, Some(0.0));
        assert_eq!(ci.ci_low, Some(0.0));
        assert_eq!(ci.ci_high, Some(0.0));
        assert!(!ci.significant);
        assert_eq!(ci.n_bootstrap, 1000);
    }

    #[test]
    fn threshold_shift_strictly_better_on_is_significant() {
        let snrs = grid(-21.0, 8);
        let off = rows(&[0, 0, 2, 8, 20, 35, 48, 50], 50);
        // `on` is the off curve moved one point (0.5 dB) earlier; every
        // trial that succeeds in `off` also succeeds in `on`.
        let on = rows(&[0, 2, 8, 20, 35, 48, 50, 50], 50);
        let ci = bootstrap_threshold_shift(&snrs, &off, &on, 1000, 0xb007);
        let p = ci.point.unwrap();
        assert!((p - 0.5).abs() < 0.2, "{p}");
        assert!(ci.ci_low.unwrap() > 0.0, "{ci:?}");
        assert!(ci.significant);
    }

    #[test]
    fn threshold_shift_seed_determinism() {
        let snrs = grid(-21.0, 6);
        let off: Vec<Vec<bool>> = (0..6)
            .map(|p| (0..40).map(|t| (t * 7 + p * 3) % 10 < p * 2).collect())
            .collect();
        let on: Vec<Vec<bool>> = (0..6)
            .map(|p| (0..40).map(|t| (t * 3 + p) % 10 < p * 2 + 1).collect())
            .collect();
        let a = bootstrap_threshold_shift(&snrs, &off, &on, 500, 99);
        let b = bootstrap_threshold_shift(&snrs, &off, &on, 500, 99);
        assert_eq!(a, b);
    }

    #[test]
    fn threshold_shift_all_undefined() {
        let snrs = grid(-21.0, 3);
        let off = rows(&[0, 0, 0], 10);
        let ci = bootstrap_threshold_shift(&snrs, &off, &off, 100, 1);
        assert_eq!(ci.point, None);
        assert_eq!((ci.ci_low, ci.ci_high), (None, None));
        assert_eq!(ci.undefined_resamples, 100);
        assert!(!ci.significant);
    }

    #[test]
    #[should_panic(expected = "aligned inputs")]
    fn threshold_shift_mismatched_points_panic() {
        let snrs = grid(-21.0, 3);
        bootstrap_threshold_shift(&snrs, &rows(&[0, 1, 2], 4), &rows(&[0, 1], 4), 10, 0);
    }

    #[test]
    #[should_panic(expected = "aligned inputs")]
    fn threshold_shift_mismatched_trials_panic() {
        let snrs = grid(-21.0, 2);
        bootstrap_threshold_shift(&snrs, &rows(&[0, 1], 4), &rows(&[0, 1], 5), 10, 0);
    }

    #[test]
    #[should_panic(expected = "aligned inputs")]
    fn threshold_shift_mismatched_snrs_panic() {
        let snrs = grid(-21.0, 3);
        bootstrap_threshold_shift(&snrs, &rows(&[0, 1], 4), &rows(&[0, 1], 4), 10, 0);
    }
}
