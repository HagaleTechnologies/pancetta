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
//! - `probe_hardware_tier(n)` — decodes an embedded off-air busy-band
//!   window through the live station's overlapped reference + native
//!   passes and production merge N times, returns percentile stats +
//!   classification + recommendations.
//!
//! PAN-369: the probe used to time the native decoder alone on one
//! synthetic CQ in silence, which measured only the per-decode floor. It
//! now times the work the coordinator does on every FT8 window (see
//! `pancetta/src/coordinator/ft8.rs`): the ft8_lib reference pass with its
//! display capture and the native AP pass started together by
//! [`crate::window_execution::run_overlapped_passes`], the join,
//! [`crate::merge::merge_ft8lib_first`] and
//! [`crate::merge::drop_unresolved_hash_twins`], on a committed 12 kHz
//! off-air recording of a busy 20 m window
//! (`tests/fixtures/wav/basicft8/live_now.wav`, 27 decodes standalone).
//! Tier thresholds remain provisional until quiet-host calibration lands.

use std::time::Duration;

/// Coarse classification of the host's decode capability.
///
/// Boundaries apply to the p95 wall clock of one busy production window
/// (PAN-369): the overlapped ft8_lib reference and native passes, their
/// join, and the production merge on the embedded off-air recording. The
/// 400 / 1200 ms boundaries are carried over from the earlier
/// synthetic-signal probe and stay **provisional**: no quiet-host
/// calibration on target hardware (M4, N100 MiniPC, Pi) exists yet for this
/// workload, and Pi-class hardware remains unmeasured.
///
/// Measured evidence (PAN-369 plan, 2026-11-18): on a two-CPU container
/// with a 1-minute load of 25.4, the old synthetic probe read p95 296 ms
/// (Fast) while the busy-window probe read p95 421 ms (Moderate), with 25
/// reference, 26 native and 26 final decodes per window. Those loaded
/// figures show the workload difference only; they are not a calibration.
///
/// Tier semantics:
/// - **Fast** (p95 < 400ms): the busy window decodes with ample slot
///   headroom; full pipeline at the Deep-equivalent budget.
/// - **Moderate** (400ms ≤ p95 < 1200ms): scoped fast-path recommended
///   to stay inside the 2000ms slot budget for in-QSO partners.
/// - **Slow** (p95 ≥ 1200ms): scoped fast-path is required, and reducing
///   multipass / OSD depth should be considered.
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

    /// Stable `u8` encoding for atomic storage (decoder-speed-overhaul
    /// Task 15: the coordinator remembers the last-resolved tier in an
    /// atomic so a live `Auto` decode-effort cycle can resolve the correct
    /// preset budget without re-probing). The mapping is fixed and MUST NOT
    /// change (`0` = Fast, `1` = Moderate, `2` = Slow).
    pub fn as_u8(&self) -> u8 {
        match self {
            HardwareTier::Fast => 0,
            HardwareTier::Moderate => 1,
            HardwareTier::Slow => 2,
        }
    }

    /// Decode a [`HardwareTier`] from its stable `u8` encoding (see
    /// [`HardwareTier::as_u8`]). Any unrecognized value decodes to the safe
    /// default [`HardwareTier::Fast`] — the same "innocent until proven
    /// otherwise" convention `scoped_fast_path` uses elsewhere.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => HardwareTier::Fast,
            1 => HardwareTier::Moderate,
            2 => HardwareTier::Slow,
            _ => HardwareTier::Fast,
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

/// The embedded off-air busy-band window (PAN-369): 15 s of 12 kHz mono
/// PCM16 recorded on 20 m, 27 decodes with the standalone cycle API. Kept in
/// the binary so the startup probe never depends on a fixture path.
const BUSY_WINDOW_WAV: &[u8] = include_bytes!("../tests/fixtures/wav/basicft8/live_now.wav");

/// Parse [`BUSY_WINDOW_WAV`] into one production-sized decode window:
/// the first [`crate::WINDOW_SAMPLES`] samples, normalized by 32768 like the
/// live audio path. Rejects anything but mono 12 kHz PCM16 holding at least
/// a full window. Runs once per probe, outside every timed iteration.
fn busy_window_samples() -> crate::Ft8Result<Vec<f32>> {
    use crate::{Ft8Error, SAMPLE_RATE, WINDOW_SAMPLES};

    let reader = hound::WavReader::new(std::io::Cursor::new(BUSY_WINDOW_WAV)).map_err(|e| {
        Ft8Error::ConfigError(format!("tier probe: busy window WAV unreadable: {e}"))
    })?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != SAMPLE_RATE
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err(Ft8Error::ConfigError(format!(
            "tier probe: busy window must be mono {SAMPLE_RATE} Hz PCM16, got {} ch {} Hz {}-bit {:?}",
            spec.channels, spec.sample_rate, spec.bits_per_sample, spec.sample_format
        )));
    }
    let samples = reader
        .into_samples::<i16>()
        .take(WINDOW_SAMPLES)
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<Result<Vec<f32>, _>>()
        .map_err(|e| Ft8Error::ConfigError(format!("tier probe: busy window WAV corrupt: {e}")))?;
    if samples.len() != WINDOW_SAMPLES {
        return Err(Ft8Error::InsufficientData {
            needed: WINDOW_SAMPLES,
            available: samples.len(),
        });
    }
    Ok(samples)
}

/// One timed busy-window decode: the reference and native results as each
/// pass returned them, and the final set production delivers.
#[derive(Debug)]
struct BusyWindowDecode {
    elapsed: Duration,
    reference: Vec<crate::DecodedMessage>,
    native: Vec<crate::DecodedMessage>,
    delivered: Vec<crate::DecodedMessage>,
}

/// Decode `samples` once the way the live coordinator decodes an FT8 window
/// (`pancetta/src/coordinator/ft8.rs`), timing dispatch through the join,
/// [`crate::merge::merge_ft8lib_first`] and
/// [`crate::merge::drop_unresolved_hash_twins`].
///
/// Fresh default decoders are built before the timer starts. The native pass
/// uses a neutral AP context, no partner filter and an unlimited budget, so
/// the measurement does not depend on the tier it is about to choose. A
/// panic in either branch or a native decode error fails the iteration; no
/// partial sample is ever recorded.
fn decode_busy_window_once(samples: &[f32]) -> crate::Ft8Result<BusyWindowDecode> {
    use crate::{
        ap::ApContext, merge, window_execution::run_overlapped_passes, DecodeBudget, Ft8Config,
        Ft8Decoder, Ft8Error, Protocol,
    };
    use std::time::Instant;

    let reference_decoder = Ft8Decoder::new(Ft8Config::default())?;
    let mut native_decoder = Ft8Decoder::new(Ft8Config::default())?;
    let ap_context = ApContext::default();

    let start = Instant::now();
    let outcome = run_overlapped_passes(
        // The decoder moves into the reference worker, as in production.
        move || {
            let (messages, _display_grid) = reference_decoder
                .decode_window_ft8lib_with_context_and_capture(samples, Protocol::Ft8);
            messages
        },
        || {
            native_decoder.decode_window_with_ap_scoped_partner_budgeted(
                samples,
                &ap_context,
                None,
                None,
                DecodeBudget::unlimited(),
            )
        },
    );
    let reference = outcome.reference.ok_or_else(|| {
        Ft8Error::MessageDecodingError("tier probe: ft8_lib reference pass panicked".to_string())
    })?;
    let (native, _report) = outcome.native.ok_or_else(|| {
        Ft8Error::MessageDecodingError("tier probe: native pass panicked".to_string())
    })??;
    let merged = merge::merge_ft8lib_first(reference.clone(), native.clone());
    let (delivered, _dropped) = merge::drop_unresolved_hash_twins(merged.messages, Protocol::Ft8);
    let elapsed = start.elapsed();

    Ok(BusyWindowDecode {
        elapsed,
        reference,
        native,
        delivered,
    })
}

/// Probe the host's decode capability by running N busy-window decodes
/// through the production overlapped passes and merge (PAN-369). Returns
/// percentile stats + tier classification.
///
/// `n` should be ≥ 3 for stable p95; ≥ 10 for stable p99. One untimed
/// warmup decode runs first; exactly `n` timed samples follow.
///
/// Errors (never a partial sample) when the embedded window cannot be read,
/// a decode fails or panics, or this build has no ft8_lib reference backend
/// (`ft8lib_stub`): without the reference pass the measurement would not be
/// the production workload.
pub fn probe_hardware_tier(n: usize) -> crate::Ft8Result<TierProbeResult> {
    assert!(n >= 1, "tier probe needs n >= 1 iteration");

    if cfg!(ft8lib_stub) {
        return Err(crate::Ft8Error::ConfigError(
            "tier probe: built without the ft8_lib reference decoder (ft8lib_stub); \
             the production two-pass window cannot be measured"
                .to_string(),
        ));
    }

    let window = busy_window_samples()?;

    // Warmup pass — stabilizes CPU caches, FFT planner state, thread pools.
    decode_busy_window_once(&window)?;

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        samples.push(decode_busy_window_once(&window)?.elapsed);
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
    fn tier_as_u8_from_u8_round_trip_for_every_variant() {
        for tier in [
            HardwareTier::Fast,
            HardwareTier::Moderate,
            HardwareTier::Slow,
        ] {
            assert_eq!(HardwareTier::from_u8(tier.as_u8()), tier);
        }
    }

    #[test]
    fn tier_from_u8_unrecognized_value_defaults_to_fast() {
        assert_eq!(HardwareTier::from_u8(255), HardwareTier::Fast);
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

    /// AC2 (PAN-369): exactly `n` timed samples after the warmup, and the
    /// tier is the classification of their p95.
    #[cfg(not(ft8lib_stub))]
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
        assert_eq!(result.tier, classify_tier(result.p95.as_millis() as u64));
        assert_eq!(result.recommendations, recommend_actions(result.tier));
    }

    /// A build without the C reference decoder cannot measure the production
    /// two-pass window, so the probe reports that instead of timing native
    /// alone.
    #[cfg(ft8lib_stub)]
    #[test]
    fn probe_hardware_tier_smoke_yields_sane_stats() {
        let err = probe_hardware_tier(2).expect_err("stub build has no reference pass");
        assert!(err.to_string().contains("ft8lib_stub"), "{err}");
    }
}

#[cfg(test)]
#[path = "tier_probe/tests/busy_window_tests.rs"]
mod busy_window_tests;
