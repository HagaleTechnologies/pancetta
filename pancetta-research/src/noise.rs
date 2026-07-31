//! Cheap noise-floor estimator used by the curate binary.
//!
//! For an FT8 WAV at 12 kHz mono, "noise floor" is approximated as the
//! median absolute amplitude of the lower 25th percentile of samples.
//! This catches busy bands (high noise floor from many overlapping signals)
//! without needing a full FFT-based spectral estimate.

use rand::rngs::StdRng;
use rand::RngExt;

/// Generates `n` samples of zero-mean Gaussian white noise via Box-Muller,
/// scaled by `sigma`. Deterministic for a given `rng` state — same seed,
/// same output. This is the manual Box-Muller variant several `examples/`
/// batches were built around; `gen_noise::generate_noise_corpus` uses
/// `rand_distr::Normal` instead for its production WAV corpus and is not a
/// drop-in replacement (different call sequence against the RNG, so it
/// would change every downstream example's byte-identical output).
pub fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.random_range(f32::EPSILON..1.0);
        let u2: f32 = rng.random_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

/// Returns an estimated noise floor in dB (relative to full-scale ±1.0).
/// Higher = noisier. Typical clean-band: -30 dB; busy-band: -20 to -15 dB.
pub fn estimate_noise_floor_db(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -100.0;
    }
    let mut abs: Vec<f32> = samples.iter().map(|s| s.abs()).collect();
    // Median of the lower 25% of |samples|.
    let q1_count = (abs.len() / 4).max(1);
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lower_quartile = &abs[..q1_count];
    let median = lower_quartile[lower_quartile.len() / 2] as f64;
    if median <= 0.0 {
        return -100.0;
    }
    20.0 * median.log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn gaussian_noise_is_deterministic_for_a_given_seed() {
        let mut a = StdRng::seed_from_u64(42);
        let mut b = StdRng::seed_from_u64(42);
        assert_eq!(
            gaussian_noise(&mut a, 100, 0.03),
            gaussian_noise(&mut b, 100, 0.03)
        );
    }

    #[test]
    fn gaussian_noise_scales_with_sigma() {
        let mut rng = StdRng::seed_from_u64(7);
        let samples = gaussian_noise(&mut rng, 10_000, 1.0);
        let rms = (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64)
            .sqrt();
        assert!((rms - 1.0).abs() < 0.1, "expected RMS near 1.0, got {rms}");
    }

    #[test]
    fn silence_has_low_noise_floor() {
        let samples = vec![0.0_f32; 1000];
        assert!(estimate_noise_floor_db(&samples) <= -50.0);
    }

    #[test]
    fn full_scale_signal_has_high_noise_floor() {
        let samples: Vec<f32> = (0..1000).map(|_| 0.5).collect();
        let floor = estimate_noise_floor_db(&samples);
        assert!(floor > -10.0, "got {floor}");
    }

    #[test]
    fn empty_samples_returns_sentinel() {
        assert_eq!(estimate_noise_floor_db(&[]), -100.0);
    }
}
