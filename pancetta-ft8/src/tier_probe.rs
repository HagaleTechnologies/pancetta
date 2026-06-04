//! Hardware-tier probe: runtime wall-clock classification of the host
//! machine into Fast / Moderate / Slow tiers, with recommended runtime
//! actions per tier.
//!
//! Motivation (hb-216): hb-091 Session 3 measured M4 Mac Mini full-decode
//! wall-clock p99=2332ms vs scoped p99=866ms. Pancetta fires decode at
//! t=13s with a 2000ms slot budget; M4 already busts at p95. Lower-tier
//! hardware (Windows 11 MiniPCs, ARMv7) will bust more frequently, where
//! the gated `PANCETTA_SCOPED_FAST_PATH=1` env var becomes operationally
//! important. This module classifies the host so the runtime (or a
//! runbook) can flip the right knobs automatically.
//!
//! API:
//! - `HardwareTier` enum (Fast | Moderate | Slow).
//! - `classify_tier(p95_ms)` — pure function mapping wall-clock to tier.
//! - `recommend_actions(tier)` — per-tier runtime recommendations.
//! - `probe_hardware_tier(n)` — gated on `transmit` feature; generates a
//!   synthetic FT8 signal, decodes N times, returns percentile stats +
//!   classification + recommendations.
//!
//! Tier thresholds are conservative initial values, tunable as
//! cross-machine data lands.

use std::time::Duration;

/// Coarse classification of the host's decode capability.
///
/// Boundaries are tuned against the **synthetic-signal baseline** the
/// probe measures, not against the real-world hard-200 distribution.
/// A clean synthetic FT8 message decodes via the easy fast path
/// (pass 1 hits, no multipass, no OSD fallback) — so the probe's p95
/// is roughly the **per-decode floor** on this hardware. Real-world
/// p95 is typically 5-10× higher because the bimodal tail (hard WAVs
/// triggering multipass + OSD) dominates the upper percentiles.
///
/// Calibration anchors (M4 Mac Mini, 2026-06-04):
///   synthetic p95 = 211ms → hard-200 p95 = 2132ms (≈10× multiplier).
///
/// Tier semantics (synthetic-basis):
/// - **Fast** (p95 < 400ms): M4-class. Real-world p95 likely under
///   3000ms; full pipeline within slot budget on most decodes.
/// - **Moderate** (400ms ≤ p95 < 1200ms): typical MiniPC (Intel N100
///   class). Real-world p95 likely 4000-8000ms; scoped fast-path
///   recommended to stay inside the 2000ms slot budget for in-QSO
///   partners.
/// - **Slow** (p95 ≥ 1200ms): older / very minimal hardware (ARMv7
///   Pi 4 class). Real-world p95 likely 8000ms+; scoped fast-path
///   is required, and reducing multipass / OSD depth should be
///   considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareTier {
    Fast,
    Moderate,
    Slow,
}

impl HardwareTier {
    /// Human-readable name for logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            HardwareTier::Fast => "fast",
            HardwareTier::Moderate => "moderate",
            HardwareTier::Slow => "slow",
        }
    }
}

const FAST_THRESHOLD_MS: u64 = 400;
const MODERATE_THRESHOLD_MS: u64 = 1200;

/// Classify a measured p95 wall-clock value (in ms) into a tier.
///
/// Pure function — no I/O, deterministic. The probe entry-point feeds
/// `probe.p95.as_millis() as u64` here after measurement.
pub fn classify_tier(p95_ms: u64) -> HardwareTier {
    if p95_ms < FAST_THRESHOLD_MS {
        HardwareTier::Fast
    } else if p95_ms < MODERATE_THRESHOLD_MS {
        HardwareTier::Moderate
    } else {
        HardwareTier::Slow
    }
}

/// A single runtime recommendation surfaced by the tier classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierRecommendation {
    /// Short identifier (e.g., "enable-scoped-fast-path").
    pub key: &'static str,
    /// One-line operator-facing rationale.
    pub message: &'static str,
}

/// Per-tier recommended runtime actions.
///
/// Fast tier returns an empty vector (current defaults work). Moderate
/// recommends scoping the in-QSO decode. Slow adds further multipass
/// and OSD-depth advice.
pub fn recommend_actions(tier: HardwareTier) -> Vec<TierRecommendation> {
    match tier {
        HardwareTier::Fast => Vec::new(),
        HardwareTier::Moderate => vec![TierRecommendation {
            key: "enable-scoped-fast-path",
            message: "Set PANCETTA_SCOPED_FAST_PATH=1 to scope in-QSO decode and stay inside the 2000ms slot budget.",
        }],
        HardwareTier::Slow => vec![
            TierRecommendation {
                key: "enable-scoped-fast-path",
                message: "Set PANCETTA_SCOPED_FAST_PATH=1 — full decode reliably busts the slot budget on this hardware.",
            },
            TierRecommendation {
                key: "consider-lower-multipass",
                message: "Consider lowering `max_decode_passes` in Ft8Config; multipass + OSD fallback dominate the long tail of decode wall-clock.",
            },
        ],
    }
}

/// Result of a hardware-tier probe.
#[derive(Debug, Clone)]
pub struct TierProbeResult {
    /// Per-iteration sorted decode wall-clock samples.
    pub samples: Vec<Duration>,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub tier: HardwareTier,
    pub recommendations: Vec<TierRecommendation>,
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Build a probe result from a vector of unsorted samples.
///
/// Exposed separately from `probe_hardware_tier` so callers can supply
/// samples gathered out-of-band (e.g., from production decode telemetry,
/// not just the synthetic probe).
pub fn summarize_samples(mut samples: Vec<Duration>) -> TierProbeResult {
    samples.sort();
    let p50 = percentile(&samples, 0.50);
    let p95 = percentile(&samples, 0.95);
    let p99 = percentile(&samples, 0.99);
    let max = samples.last().copied().unwrap_or(Duration::ZERO);
    let tier = classify_tier(p95.as_millis() as u64);
    let recommendations = recommend_actions(tier);
    TierProbeResult {
        samples,
        p50,
        p95,
        p99,
        max,
        tier,
        recommendations,
    }
}

/// Probe the host's decode capability by running N decodes of a
/// synthetic FT8 signal. Returns percentile stats + tier classification.
///
/// `n` should be ≥ 3 for stable p95; ≥ 10 for stable p99. The first
/// decode is treated as a warmup and discarded.
#[cfg(feature = "transmit")]
pub fn probe_hardware_tier(n: usize) -> crate::Ft8Result<TierProbeResult> {
    use crate::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
    use std::time::Instant;

    assert!(n >= 1, "tier probe needs n >= 1 iteration");

    // Generate a synthetic FT8 message once; reuse the buffer across iterations.
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("CQ K5ARH EM10", None).map_err(|e| {
        crate::Ft8Error::ConfigError(format!("tier probe: failed to encode synthetic CQ: {e}"))
    })?;
    let mut modulator = Ft8Modulator::new_default()?;
    let mut tx = modulator.modulate_symbols(&symbols, 500.0).map_err(|e| {
        crate::Ft8Error::ConfigError(format!("tier probe: failed to modulate synthetic CQ: {e}"))
    })?;
    tx.resize(WINDOW_SAMPLES, 0.0);

    // Warmup pass — stabilizes CPU caches, FFT planner state, etc.
    {
        let mut decoder = Ft8Decoder::new(Ft8Config::default())?;
        let _ = decoder.decode_window(&tx);
    }

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let mut decoder = Ft8Decoder::new(Ft8Config::default())?;
        let start = Instant::now();
        let _ = decoder.decode_window(&tx)?;
        samples.push(start.elapsed());
    }

    Ok(summarize_samples(samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_tier_below_fast_threshold_is_fast() {
        assert_eq!(classify_tier(0), HardwareTier::Fast);
        assert_eq!(classify_tier(399), HardwareTier::Fast);
    }

    #[test]
    fn classify_tier_at_moderate_boundary_is_moderate() {
        assert_eq!(classify_tier(400), HardwareTier::Moderate);
        assert_eq!(classify_tier(1199), HardwareTier::Moderate);
    }

    #[test]
    fn classify_tier_at_slow_boundary_is_slow() {
        assert_eq!(classify_tier(1200), HardwareTier::Slow);
        assert_eq!(classify_tier(5000), HardwareTier::Slow);
    }

    #[test]
    fn recommend_actions_fast_is_empty() {
        assert!(recommend_actions(HardwareTier::Fast).is_empty());
    }

    #[test]
    fn recommend_actions_moderate_suggests_scoped_fast_path() {
        let recs = recommend_actions(HardwareTier::Moderate);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].key, "enable-scoped-fast-path");
    }

    #[test]
    fn recommend_actions_slow_includes_multipass_advice() {
        let recs = recommend_actions(HardwareTier::Slow);
        assert!(recs.iter().any(|r| r.key == "enable-scoped-fast-path"));
        assert!(recs.iter().any(|r| r.key == "consider-lower-multipass"));
    }

    #[test]
    fn summarize_samples_computes_sorted_percentiles() {
        let samples = vec![
            Duration::from_millis(100),
            Duration::from_millis(150),
            Duration::from_millis(200),
            Duration::from_millis(250),
            Duration::from_millis(300),
        ];
        let result = summarize_samples(samples);
        assert_eq!(result.p50, Duration::from_millis(200));
        assert_eq!(result.max, Duration::from_millis(300));
        assert_eq!(result.tier, HardwareTier::Fast); // 300ms p95 < 400ms
    }

    #[test]
    fn summarize_samples_classifies_moderate() {
        let samples: Vec<Duration> = (0..20)
            .map(|i| Duration::from_millis(500 + i * 10))
            .collect();
        let result = summarize_samples(samples);
        assert_eq!(result.tier, HardwareTier::Moderate);
        assert_eq!(result.recommendations.len(), 1);
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn probe_hardware_tier_smoke_yields_sane_stats() {
        let result = probe_hardware_tier(2).expect("probe");
        assert_eq!(result.samples.len(), 2);
        // Percentiles must be monotonically ordered.
        assert!(result.p50 <= result.p95);
        assert!(result.p95 <= result.p99);
        assert!(result.p99 <= result.max);
        // Decode wall-clock should be positive (>0ms).
        assert!(result.max.as_millis() > 0);
    }
}
