//! Cheap noise-floor estimator used by the curate binary.
//!
//! For an FT8 WAV at 12 kHz mono, "noise floor" is approximated as the
//! median absolute amplitude of the lower 25th percentile of samples.
//! This catches busy bands (high noise floor from many overlapping signals)
//! without needing a full FFT-based spectral estimate.

use rand::rngs::StdRng;
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::impulsiveness::IMPULSE_BLOCK_SAMPLES;

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

/// Bernoulli-timed broadband impulse bursts (hb-256's lightning-static
/// plant, first built privately in batch101). At each FT8 symbol boundary a
/// burst begins with `burst_start_prob`; its duration is uniform in
/// `1..=max_duration_symbols`; its waveform is white Gaussian noise
/// `amp_db` ~ Uniform(`amp_db_min`, `amp_db_max`) dB above the base σ.
/// Overlapping bursts add in variance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImpulseBurstPlant {
    pub burst_start_prob: f64,
    pub max_duration_symbols: usize,
    pub amp_db_min: f64,
    pub amp_db_max: f64,
}

impl ImpulseBurstPlant {
    /// batch101's plant: ≈4% of symbols hit.
    pub const BATCH101: Self = Self {
        burst_start_prob: 0.02,
        max_duration_symbols: 3,
        amp_db_min: 10.0,
        amp_db_max: 30.0,
    };
    /// PAN-162's decision plant: 2.5× batch101's burst rate (≈10% of symbols).
    pub const STORM: Self = Self {
        burst_start_prob: 0.05,
        ..Self::BATCH101
    };
    /// PAN-162's descriptive dose-response plant (≈19% of symbols).
    pub const SEVERE: Self = Self {
        burst_start_prob: 0.10,
        ..Self::BATCH101
    };
}

/// Adds `plant`'s bursts IN PLACE on top of base noise drawn at
/// `sigma_base`. The RNG call sequence is batch101's, so its output is
/// byte-identical to the private copy it replaces.
pub fn add_impulse_bursts(
    rng: &mut StdRng,
    noise: &mut [f32],
    sigma_base: f32,
    plant: &ImpulseBurstPlant,
) {
    let n_symbols = noise.len().div_ceil(IMPULSE_BLOCK_SAMPLES);
    let mut extra_var = vec![0.0f64; n_symbols];
    for s in 0..n_symbols {
        if rng.random_range(0.0f64..1.0) < plant.burst_start_prob {
            let dur = rng.random_range(1usize..=plant.max_duration_symbols);
            let amp_db = rng.random_range(plant.amp_db_min..plant.amp_db_max);
            let sigma_burst = sigma_base as f64 * 10f64.powf(amp_db / 20.0);
            for d in 0..dur {
                if s + d < n_symbols {
                    extra_var[s + d] += sigma_burst * sigma_burst;
                }
            }
        }
    }
    for (s, &var) in extra_var.iter().enumerate() {
        if var <= 0.0 {
            continue;
        }
        let sigma = (var as f32).sqrt();
        let start = s * IMPULSE_BLOCK_SAMPLES;
        let end = ((s + 1) * IMPULSE_BLOCK_SAMPLES).min(noise.len());
        let burst = gaussian_noise(rng, end - start, sigma);
        for (x, b) in noise[start..end].iter_mut().zip(burst) {
            *x += b;
        }
    }
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
    /// Frozen copy of batch101's private `add_impulse_bursts`
    /// (`batch101_impulse_robust_llr_kill_switch.rs`, before PAN-162).
    fn batch101_oracle(rng: &mut StdRng, noise: &mut [f32], sigma_base: f32) {
        const SYMBOL_SAMPLES: usize = 1920;
        let n_symbols = noise.len().div_ceil(SYMBOL_SAMPLES);
        let mut extra_var = vec![0.0f64; n_symbols];
        for s in 0..n_symbols {
            if rng.random_range(0.0f64..1.0) < 0.02 {
                let dur = rng.random_range(1usize..=3);
                let amp_db = rng.random_range(10.0f64..30.0);
                let sigma_burst = sigma_base as f64 * 10f64.powf(amp_db / 20.0);
                for d in 0..dur {
                    if s + d < n_symbols {
                        extra_var[s + d] += sigma_burst * sigma_burst;
                    }
                }
            }
        }
        for (s, &var) in extra_var.iter().enumerate() {
            if var <= 0.0 {
                continue;
            }
            let sigma = (var as f32).sqrt();
            let start = s * SYMBOL_SAMPLES;
            let end = ((s + 1) * SYMBOL_SAMPLES).min(noise.len());
            let burst = gaussian_noise(rng, end - start, sigma);
            for (x, b) in noise[start..end].iter_mut().zip(burst) {
                *x += b;
            }
        }
    }

    #[test]
    fn batch101_plant_is_byte_identical_to_the_private_copy() {
        for offset in [0u64, 7033, 12049] {
            let seed = 0xB101_0000u64 + offset;
            let mut a = StdRng::seed_from_u64(seed);
            let mut b = StdRng::seed_from_u64(seed);
            let mut na = gaussian_noise(&mut a, 151_680, 0.02);
            let mut nb = gaussian_noise(&mut b, 151_680, 0.02);
            batch101_oracle(&mut a, &mut na, 0.02);
            add_impulse_bursts(&mut b, &mut nb, 0.02, &ImpulseBurstPlant::BATCH101);
            assert_eq!(na, nb, "seed offset {offset}");
            assert_eq!(
                a.random_range(0u64..u64::MAX),
                b.random_range(0u64..u64::MAX)
            );
        }
    }

    #[test]
    fn storm_plants_differ_only_in_burst_rate() {
        let base = ImpulseBurstPlant::BATCH101;
        for (plant, p) in [
            (ImpulseBurstPlant::STORM, 0.05),
            (ImpulseBurstPlant::SEVERE, 0.10),
        ] {
            assert_eq!(plant.burst_start_prob, p);
            assert_eq!(
                ImpulseBurstPlant {
                    burst_start_prob: base.burst_start_prob,
                    ..plant
                },
                base
            );
        }
    }

    #[test]
    fn plants_sit_on_the_instrument_scale() {
        use crate::impulsiveness::slot_impulsiveness;
        for (plant, lo, hi) in [
            (ImpulseBurstPlant::BATCH101, 0.03, 0.06),
            (ImpulseBurstPlant::STORM, 0.07, 0.13),
            (ImpulseBurstPlant::SEVERE, 0.15, 0.23),
        ] {
            let mean = (0..32u64)
                .map(|seed| {
                    let mut rng = StdRng::seed_from_u64(seed);
                    let mut x = gaussian_noise(&mut rng, 151_680, 0.03);
                    add_impulse_bursts(&mut rng, &mut x, 0.03, &plant);
                    slot_impulsiveness(&x).impulsive_block_fraction
                })
                .sum::<f64>()
                / 32.0;
            assert!(
                (lo..=hi).contains(&mean),
                "p={} mean {mean}",
                plant.burst_start_prob
            );
        }
    }
}
