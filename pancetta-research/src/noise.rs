//! Cheap noise-floor estimator used by the curate binary.
//!
//! For an FT8 WAV at 12 kHz mono, "noise floor" is approximated as the
//! median absolute amplitude of the lower 25th percentile of samples.
//! This catches busy bands (high noise floor from many overlapping signals)
//! without needing a full FFT-based spectral estimate.

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
