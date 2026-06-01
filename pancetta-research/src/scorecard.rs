use crate::Mode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level scorecard JSON document. See spec section "Eval binary —
/// Scorecard JSON shape".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scorecard {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub mode: Mode,
    pub git: GitInfo,
    pub build: BuildInfo,
    pub harness: HarnessInfo,
    pub config: ConfigInfo,
    /// Keyed by tier name ("synth-clean", "fixtures", "curated-hard-200", …)
    pub tiers: BTreeMap<String, TierResult>,
    pub composite: CompositeInfo,
    pub regressions: RegressionFlags,
    pub notes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitInfo {
    pub branch: String,
    pub head_sha: String,
    pub main_merge_base: String,
    pub dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildInfo {
    pub rustc_version: String,
    pub release: bool,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub harness_version: String,
    pub host: String,
    pub cores_used: usize,
    pub elapsed_seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigInfo {
    pub decoder: Value, // opaque snapshot of the decoder config
    pub seed: u64,
    pub tiers_run: Vec<String>,
    /// Whether the eval invocation enabled the harness-side FP filter
    /// (`--fp-filter-baselines`, `--fp-filter-adif`, or `--fp-filter-rolling`).
    /// Recorded so cross-scorecard comparisons can detect methodology
    /// shifts (the filter is invisible to recall but moves `novel_decodes`).
    /// See `research/experiments/2026-05-31-hard-1000-novel-investigation.md`.
    #[serde(default)]
    pub fp_filter_active: bool,
}

/// Per-tier results. Sparse: only fields relevant to the tier are populated.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TierResult {
    pub wavs_processed: u32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub by_snr_db: Vec<SnrBin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_at_50pct_recovery_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_at_90pct_recovery_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub false_positives_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_passed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_failed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixtures_skipped: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<FixtureFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truth_decodes_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truth_decodes_recovered: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub novel_decodes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsjtx_decoded: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jtdx_decoded: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_wsjtx_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_jtdx_pct: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub per_wav_top_failures: Vec<PerWavFailure>,
    /// hb-129: Time-To-First-Decode (TTFD) distribution for this tier.
    /// `None` when the tier doesn't run the decoder (e.g., fixture-only
    /// tiers) or when no decode produced a stamped result.
    ///
    /// Computed per-WAV as `min(decode_time_into_window)` across all
    /// decodes in that WAV, aggregated as p50 / p90 / mean across WAVs.
    /// Sidecar metric — NOT part of the composite score (yet). See
    /// `research/ideation/2026-06-01-metric.md` M1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfd_distribution: Option<TtfdDistribution>,
}

/// hb-129: aggregate Time-To-First-Decode distribution across WAVs in a tier.
///
/// All durations expressed in seconds (f64). `per_wav_seconds` is sorted
/// so consumers can compute additional quantiles without re-sorting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TtfdDistribution {
    /// Number of WAVs that produced at least one stamped decode.
    pub wavs_with_decode: u32,
    /// Median (p50) TTFD across WAVs, in seconds.
    pub p50_seconds: f64,
    /// 90th percentile TTFD across WAVs, in seconds.
    pub p90_seconds: f64,
    /// Arithmetic mean TTFD across WAVs, in seconds.
    pub mean_seconds: f64,
    /// All per-WAV TTFD values (the min decode time for that WAV), sorted
    /// ascending. Length equals `wavs_with_decode`.
    pub per_wav_seconds: Vec<f64>,
}

impl TtfdDistribution {
    /// hb-129: Aggregate per-WAV TTFD values into a distribution.
    ///
    /// `per_wav_ttfd_s` is one entry per WAV that produced at least one
    /// stamped decode (the minimum `decode_time_into_window_s` for that
    /// WAV). Returns `None` if the input is empty.
    pub fn from_per_wav(mut per_wav_ttfd_s: Vec<f64>) -> Option<Self> {
        if per_wav_ttfd_s.is_empty() {
            return None;
        }
        per_wav_ttfd_s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = per_wav_ttfd_s.len();
        let mean = per_wav_ttfd_s.iter().sum::<f64>() / n as f64;
        // Inclusive percentile: index = round((p/100)*(n-1)), clamped.
        let pct = |p: f64| -> f64 {
            let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
            per_wav_ttfd_s[idx.min(n - 1)]
        };
        Some(Self {
            wavs_with_decode: n as u32,
            p50_seconds: pct(50.0),
            p90_seconds: pct(90.0),
            mean_seconds: mean,
            per_wav_seconds: per_wav_ttfd_s,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnrBin {
    pub snr_db: f64,
    pub attempts: u32,
    pub decoded: u32,
    pub fp: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureFailure {
    pub wav: String,
    pub expected: Vec<String>,
    pub got: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerWavFailure {
    pub wav_hash: String,
    pub truth: u32,
    pub recovered: u32,
    pub wsjtx: u32,
    pub jtdx: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompositeInfo {
    pub weights: BTreeMap<String, f64>,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_baseline_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_vs_main: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegressionFlags {
    pub fixture_regression: bool,
    pub false_positive_introduced: bool,
    pub snr_curve_regression_db: f64,
}

impl Scorecard {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        let pretty = serde_json::to_string_pretty(self)?;
        std::fs::write(path, pretty)?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let card: Scorecard = serde_json::from_str(&s)?;
        if card.schema_version != Self::CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "scorecard schema_version {} not supported (expected {})",
                card.schema_version,
                Self::CURRENT_SCHEMA_VERSION,
            );
        }
        Ok(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttfd_empty_returns_none() {
        assert!(TtfdDistribution::from_per_wav(vec![]).is_none());
    }

    #[test]
    fn ttfd_single_value() {
        let d = TtfdDistribution::from_per_wav(vec![3.5]).unwrap();
        assert_eq!(d.wavs_with_decode, 1);
        assert!((d.p50_seconds - 3.5).abs() < 1e-9);
        assert!((d.p90_seconds - 3.5).abs() < 1e-9);
        assert!((d.mean_seconds - 3.5).abs() < 1e-9);
    }

    #[test]
    fn ttfd_sorted_and_percentiles() {
        // 10 values 1.0..10.0 → p50 ≈ 6.0 (round(0.5*9)=5 → idx 5 → 6.0),
        // p90 ≈ 9.0 (round(0.9*9)=8 → idx 8 → 9.0).
        let input: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        let d = TtfdDistribution::from_per_wav(input).unwrap();
        assert_eq!(d.wavs_with_decode, 10);
        assert!(
            (d.p50_seconds - 6.0).abs() < 1e-9,
            "p50 = {}",
            d.p50_seconds
        );
        assert!(
            (d.p90_seconds - 9.0).abs() < 1e-9,
            "p90 = {}",
            d.p90_seconds
        );
        assert!((d.mean_seconds - 5.5).abs() < 1e-9);
        // per_wav_seconds sorted ascending.
        for w in d.per_wav_seconds.windows(2) {
            assert!(w[0] <= w[1]);
        }
    }

    #[test]
    fn ttfd_unsorted_input_sorted() {
        let d = TtfdDistribution::from_per_wav(vec![5.0, 1.0, 3.0, 2.0, 4.0]).unwrap();
        assert_eq!(d.per_wav_seconds, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!((d.p50_seconds - 3.0).abs() < 1e-9);
        assert!((d.mean_seconds - 3.0).abs() < 1e-9);
    }
}
