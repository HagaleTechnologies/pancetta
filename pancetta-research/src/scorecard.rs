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
