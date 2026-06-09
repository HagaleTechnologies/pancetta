//! Per-candidate adaptive frequency tracker.
//!
//! Clean-room Rust port inspired by spec ref
//! `research/specs/spec-js8call-per-candidate-frequency-tracker.md`. The
//! spec describes a JS8Call-Improved mechanism (`JS8_Mode/FrequencyTracker.{h,cpp}`,
//! GPL-3.0) — pancetta's implementation is written from the prose-only
//! algorithm spec without reading the peer source.
//!
//! # Mechanism
//!
//! The candidate generator emits a coarse frequency estimate quantised to
//! the FFT bin grid. A residual frequency error (up to half a bin plus
//! any in-burst drift contributed by the transmitter or the ionosphere)
//! remains for the rest of the slot. The per-candidate tracker treats the
//! Costas pilot tones as a frequency reference, accumulates a damped
//! residual estimate, and applies it as a phase rotation to each
//! symbol's complex samples before they reach the demapper.
//!
//! # Algorithm (PROSE, no peer code consulted)
//!
//! 1. Initialise with the coarse frequency and a zero offset.
//! 2. For each chunk of samples (typically one symbol's worth), rotate
//!    by `exp(-j × 2π × current_offset × n / sample_rate)` before
//!    demapping.
//! 3. At each pilot opportunity (three Costas blocks per FT8 slot),
//!    consume a residual measurement, scale by `alpha`, clamp the step
//!    to `±max_step_hz`, then clamp the running offset to
//!    `±max_error_hz`. Record the step in an EMA for telemetry.
//! 4. Discard at end of candidate — the tracker is single-use.
//!
//! # Default state
//!
//! `Ft8Config::per_candidate_freq_tracker_enabled` defaults to `false`.
//! When disabled the decoder hot path is byte-identical to the legacy
//! behaviour: no tracker is instantiated, no rotation is applied, no
//! update calls fire. The mechanism is shipped behind the gate so the
//! research harness can A/B it on drifting-station corpora.

use num_complex::Complex;

/// Configuration knobs for [`FrequencyTracker`].
///
/// All defaults match the prose-spec recommendations (`alpha = 0.2`,
/// `max_step_hz = 1.5`, `max_error_hz = 5.0`). The defaults track the
/// spec; see `research/specs/spec-js8call-per-candidate-frequency-tracker.md`.
#[derive(Debug, Clone, Copy)]
pub struct FreqTrackerConfig {
    /// Damping factor for the running estimate. Typical 0.1–0.3.
    pub alpha: f64,
    /// Per-update cap on how much the estimate can move in one update,
    /// in Hz. Typical 1.0–2.0.
    pub max_step_hz: f64,
    /// Absolute bound on the running offset (relative to coarse), in Hz.
    /// Typical 5.0 ≈ ±0.8 FFT bins at FT8's 6.25 Hz tone spacing.
    pub max_error_hz: f64,
    /// EMA factor for the step-size telemetry trace. Smaller = smoother
    /// rolling estimate. Pure observability; doesn't change behaviour.
    pub step_ema_alpha: f64,
}

impl Default for FreqTrackerConfig {
    fn default() -> Self {
        Self {
            alpha: 0.2,
            max_step_hz: 1.5,
            max_error_hz: 5.0,
            step_ema_alpha: 0.3,
        }
    }
}

/// Per-candidate adaptive frequency tracker.
///
/// State is intentionally not `Copy` — each candidate constructs its own
/// tracker, mutates it during the symbol stream, and discards it.
#[derive(Debug, Clone)]
pub struct FrequencyTracker {
    coarse_hz: f64,
    current_offset_hz: f64,
    sample_rate: f64,
    cfg: FreqTrackerConfig,
    /// Rolling average of |actual step| for telemetry.
    step_ema_hz: f64,
    /// Count of `update` calls (including no-op skips on bad measurements).
    update_count: u32,
    /// Count of `update` calls that were skipped because the residual
    /// was non-finite. Surfaced for diagnostics.
    skipped_count: u32,
}

impl FrequencyTracker {
    /// Construct a fresh tracker for one candidate at coarse frequency
    /// `coarse_hz` (Hz). `sample_rate` is the audio sample rate (12000
    /// for FT8 / JS8 audio).
    pub fn new(coarse_hz: f64, sample_rate: f64, cfg: FreqTrackerConfig) -> Self {
        Self {
            coarse_hz,
            current_offset_hz: 0.0,
            sample_rate,
            cfg,
            step_ema_hz: 0.0,
            update_count: 0,
            skipped_count: 0,
        }
    }

    /// Read the current frequency-error estimate (Hz, relative to
    /// `coarse_hz`).
    pub fn current_offset_hz(&self) -> f64 {
        self.current_offset_hz
    }

    /// Absolute frequency estimate (`coarse_hz + current_offset_hz`).
    pub fn current_hz(&self) -> f64 {
        self.coarse_hz + self.current_offset_hz
    }

    /// EMA of update step sizes (Hz) — pure telemetry. Drifting rigs
    /// produce large values; clean rigs stay near zero.
    pub fn step_ema_hz(&self) -> f64 {
        self.step_ema_hz
    }

    /// Number of `update` calls received.
    pub fn update_count(&self) -> u32 {
        self.update_count
    }

    /// Number of `update` calls skipped because the residual was non-finite.
    pub fn skipped_count(&self) -> u32 {
        self.skipped_count
    }

    /// Consume a residual frequency measurement (Hz) from the most
    /// recent Costas pilot block and update the running offset.
    ///
    /// `residual_hz` is the *additional* drift the pilot tones show
    /// after the previously-applied correction. Non-finite values
    /// (NaN/inf — typical signal of a sync block too weak to measure)
    /// are silently skipped so a single poisoned measurement cannot
    /// yank the tracker off-course.
    pub fn update(&mut self, residual_hz: f64) {
        self.update_count = self.update_count.saturating_add(1);
        if !residual_hz.is_finite() {
            self.skipped_count = self.skipped_count.saturating_add(1);
            return;
        }
        // Damped step, clamped to ±max_step_hz so a noisy pilot can't
        // jerk the tracker.
        let raw_step = self.cfg.alpha * residual_hz;
        let clamped_step = raw_step.clamp(-self.cfg.max_step_hz, self.cfg.max_step_hz);
        let mut new_offset = self.current_offset_hz + clamped_step;
        // Clamp the absolute offset so the tracker can't wander.
        new_offset = new_offset.clamp(-self.cfg.max_error_hz, self.cfg.max_error_hz);
        // Telemetry: EMA of the actual applied step magnitude.
        let actual_step = (new_offset - self.current_offset_hz).abs();
        self.step_ema_hz = (1.0 - self.cfg.step_ema_alpha) * self.step_ema_hz
            + self.cfg.step_ema_alpha * actual_step;
        self.current_offset_hz = new_offset;
    }

    /// Rotate `samples` in-place by the current frequency offset.
    ///
    /// `chunk_start_n` is the absolute sample index of `samples[0]`
    /// within the audio buffer being decoded. The rotation phase ramps
    /// according to `exp(-j × 2π × current_offset_hz × n / sample_rate)`
    /// so the rotation phase is *coherent* across successive chunks of
    /// the same candidate.
    ///
    /// When `current_offset_hz == 0.0` the rotation is the identity and
    /// the buffer is left untouched (early-return) — this preserves
    /// byte-identity for the disabled hot path even if a tracker is
    /// instantiated but never `update`d.
    pub fn apply(&self, samples: &mut [Complex<f64>], chunk_start_n: usize) {
        if self.current_offset_hz == 0.0 || samples.is_empty() {
            return;
        }
        let pi2 = 2.0 * std::f64::consts::PI;
        let phase_step_angle = -pi2 * self.current_offset_hz / self.sample_rate;
        let phase_step = Complex::new(phase_step_angle.cos(), phase_step_angle.sin());
        let initial_angle = -pi2 * self.current_offset_hz * chunk_start_n as f64 / self.sample_rate;
        let mut rotator = Complex::new(initial_angle.cos(), initial_angle.sin());
        for s in samples.iter_mut() {
            *s = *s * rotator;
            rotator = rotator * phase_step;
        }
    }
}

// ============================================================================
// Tests — own module so additions don't collide with decoder.rs::tests
// ============================================================================

#[cfg(test)]
mod freq_tracker_tests {
    use super::*;
    use num_complex::Complex;

    const SR: f64 = 12_000.0;

    fn default_cfg() -> FreqTrackerConfig {
        FreqTrackerConfig::default()
    }

    #[test]
    fn default_offset_is_zero() {
        let t = FrequencyTracker::new(1500.0, SR, default_cfg());
        assert_eq!(t.current_offset_hz(), 0.0);
        assert_eq!(t.current_hz(), 1500.0);
        assert_eq!(t.update_count(), 0);
    }

    #[test]
    fn apply_is_identity_when_offset_zero() {
        // Default-OFF byte-identity: without an `update` call, `apply`
        // must leave samples untouched bit-for-bit.
        let t = FrequencyTracker::new(1500.0, SR, default_cfg());
        let mut buf: Vec<Complex<f64>> = (0..32)
            .map(|i| Complex::new(i as f64, (i as f64) * 0.5))
            .collect();
        let before = buf.clone();
        t.apply(&mut buf, 0);
        for (a, b) in buf.iter().zip(before.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
        // And at a non-zero chunk start.
        t.apply(&mut buf, 9999);
        for (a, b) in buf.iter().zip(before.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn empty_apply_is_safe() {
        let t = FrequencyTracker::new(1500.0, SR, default_cfg());
        let mut buf: Vec<Complex<f64>> = vec![];
        t.apply(&mut buf, 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn step_clamped_to_max() {
        // With alpha=0.2 and max_step=1.5, a raw residual of 100 Hz
        // would yield 20 Hz of raw step; clamp must cap it at 1.5.
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        t.update(100.0);
        assert!((t.current_offset_hz() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn offset_clamped_to_max_error_hz() {
        // Push repeatedly past the absolute bound — the tracker must
        // saturate at +max_error_hz (5.0 Hz default).
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        for _ in 0..50 {
            t.update(100.0);
        }
        assert!((t.current_offset_hz() - 5.0).abs() < 1e-9);
        // Pull in the opposite direction — must saturate at -max_error_hz.
        for _ in 0..50 {
            t.update(-100.0);
        }
        assert!((t.current_offset_hz() - (-5.0)).abs() < 1e-9);
    }

    #[test]
    fn nan_inf_residual_is_skipped() {
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        t.update(1.0); // valid, brings offset to 0.2
        let before = t.current_offset_hz();
        t.update(f64::NAN);
        t.update(f64::INFINITY);
        t.update(f64::NEG_INFINITY);
        assert_eq!(t.current_offset_hz(), before);
        // Three skips were counted.
        assert_eq!(t.skipped_count(), 3);
        // update_count counts all calls (incl. skipped).
        assert_eq!(t.update_count(), 4);
    }

    #[test]
    fn static_freq_signal_tracker_stays_near_initial() {
        // Simulate three Costas pilots reporting tiny residuals (clean,
        // non-drifting transmitter). With damping the tracker should
        // converge near zero, certainly within ±0.5 Hz.
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        // Three Costas opportunities per FT8 slot.
        for _ in 0..3 {
            t.update(0.05); // ~5 cHz residual = floor-of-the-floor
        }
        let off = t.current_offset_hz();
        assert!(off.abs() < 0.5, "static-freq tracker drifted to {} Hz", off);
    }

    #[test]
    fn linear_drift_tracker_follows() {
        // Simulate a transmitter drifting +0.6 Hz between each Costas
        // block (a generous chirp; ±2 Hz over the slot). The tracker
        // should integrate this and end up in the positive range.
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        let residuals = [0.6, 0.6, 0.6]; // three pilots, all +0.6
        for r in residuals {
            t.update(r);
        }
        let off = t.current_offset_hz();
        // alpha=0.2: each update adds 0.12, so after three updates
        // accumulator should be near 0.36 Hz — non-zero, positive, and
        // well below the clamp bounds.
        assert!(off > 0.0, "tracker did not move positively, got {}", off);
        assert!(
            off < 5.0,
            "tracker exceeded its absolute bound, got {}",
            off
        );
        // Direction agreement check.
        let mut t2 = FrequencyTracker::new(1500.0, SR, default_cfg());
        for r in residuals {
            t2.update(-r);
        }
        assert!(
            t2.current_offset_hz() < 0.0,
            "tracker failed to track negative drift"
        );
    }

    #[test]
    fn noise_only_does_not_diverge() {
        // Drive the tracker with bounded random noise; with the absolute
        // clamp + damped step + ±max_step cap, the running offset must
        // stay within ±max_error_hz no matter what the residuals look
        // like.
        let mut t = FrequencyTracker::new(1500.0, SR, default_cfg());
        // Deterministic pseudo-random noise (no rng crate; xorshift on the loop var).
        let mut state: u64 = 0xdead_beef_cafe_f00d;
        for _ in 0..1000 {
            // Crude xorshift64.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Map to noisy residual in ±10 Hz.
            let r = ((state as i64 as f64) / (i64::MAX as f64)) * 10.0;
            t.update(r);
            assert!(
                t.current_offset_hz().abs() <= 5.0 + 1e-12,
                "offset escaped clamp: {}",
                t.current_offset_hz()
            );
        }
    }

    #[test]
    fn apply_rotates_unit_vector_by_expected_phase() {
        // With a +1 Hz offset, applying to a unit-vector buffer of length
        // sample_rate should accumulate ~2π radians (one full revolution).
        let mut t = FrequencyTracker::new(0.0, SR, default_cfg());
        // Force a non-zero offset that is well below the clamp.
        // alpha*residual = 0.2 → residual=5 produces step=1.0.
        t.update(5.0);
        let off = t.current_offset_hz();
        assert!((off - 1.0).abs() < 1e-9, "expected offset 1.0, got {}", off);

        // Build N samples = 100, rotate at 1 Hz, sample_rate 12000.
        let n = 1200usize;
        let mut samples: Vec<Complex<f64>> = vec![Complex::new(1.0, 0.0); n];
        t.apply(&mut samples, 0);
        // The phase at index n should be -2π * 1 Hz * n / SR = -2π *
        // 1200/12000 = -0.628... radians = -36 degrees.
        let last = samples[n - 1];
        let phase = last.im.atan2(last.re);
        let expected = -2.0 * std::f64::consts::PI * 1.0 * (n - 1) as f64 / SR;
        assert!(
            (phase - expected).abs() < 1e-6,
            "phase mismatch: got {}, expected {}",
            phase,
            expected
        );
        // Magnitude unchanged.
        assert!((last.norm() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn apply_is_coherent_across_chunks() {
        // Two successive `apply` calls with chunk_start_n bookkeeping
        // must produce the same result as one big `apply` of the
        // concatenated buffer. This is the property the per-symbol
        // rotation in the decoder relies on.
        let mut t = FrequencyTracker::new(0.0, SR, default_cfg());
        t.update(2.0); // alpha=0.2 → step=0.4, well below clamp

        let n = 64usize;
        let mut a: Vec<Complex<f64>> = (0..n).map(|i| Complex::new(i as f64, 0.0)).collect();
        let mut b: Vec<Complex<f64>> = (n..(2 * n)).map(|i| Complex::new(i as f64, 0.0)).collect();
        let mut whole: Vec<Complex<f64>> =
            (0..(2 * n)).map(|i| Complex::new(i as f64, 0.0)).collect();

        t.apply(&mut a, 0);
        t.apply(&mut b, n);
        t.apply(&mut whole, 0);

        for i in 0..n {
            let lhs = a[i];
            let rhs = whole[i];
            assert!(
                (lhs.re - rhs.re).abs() < 1e-9 && (lhs.im - rhs.im).abs() < 1e-9,
                "chunked apply diverges from whole at index {}",
                i
            );
        }
        for i in 0..n {
            let lhs = b[i];
            let rhs = whole[n + i];
            assert!(
                (lhs.re - rhs.re).abs() < 1e-7 && (lhs.im - rhs.im).abs() < 1e-7,
                "chunked apply diverges from whole at index {} (second chunk)",
                n + i
            );
        }
    }

    #[test]
    fn step_ema_grows_with_drift() {
        // A drifting input should produce a nonzero step EMA; a static
        // input should leave it near zero.
        let mut t_drift = FrequencyTracker::new(0.0, SR, default_cfg());
        for r in [0.5_f64, 0.5, 0.5] {
            t_drift.update(r);
        }
        assert!(t_drift.step_ema_hz() > 0.0);

        let mut t_static = FrequencyTracker::new(0.0, SR, default_cfg());
        for _ in 0..3 {
            t_static.update(0.0);
        }
        assert!(t_static.step_ema_hz() < 1e-9);
    }

    #[test]
    fn skipped_count_does_not_advance_offset_or_ema() {
        let mut t = FrequencyTracker::new(0.0, SR, default_cfg());
        t.update(1.0);
        let off_before = t.current_offset_hz();
        let ema_before = t.step_ema_hz();
        t.update(f64::NAN);
        assert_eq!(t.current_offset_hz(), off_before);
        assert_eq!(t.step_ema_hz(), ema_before);
        assert_eq!(t.skipped_count(), 1);
    }
}
