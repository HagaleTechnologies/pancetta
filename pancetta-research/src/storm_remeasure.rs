//! PAN-162: pre-registered re-measurement of hb-256's impulse-robust LLR
//! knee on stormier synthetic plants plus an off-air do-no-harm check.
//!
//! Everything that decides the verdict lives here (constants mirroring the
//! plan's pre-registration table, the stimulus, the statistic, the decision
//! rule and the scorecards with `check_invariants`). The
//! `batch104_impulse_llr_storm_remeasure` example is only the I/O and
//! decoding driver.
//!
//! Stimulus is identical to batch101 Part A: the 20 `MESSAGES` rotate by
//! `trial % 20`, each modulated at offset 0.0 and resized to
//! `WINDOW_SAMPLES`; σ comes from `sigma_for_snr_db` (2500 Hz reference);
//! the seed is `0xB101_0000 + si*1000 + trial` with
//! `si = round((snr_db + 24) * 2)`; base AWGN is drawn first, then bursts
//! from the same RNG, so the base noise is paired across plants and configs.

use crate::bootstrap_ci::{
    bootstrap_recall_delta, bootstrap_threshold_shift, threshold_50, DeltaCi, ThresholdShiftCi,
};
use crate::impulsiveness::slot_impulsiveness;
use crate::metrics::hash_normalize_message;
use crate::noise::{add_impulse_bursts, gaussian_noise, ImpulseBurstPlant};
use anyhow::{anyhow, bail, ensure, Context, Result};
use pancetta_ft8::{
    Ft8Config, Ft8Decoder as NativeFt8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES,
};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

// ---------------------------------------------------------------------------
// Pre-registration (mirrors the PAN-162 plan; never change after results)
// ---------------------------------------------------------------------------

/// Scorecard / shard schema version.
pub const SCHEMA_VERSION: u32 = 1;
/// Audio sample rate (Hz).
pub const SAMPLE_RATE: f32 = 12_000.0;

/// The 20 batch101 messages, verbatim.
pub const MESSAGES: [&str; 20] = [
    "CQ K1ABC FN42",
    "CQ W9XYZ EN50",
    "CQ N5DDD DM79",
    "CQ G4AAA IO91",
    "CQ JA1BBB PM95",
    "K1ABC W9XYZ EN50",
    "W9XYZ K1ABC -07",
    "K1ABC W9XYZ R-09",
    "W9XYZ K1ABC RR73",
    "K1ABC W9XYZ 73",
    "N5DDD G4AAA IO91",
    "G4AAA N5DDD -15",
    "N5DDD G4AAA R-18",
    "G4AAA N5DDD RR73",
    "CQ VK3CCC QF22",
    "VK3CCC JA1BBB PM95",
    "JA1BBB VK3CCC +03",
    "VK3CCC JA1BBB R+01",
    "JA1BBB VK3CCC RRR",
    "CQ PY2DDD GG66",
];

/// Plants in record order. `None` = pure AWGN.
pub const PLANTS: [(&str, Option<ImpulseBurstPlant>); 4] = [
    ("awgn", None),
    ("base", Some(ImpulseBurstPlant::BATCH101)),
    ("storm", Some(ImpulseBurstPlant::STORM)),
    ("severe", Some(ImpulseBurstPlant::SEVERE)),
];
/// Trials per SNR point, per plant.
pub const TRIALS: [(&str, usize); 4] = [
    ("awgn", 200),
    ("base", 100),
    ("storm", 200),
    ("severe", 100),
];
/// Pre-registered SNR grid per plant as inclusive `(lo, hi)` in dB, stepped by `GRID_STEP_DB`.
pub const GRIDS: [(&str, f64, f64); 4] = [
    ("awgn", -21.0, -16.0),
    ("base", -21.0, -12.0),
    ("storm", -21.0, -12.0),
    ("severe", -21.0, -12.0),
];
pub const GRID_STEP_DB: f64 = 0.5;
/// Extension limits (the extension rule can only add points within these).
pub const GRID_EXT_MAX_DB: f64 = -6.0;
pub const GRID_EXT_MIN_DB: f64 = -24.0;
/// Bar (a): awgn shift ≥ this.
pub const BAR_AWGN_DB: f64 = -0.05;
/// Bar (b): storm shift ≥ this (plus `ci_low > 0` and the undefined cap).
pub const BAR_STORM_DB: f64 = 0.30;
/// Bar (c′): off-air ΔTP ≥ this.
pub const BAR_OFFAIR_DTP: i64 = -2;
pub const MAX_UNDEFINED_RESAMPLES: usize = 50;
pub const BOOT_N: usize = 1000;
pub const BOOT_SEED: u64 = 0xb007;
/// Knees in decision order (k=6 first).
pub const K_ORDER: [f64; 2] = [6.0, 3.0];
/// Probe configs: (name, knee).
pub const CONFIGS: [(&str, Option<f64>); 3] = [("off", None), ("k3", Some(3.0)), ("k6", Some(6.0))];
pub const DECISION_PLANT: &str = "storm";
pub const AWGN_PLANT: &str = "awgn";
/// batch101's seed base.
pub const SEED_BASE: u64 = 0xB101_0000;

pub fn plant_by_name(name: &str) -> Result<Option<ImpulseBurstPlant>> {
    PLANTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| *p)
        .ok_or_else(|| anyhow!("unknown plant {name:?} (expected awgn|base|storm|severe)"))
}

pub fn trials_for(plant: &str) -> Result<usize> {
    TRIALS
        .iter()
        .find(|(n, _)| *n == plant)
        .map(|(_, t)| *t)
        .ok_or_else(|| anyhow!("unknown plant {plant:?}"))
}

pub fn config_knee(config: &str) -> Result<Option<f64>> {
    CONFIGS
        .iter()
        .find(|(n, _)| *n == config)
        .map(|(_, k)| *k)
        .ok_or_else(|| anyhow!("unknown config {config:?} (expected off|k3|k6)"))
}

/// Config name for a knee in `K_ORDER` (`6.0` → `"k6"`).
pub fn config_for_k(k: f64) -> Result<&'static str> {
    CONFIGS
        .iter()
        .find(|(_, kn)| *kn == Some(k))
        .map(|(n, _)| *n)
        .ok_or_else(|| anyhow!("no config for k={k}"))
}

/// Inclusive grid `lo..=hi` in `GRID_STEP_DB` steps (exact multiples of 0.5).
pub fn grid_points(lo: f64, hi: f64) -> Vec<f64> {
    let n = ((hi - lo) / GRID_STEP_DB).round() as i64;
    (0..=n.max(-1))
        .map(|i| lo + GRID_STEP_DB * i as f64)
        .collect()
}

/// The plant's pre-registered grid.
pub fn preregistered_grid(plant: &str) -> Result<Vec<f64>> {
    GRIDS
        .iter()
        .find(|(n, _, _)| *n == plant)
        .map(|(_, lo, hi)| grid_points(*lo, *hi))
        .ok_or_else(|| anyhow!("unknown plant {plant:?}"))
}

// ---------------------------------------------------------------------------
// Stimulus (bit-identical to batch101 Part A)
// ---------------------------------------------------------------------------

/// Encode + modulate each `MESSAGES` entry at offset 0.0, resized to `WINDOW_SAMPLES`.
pub fn clean_signals() -> Result<Vec<Vec<f32>>> {
    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default().map_err(|e| anyhow!("modulator: {e}"))?;
    let mut cleans = Vec::with_capacity(MESSAGES.len());
    for msg in MESSAGES {
        let symbols = encoder
            .encode_message(msg, None)
            .map_err(|e| anyhow!("encode '{msg}': {e}"))?;
        let mut clean = modulator
            .modulate_symbols(&symbols, 0.0)
            .map_err(|e| anyhow!("modulate '{msg}': {e}"))?;
        clean.resize(WINDOW_SAMPLES, 0.0);
        cleans.push(clean);
    }
    Ok(cleans)
}

pub fn signal_power(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// σ for AWGN such that SNR in the 2500 Hz reference bandwidth = `snr_db`.
pub fn sigma_for_snr_db(p_signal: f32, snr_db: f32) -> f32 {
    let bw: f32 = 2500.0;
    let p_n_in_bw = p_signal / 10.0f32.powf(snr_db / 10.0);
    let nyquist = SAMPLE_RATE / 2.0;
    (p_n_in_bw * nyquist / bw).sqrt()
}

/// batch101 seed: `0xB101_0000 + si*1000 + trial`, `si = round((snr_db + 24) * 2)`.
pub fn seed_for(snr_db: f64, trial: usize) -> u64 {
    let si = ((snr_db + 24.0) * 2.0).round();
    assert!(si >= 0.0, "seed_for: snr_db {snr_db} below -24 dB");
    SEED_BASE + (si as u64) * 1000 + trial as u64
}

/// Noise-only realization: base AWGN first, then (if a plant) bursts from the same RNG.
pub fn noise_realization(
    plant: Option<&ImpulseBurstPlant>,
    snr_db: f64,
    trial: usize,
    sigma: f32,
) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed_for(snr_db, trial));
    let mut noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
    if let Some(p) = plant {
        add_impulse_bursts(&mut rng, &mut noise, sigma, p);
    }
    noise
}

/// `(received, noise)` for one (SNR, trial); the message is `MESSAGES[trial % 20]`.
pub fn received_and_noise(
    plant: Option<&ImpulseBurstPlant>,
    snr_db: f64,
    trial: usize,
    cleans: &[Vec<f32>],
) -> (Vec<f32>, Vec<f32>) {
    let clean = &cleans[trial % MESSAGES.len()];
    // batch101 computed σ from an f32 SNR; multiples of 0.5 dB convert exactly.
    let sigma = sigma_for_snr_db(signal_power(clean), snr_db as f32);
    let noise = noise_realization(plant, snr_db, trial, sigma);
    let recv = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
    (recv, noise)
}

/// The received window for one (SNR, trial), bit-identical to batch101.
pub fn received_window(
    plant: Option<&ImpulseBurstPlant>,
    snr_db: f64,
    trial: usize,
    cleans: &[Vec<f32>],
) -> Vec<f32> {
    received_and_noise(plant, snr_db, trial, cleans).0
}

/// Impulsive-block fraction of a noise-only realization.
pub fn impulsive_block_fraction(noise: &[f32]) -> f64 {
    slot_impulsiveness(noise).impulsive_block_fraction
}

fn native_config(knee: Option<f64>) -> Ft8Config {
    Ft8Config {
        impulse_robust_llr: knee,
        ..Ft8Config::default()
    }
}

/// Decode `samples` with the native decoder (default config + `knee`), returning texts.
pub fn decode_texts(samples: &[f32], knee: Option<f64>) -> Result<Vec<String>> {
    let mut d =
        NativeFt8Decoder::new(native_config(knee)).map_err(|e| anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = d
        .decode_window(samples)
        .map_err(|e| anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|m| m.text).collect())
}

/// One success bit per `(window, msg_idx)`: some decode's text equals
/// `MESSAGES[msg_idx]`. A fresh decoder per window, so rayon and
/// sequential modes give identical results.
pub fn decode_success_bits(
    windows: &[(Vec<f32>, usize)],
    knee: Option<f64>,
    parallel: bool,
) -> Result<Vec<bool>> {
    let one = |(w, idx): &(Vec<f32>, usize)| -> Result<bool> {
        let want = MESSAGES.get(*idx).context("msg_idx out of range")?;
        Ok(decode_texts(w, knee)?.iter().any(|t| t == want))
    };
    if parallel {
        windows.par_iter().map(one).collect()
    } else {
        windows.iter().map(one).collect()
    }
}

// ---------------------------------------------------------------------------
// Grid extension rule
// ---------------------------------------------------------------------------

/// The grid the extension rule asks for, given rate curves (one per config,
/// each aligned with `grid`). Adds at most ONE 0.5 dB point at each end per
/// call, never alters existing points:
/// - top: if any curve's `threshold_50` is undefined (has not crossed 50% by
///   the top point) and the top is below `GRID_EXT_MAX_DB`, append top + 0.5;
/// - bottom: if any curve is already ≥ 50% at the bottom point and the bottom
///   is above `GRID_EXT_MIN_DB`, prepend bottom − 0.5.
///
/// Returns `grid` unchanged when no extension is needed. The summarizer
/// iterates: decode the new points, summarize again, until stable.
pub fn extended_grid(grid: &[f64], curves: &[Vec<f64>]) -> Vec<f64> {
    let mut out = grid.to_vec();
    let (Some(&lo), Some(&hi)) = (grid.first(), grid.last()) else {
        return out;
    };
    let up = curves.iter().any(|c| threshold_50(grid, c).is_none());
    if up && hi + GRID_STEP_DB <= GRID_EXT_MAX_DB + 1e-9 {
        out.push(hi + GRID_STEP_DB);
    }
    let down = curves.iter().any(|c| c.first().is_some_and(|&r| r >= 0.5));
    if down && lo - GRID_STEP_DB >= GRID_EXT_MIN_DB - 1e-9 {
        out.insert(0, lo - GRID_STEP_DB);
    }
    out
}

/// Validates a plant grid against the pre-registration and the extension
/// rule: contiguous 0.5 dB steps within the extension limits, containing the
/// pre-registered grid, every added point justified by the rule on the
/// points inside it, and no further extension pending.
pub fn check_grid(plant: &str, grid: &[f64], curves: &[Vec<f64>]) -> Result<()> {
    let pre = preregistered_grid(plant)?;
    ensure!(!grid.is_empty(), "{plant}: empty grid");
    let lo = grid[0];
    ensure!(
        grid == grid_points(lo, *grid.last().unwrap()).as_slice(),
        "{plant}: grid is not contiguous 0.5 dB steps: {grid:?}"
    );
    ensure!(
        lo >= GRID_EXT_MIN_DB && *grid.last().unwrap() <= GRID_EXT_MAX_DB,
        "{plant}: grid outside extension limits"
    );
    let start = grid
        .iter()
        .position(|&g| g == pre[0])
        .with_context(|| format!("{plant}: grid lacks pre-registered point {}", pre[0]))?;
    ensure!(
        grid.len() >= start + pre.len() && grid[start..start + pre.len()] == pre[..],
        "{plant}: grid does not contain the pre-registered grid {pre:?}"
    );
    for c in curves {
        ensure!(
            c.len() == grid.len(),
            "{plant}: curve length != grid length"
        );
    }
    let sub =
        |a: usize, b: usize| -> Vec<Vec<f64>> { curves.iter().map(|c| c[a..b].to_vec()).collect() };
    // Upward points beyond the pre-registered top.
    for end in start + pre.len()..grid.len() {
        let g = &grid[..end];
        ensure!(
            sub(0, end).iter().any(|c| threshold_50(g, c).is_none()),
            "{plant}: extension point {} dB not justified (every config crossed 50% below it)",
            grid[end]
        );
    }
    // Downward points below the pre-registered bottom.
    for first in (0..start).rev() {
        ensure!(
            curves.iter().any(|c| c[first + 1] >= 0.5),
            "{plant}: extension point {} dB not justified (no config ≥ 50% at {} dB)",
            grid[first],
            grid[first + 1]
        );
    }
    ensure!(
        extended_grid(grid, curves) == grid,
        "{plant}: the extension rule still asks for more points"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Off-air reference lists
// ---------------------------------------------------------------------------

/// Parses a vendored ft8_lib reference list (`HHMMSS SNR DT FREQ ~ MESSAGE`).
/// Returns the trimmed text after `~`; skips lines without `~` or with an
/// empty message. A trailing WSJT-X DXCC annotation separated from the
/// message by two or more spaces (`CQ TA6CQ KN70      AS Turkey`) is cut,
/// since an FT8 message never contains a double space.
pub fn parse_reference_txt(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once('~')?;
            let rest = rest.trim();
            let msg = rest.split("  ").next().unwrap_or("").trim();
            (!msg.is_empty()).then(|| msg.to_string())
        })
        .collect()
}

/// `(tp, fp)` over DISTINCT `hash_normalize_message` decoded texts: TP =
/// those in `truth` (already normalized), FP = the rest. So `tp ≤ |truth|`.
pub fn score_decodes<'a>(
    truth: &HashSet<String>,
    decoded: impl IntoIterator<Item = &'a str>,
) -> (usize, usize) {
    let distinct: HashSet<String> = decoded.into_iter().map(hash_normalize_message).collect();
    let tp = distinct.iter().filter(|t| truth.contains(*t)).count();
    (tp, distinct.len() - tp)
}

// ---------------------------------------------------------------------------
// Decision rule
// ---------------------------------------------------------------------------

/// PAN-162 verdict, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    #[serde(rename = "candidate-k6")]
    CandidateK6,
    #[serde(rename = "candidate-k3")]
    CandidateK3,
    #[serde(rename = "stays-off-harm-gate")]
    StaysOffHarmGate,
    #[serde(rename = "stays-off-inconclusive")]
    StaysOffInconclusive,
    #[serde(rename = "stays-off-below-bar")]
    StaysOffBelowBar,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::CandidateK6 => "candidate-k6",
            Verdict::CandidateK3 => "candidate-k3",
            Verdict::StaysOffHarmGate => "stays-off-harm-gate",
            Verdict::StaysOffInconclusive => "stays-off-inconclusive",
            Verdict::StaysOffBelowBar => "stays-off-below-bar",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decision inputs for one knee.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KneeEvidence {
    pub k: f64,
    /// `awgn` point shift (threshold(off) − threshold(k)); `None` if undefined.
    pub awgn_shift: Option<f64>,
    /// `storm` paired-bootstrap shift.
    pub storm: ThresholdShiftCi,
    /// Off-air ΔTP(k) = ΣTP(k) − ΣTP(off).
    pub offair_dtp: i64,
}

impl KneeEvidence {
    /// Bar (a).
    pub fn passes_awgn(&self) -> bool {
        self.awgn_shift.is_some_and(|s| s >= BAR_AWGN_DB)
    }
    /// Bar (b).
    pub fn passes_storm(&self) -> bool {
        self.storm.point.is_some_and(|p| p >= BAR_STORM_DB)
            && self.storm.ci_low.is_some_and(|l| l > 0.0)
            && self.storm.undefined_resamples <= MAX_UNDEFINED_RESAMPLES
    }
    /// Bar (c′).
    pub fn passes_offair(&self) -> bool {
        self.offair_dtp >= BAR_OFFAIR_DTP
    }
    fn storm_undefined(&self) -> bool {
        self.storm.point.is_none() || self.storm.undefined_resamples > MAX_UNDEFINED_RESAMPLES
    }
}

/// The pre-registered decision rule. Evaluates knees in `K_ORDER` (k=6
/// first); `evidence` must hold exactly one entry per `K_ORDER` knee.
pub fn decide(evidence: &[KneeEvidence]) -> Result<Verdict> {
    ensure!(
        evidence.len() == K_ORDER.len(),
        "decide: expected {} knees, got {}",
        K_ORDER.len(),
        evidence.len()
    );
    let mut ordered = Vec::with_capacity(K_ORDER.len());
    for k in K_ORDER {
        ordered.push(
            evidence
                .iter()
                .find(|e| e.k == k)
                .with_context(|| format!("decide: no evidence for k={k}"))?,
        );
    }
    for e in &ordered {
        if e.passes_awgn() && e.passes_storm() && e.passes_offair() {
            return Ok(if e.k == 6.0 {
                Verdict::CandidateK6
            } else {
                Verdict::CandidateK3
            });
        }
    }
    if ordered.iter().any(|e| e.passes_storm()) {
        return Ok(Verdict::StaysOffHarmGate);
    }
    if ordered.iter().any(|e| e.storm_undefined()) {
        return Ok(Verdict::StaysOffInconclusive);
    }
    Ok(Verdict::StaysOffBelowBar)
}

// ---------------------------------------------------------------------------
// Scorecards
// ---------------------------------------------------------------------------

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let txt =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&txt).with_context(|| format!("parsing {}", path.display()))
}

fn save_json<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn close_opt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => close(x, y),
        (None, None) => true,
        _ => false,
    }
}

fn close_shift(a: &ThresholdShiftCi, b: &ThresholdShiftCi) -> bool {
    close_opt(a.point, b.point)
        && close_opt(a.ci_low, b.ci_low)
        && close_opt(a.ci_high, b.ci_high)
        && a.significant == b.significant
        && a.n_bootstrap == b.n_bootstrap
        && a.undefined_resamples == b.undefined_resamples
}

fn close_delta(a: &DeltaCi, b: &DeltaCi) -> bool {
    close(a.mean, b.mean)
        && close(a.ci_low, b.ci_low)
        && close(a.ci_high, b.ci_high)
        && a.significant == b.significant
        && a.n_bootstrap == b.n_bootstrap
}

/// Per-WAV off-air counts (distinct normalized texts; see `score_decodes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffairWav {
    /// Repo-relative path (or the manifest's path in `--manifest` mode).
    pub path: String,
    pub sha256: String,
    /// Distinct normalized reference messages.
    pub truth: usize,
    pub tp_off: usize,
    pub fp_off: usize,
    pub tp_k3: usize,
    pub fp_k3: usize,
    pub tp_k6: usize,
    pub fp_k6: usize,
}

/// Off-air do-no-harm (c′) record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffairScorecard {
    pub schema_version: u32,
    pub label: String,
    pub wav_dir: String,
    pub boot_n: usize,
    pub boot_seed: u64,
    /// WAVs skipped for lack of truth (`--manifest` mode; 0 for `--offair`).
    #[serde(default)]
    pub missing_truth: usize,
    pub wavs: Vec<OffairWav>,
    pub delta_tp_k3: i64,
    pub delta_tp_k6: i64,
    /// Paired `bootstrap_recall_delta` CI on per-WAV TP (reported, not gated).
    pub ci_k3: DeltaCi,
    pub ci_k6: DeltaCi,
}

fn offair_derived(wavs: &[OffairWav]) -> (i64, i64, DeltaCi, DeltaCi) {
    let off: Vec<(u32, u32)> = wavs
        .iter()
        .map(|w| (w.tp_off as u32, w.truth as u32))
        .collect();
    let k3: Vec<(u32, u32)> = wavs
        .iter()
        .map(|w| (w.tp_k3 as u32, w.truth as u32))
        .collect();
    let k6: Vec<(u32, u32)> = wavs
        .iter()
        .map(|w| (w.tp_k6 as u32, w.truth as u32))
        .collect();
    let sum = |v: &[(u32, u32)]| v.iter().map(|x| x.0 as i64).sum::<i64>();
    (
        sum(&k3) - sum(&off),
        sum(&k6) - sum(&off),
        bootstrap_recall_delta(&off, &k3, BOOT_N, BOOT_SEED),
        bootstrap_recall_delta(&off, &k6, BOOT_N, BOOT_SEED),
    )
}

impl OffairScorecard {
    /// Builds the record, deriving ΔTP and the CIs from the per-WAV counts.
    pub fn build(label: &str, wav_dir: &str, wavs: Vec<OffairWav>, missing_truth: usize) -> Self {
        let (delta_tp_k3, delta_tp_k6, ci_k3, ci_k6) = offair_derived(&wavs);
        OffairScorecard {
            schema_version: SCHEMA_VERSION,
            label: label.to_string(),
            wav_dir: wav_dir.to_string(),
            boot_n: BOOT_N,
            boot_seed: BOOT_SEED,
            missing_truth,
            wavs,
            delta_tp_k3,
            delta_tp_k6,
            ci_k3,
            ci_k6,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        load_json(path.as_ref())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        save_json(self, path.as_ref())
    }

    /// Requires at least one WAV with reference messages, recomputes ΔTP and CIs,
    /// and checks constants and `tp ≤ truth`.
    pub fn check_invariants(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "offair: schema_version"
        );
        ensure!(
            self.boot_n == BOOT_N,
            "offair: boot_n {} != {BOOT_N}",
            self.boot_n
        );
        ensure!(
            self.boot_seed == BOOT_SEED,
            "offair: boot_seed differs from code"
        );
        ensure!(
            self.wavs.iter().any(|w| w.truth > 0),
            "offair: no WAVs with reference messages"
        );
        for w in &self.wavs {
            ensure!(
                w.sha256.len() == 64 && w.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "offair: {} has a malformed sha256",
                w.path
            );
            ensure!(
                w.tp_off <= w.truth && w.tp_k3 <= w.truth && w.tp_k6 <= w.truth,
                "offair: {} has tp > truth",
                w.path
            );
        }
        let (d3, d6, c3, c6) = offair_derived(&self.wavs);
        ensure!(
            d3 == self.delta_tp_k3,
            "offair: delta_tp_k3 {} != recomputed {d3}",
            self.delta_tp_k3
        );
        ensure!(
            d6 == self.delta_tp_k6,
            "offair: delta_tp_k6 {} != recomputed {d6}",
            self.delta_tp_k6
        );
        ensure!(
            close_delta(&c3, &self.ci_k3),
            "offair: ci_k3 != recomputed {c3:?}"
        );
        ensure!(
            close_delta(&c6, &self.ci_k6),
            "offair: ci_k6 != recomputed {c6:?}"
        );
        Ok(())
    }

    pub fn delta_tp(&self, k: f64) -> Result<i64> {
        match config_for_k(k)? {
            "k3" => Ok(self.delta_tp_k3),
            "k6" => Ok(self.delta_tp_k6),
            other => bail!("no off-air ΔTP for {other}"),
        }
    }
}

/// One per-(plant, config) decode shard written by batch104.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shard {
    pub schema_version: u32,
    pub plant: String,
    pub config: String,
    pub knee: Option<f64>,
    pub trials: usize,
    /// Smoke shards (`--smoke`) are refused by the summarizer.
    pub smoke: bool,
    pub grid: Vec<f64>,
    /// One '0'/'1' string per grid point, length == trials.
    pub outcomes: Vec<String>,
    /// Mean impulsive-block fraction of the noise-only realizations per point.
    pub ibf_by_point: Vec<f64>,
    /// Mean of `ibf_by_point` (equal trials per point).
    pub mean_impulsive_block_fraction: f64,
}

impl Shard {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        load_json(path.as_ref())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        save_json(self, path.as_ref())
    }

    /// `<shard_dir>/<plant>-<config>.json`.
    pub fn path_in(dir: &Path, plant: &str, config: &str) -> std::path::PathBuf {
        dir.join(format!("{plant}-{config}.json"))
    }

    pub fn rows(&self) -> Result<Vec<Vec<bool>>> {
        self.outcomes
            .iter()
            .map(|s| parse_bits(s, self.trials))
            .collect()
    }

    /// Structural checks (names/knee/trials vs code, lengths, sorted grid).
    /// `allow_smoke = false` refuses smoke shards.
    pub fn validate(&self, plant: &str, config: &str, allow_smoke: bool) -> Result<()> {
        let id = format!("shard {plant}-{config}");
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "{id}: schema_version"
        );
        ensure!(
            self.plant == plant && self.config == config,
            "{id}: names {}-{}",
            self.plant,
            self.config
        );
        ensure!(
            self.knee == config_knee(config)?,
            "{id}: knee differs from code"
        );
        plant_by_name(plant)?;
        ensure!(
            allow_smoke || !self.smoke,
            "{id}: is a --smoke shard; refusing to summarize it"
        );
        if !self.smoke {
            ensure!(
                self.trials == trials_for(plant)?,
                "{id}: trials {} != pre-registered",
                self.trials
            );
        }
        ensure!(
            self.outcomes.len() == self.grid.len(),
            "{id}: outcomes/grid length"
        );
        ensure!(
            self.ibf_by_point.len() == self.grid.len(),
            "{id}: ibf/grid length"
        );
        ensure!(
            self.grid.windows(2).all(|w| w[0] < w[1]),
            "{id}: grid not ascending"
        );
        self.rows().with_context(|| id.clone())?;
        let mean = mean(&self.ibf_by_point);
        ensure!(
            close(mean, self.mean_impulsive_block_fraction),
            "{id}: mean IBF mismatch"
        );
        Ok(())
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

pub fn bits_to_string(bits: &[bool]) -> String {
    bits.iter().map(|&b| if b { '1' } else { '0' }).collect()
}

pub fn parse_bits(s: &str, trials: usize) -> Result<Vec<bool>> {
    ensure!(
        s.len() == trials,
        "bitstring length {} != trials {trials}",
        s.len()
    );
    s.chars()
        .map(|c| match c {
            '0' => Ok(false),
            '1' => Ok(true),
            _ => Err(anyhow!("bitstring has non-0/1 character {c:?}")),
        })
        .collect()
}

pub fn rates_of(rows: &[Vec<bool>]) -> Vec<f64> {
    rows.iter()
        .map(|r| r.iter().filter(|&&b| b).count() as f64 / r.len().max(1) as f64)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bars {
    pub awgn_db: f64,
    pub storm_db: f64,
    pub offair_dtp: i64,
    pub max_undefined_resamples: usize,
}

impl Bars {
    pub const PREREGISTERED: Bars = Bars {
        awgn_db: BAR_AWGN_DB,
        storm_db: BAR_STORM_DB,
        offair_dtp: BAR_OFFAIR_DTP,
        max_undefined_resamples: MAX_UNDEFINED_RESAMPLES,
    };
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigRecord {
    pub name: String,
    pub knee: Option<f64>,
    /// One '0'/'1' string per SNR point, length == trials.
    pub outcomes: Vec<String>,
    pub rates: Vec<f64>,
    pub threshold_db: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShiftRecord {
    pub k: f64,
    pub ci: ThresholdShiftCi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlantRecord {
    pub name: String,
    pub plant: Option<ImpulseBurstPlant>,
    pub trials: usize,
    pub grid: Vec<f64>,
    pub mean_impulsive_block_fraction: f64,
    pub configs: Vec<ConfigRecord>,
    /// One per `K_ORDER` knee: off vs that knee.
    pub shifts: Vec<ShiftRecord>,
}

impl PlantRecord {
    pub fn shift(&self, k: f64) -> Option<&ThresholdShiftCi> {
        self.shifts.iter().find(|s| s.k == k).map(|s| &s.ci)
    }
}

/// Raw per-plant input to `build_storm_scorecard`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlantInput {
    pub name: String,
    pub grid: Vec<f64>,
    pub mean_impulsive_block_fraction: f64,
    /// `(config name, rows[point][trial])` for every `CONFIGS` entry.
    pub configs: Vec<(String, Vec<Vec<bool>>)>,
}

/// The PAN-162 storm re-measurement record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormRemeasureScorecard {
    pub schema_version: u32,
    pub label: String,
    pub commit: String,
    pub rayon_threads: usize,
    pub boot_n: usize,
    pub boot_seed: u64,
    pub bars: Bars,
    pub k_order: Vec<f64>,
    pub plants: Vec<PlantRecord>,
    pub offair_delta_tp_k3: i64,
    pub offair_delta_tp_k6: i64,
    pub verdict: Verdict,
}

fn shifts_for(grid: &[f64], configs: &[(String, Vec<Vec<bool>>)]) -> Result<Vec<ShiftRecord>> {
    let rows_of = |name: &str| -> Result<&Vec<Vec<bool>>> {
        configs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r)
            .with_context(|| format!("missing config {name}"))
    };
    let off = rows_of("off")?;
    K_ORDER
        .iter()
        .map(|&k| {
            let on = rows_of(config_for_k(k)?)?;
            Ok(ShiftRecord {
                k,
                ci: bootstrap_threshold_shift(grid, off, on, BOOT_N, BOOT_SEED),
            })
        })
        .collect()
}

/// Builds the scorecard from raw bit vectors: rates, thresholds, shifts,
/// the decision and constants are all derived here. `plants` must follow
/// `PLANTS` order, each with every `CONFIGS` entry in order.
pub fn build_storm_scorecard(
    label: &str,
    plants: Vec<PlantInput>,
    offair: &OffairScorecard,
    commit: &str,
    rayon_threads: usize,
) -> Result<StormRemeasureScorecard> {
    offair.check_invariants()?;
    ensure!(
        plants.len() == PLANTS.len(),
        "expected {} plants",
        PLANTS.len()
    );
    let mut records = Vec::with_capacity(plants.len());
    for (input, (pname, plant)) in plants.into_iter().zip(PLANTS) {
        ensure!(
            input.name == pname,
            "plant order: got {}, expected {pname}",
            input.name
        );
        let trials = trials_for(pname)?;
        ensure!(
            input.configs.len() == CONFIGS.len(),
            "{pname}: expected {} configs",
            CONFIGS.len()
        );
        let mut configs = Vec::new();
        for ((cname, rows), (want, knee)) in input.configs.iter().zip(CONFIGS) {
            ensure!(
                cname == want,
                "{pname}: config order: got {cname}, expected {want}"
            );
            ensure!(
                rows.len() == input.grid.len(),
                "{pname}-{cname}: rows/grid length"
            );
            ensure!(
                rows.iter().all(|r| r.len() == trials),
                "{pname}-{cname}: trials != {trials}"
            );
            let rates = rates_of(rows);
            configs.push(ConfigRecord {
                name: cname.clone(),
                knee,
                outcomes: rows.iter().map(|r| bits_to_string(r)).collect(),
                threshold_db: threshold_50(&input.grid, &rates),
                rates,
            });
        }
        let shifts = shifts_for(&input.grid, &input.configs)?;
        records.push(PlantRecord {
            name: pname.to_string(),
            plant,
            trials,
            grid: input.grid,
            mean_impulsive_block_fraction: input.mean_impulsive_block_fraction,
            configs,
            shifts,
        });
    }
    let mut card = StormRemeasureScorecard {
        schema_version: SCHEMA_VERSION,
        label: label.to_string(),
        commit: commit.to_string(),
        rayon_threads,
        boot_n: BOOT_N,
        boot_seed: BOOT_SEED,
        bars: Bars::PREREGISTERED,
        k_order: K_ORDER.to_vec(),
        plants: records,
        offair_delta_tp_k3: offair.delta_tp_k3,
        offair_delta_tp_k6: offair.delta_tp_k6,
        verdict: Verdict::StaysOffBelowBar,
    };
    card.verdict = decide(&card.evidence()?)?;
    Ok(card)
}

impl StormRemeasureScorecard {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        load_json(path.as_ref())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        save_json(self, path.as_ref())
    }

    pub fn plant(&self, name: &str) -> Result<&PlantRecord> {
        self.plants
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("scorecard has no plant {name}"))
    }

    pub fn offair_delta_tp(&self, k: f64) -> Result<i64> {
        match config_for_k(k)? {
            "k3" => Ok(self.offair_delta_tp_k3),
            "k6" => Ok(self.offair_delta_tp_k6),
            other => bail!("no off-air ΔTP for {other}"),
        }
    }

    /// Decision inputs per `K_ORDER` knee, from the stored records.
    pub fn evidence(&self) -> Result<Vec<KneeEvidence>> {
        let awgn = self.plant(AWGN_PLANT)?;
        let storm = self.plant(DECISION_PLANT)?;
        K_ORDER
            .iter()
            .map(|&k| {
                Ok(KneeEvidence {
                    k,
                    awgn_shift: awgn.shift(k).context("awgn shift")?.point,
                    storm: *storm.shift(k).context("storm shift")?,
                    offair_dtp: self.offair_delta_tp(k)?,
                })
            })
            .collect()
    }

    /// Recomputes everything derivable from the stored bitstrings and checks
    /// the stored values and constants against code.
    pub fn check_invariants(&self) -> Result<()> {
        ensure!(self.schema_version == SCHEMA_VERSION, "schema_version");
        ensure!(self.boot_n == BOOT_N, "boot_n {} != {BOOT_N}", self.boot_n);
        ensure!(self.boot_seed == BOOT_SEED, "boot_seed differs from code");
        ensure!(
            self.bars == Bars::PREREGISTERED,
            "bars differ from code: {:?}",
            self.bars
        );
        ensure!(
            self.k_order == K_ORDER,
            "k_order differs from code: {:?}",
            self.k_order
        );
        ensure!(
            self.plants.len() == PLANTS.len(),
            "plant count differs from code"
        );
        for (rec, (pname, plant)) in self.plants.iter().zip(PLANTS) {
            ensure!(rec.name == pname, "plant {} != {pname}", rec.name);
            ensure!(
                rec.plant == plant,
                "{pname}: plant parameters differ from code"
            );
            ensure!(
                rec.trials == trials_for(pname)?,
                "{pname}: trials differ from code"
            );
            ensure!(
                rec.mean_impulsive_block_fraction.is_finite()
                    && (0.0..=1.0).contains(&rec.mean_impulsive_block_fraction),
                "{pname}: mean IBF out of range"
            );
            ensure!(rec.configs.len() == CONFIGS.len(), "{pname}: config count");
            let mut raw = Vec::new();
            let mut curves = Vec::new();
            for (c, (cname, knee)) in rec.configs.iter().zip(CONFIGS) {
                let id = format!("{pname}-{cname}");
                ensure!(
                    c.name == cname && c.knee == knee,
                    "{id}: config name/knee differ from code"
                );
                ensure!(
                    c.outcomes.len() == rec.grid.len(),
                    "{id}: outcomes/grid length"
                );
                let rows = c
                    .outcomes
                    .iter()
                    .map(|s| parse_bits(s, rec.trials))
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| id.clone())?;
                let rates = rates_of(&rows);
                ensure!(
                    rates.len() == c.rates.len()
                        && rates.iter().zip(&c.rates).all(|(a, b)| close(*a, *b)),
                    "{id}: stored rates differ from bitstrings"
                );
                ensure!(
                    close_opt(threshold_50(&rec.grid, &rates), c.threshold_db),
                    "{id}: stored threshold differs from recomputed"
                );
                curves.push(rates);
                raw.push((cname.to_string(), rows));
            }
            check_grid(pname, &rec.grid, &curves)?;
            let shifts = shifts_for(&rec.grid, &raw)?;
            ensure!(rec.shifts.len() == shifts.len(), "{pname}: shift count");
            for (a, b) in rec.shifts.iter().zip(&shifts) {
                ensure!(
                    a.k == b.k && close_shift(&a.ci, &b.ci),
                    "{pname}: stored k={} shift differs from recomputed {:?}",
                    a.k,
                    b.ci
                );
            }
        }
        let verdict = decide(&self.evidence()?)?;
        ensure!(
            verdict == self.verdict,
            "verdict {} != recomputed {verdict}",
            self.verdict
        );
        Ok(())
    }

    /// One-line verdict summary for the experiment doc.
    pub fn verdict_line(&self) -> String {
        let fmt_ci = |ci: Option<&ThresholdShiftCi>| match ci {
            Some(c) => {
                let f = |x: Option<f64>| x.map_or("undefined".to_string(), |v| format!("{v:+.3}"));
                format!(
                    "{} dB [{}, {}]{}",
                    f(c.point),
                    f(c.ci_low),
                    f(c.ci_high),
                    if c.undefined_resamples > 0 {
                        format!(" ({} undefined)", c.undefined_resamples)
                    } else {
                        String::new()
                    }
                )
            }
            None => "missing".to_string(),
        };
        let pt = |ci: Option<&ThresholdShiftCi>| {
            ci.and_then(|c| c.point)
                .map_or("undefined".to_string(), |v| format!("{v:+.3} dB"))
        };
        let storm = self.plant(DECISION_PLANT).ok();
        let awgn = self.plant(AWGN_PLANT).ok();
        let parts: Vec<String> = K_ORDER
            .iter()
            .map(|&k| format!("k={k} {}", fmt_ci(storm.and_then(|p| p.shift(k)))))
            .collect();
        let awgn_parts: Vec<String> = K_ORDER
            .iter()
            .map(|&k| format!("k={k} {}", pt(awgn.and_then(|p| p.shift(k)))))
            .collect();
        let dtp: Vec<String> = K_ORDER
            .iter()
            .map(|&k| match self.offair_delta_tp(k) {
                Ok(d) => format!("k={k} {d:+}"),
                Err(_) => format!("k={k} missing"),
            })
            .collect();
        format!(
            "PAN-162 verdict: {} — storm {}; awgn {}; off-air ΔTP {}",
            self.verdict,
            parts.join(", "),
            awgn_parts.join(", "),
            dtp.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngExt;

    // ---- constants -------------------------------------------------------

    #[test]
    fn constants_match_preregistration() {
        assert_eq!(MESSAGES.len(), 20);
        assert_eq!(MESSAGES[0], "CQ K1ABC FN42");
        assert_eq!(MESSAGES[19], "CQ PY2DDD GG66");
        let names: Vec<&str> = PLANTS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["awgn", "base", "storm", "severe"]);
        assert_eq!(PLANTS[0].1, None);
        assert_eq!(PLANTS[1].1, Some(ImpulseBurstPlant::BATCH101));
        assert_eq!(PLANTS[2].1, Some(ImpulseBurstPlant::STORM));
        assert_eq!(PLANTS[3].1, Some(ImpulseBurstPlant::SEVERE));
        assert_eq!(ImpulseBurstPlant::BATCH101.burst_start_prob, 0.02);
        assert_eq!(ImpulseBurstPlant::STORM.burst_start_prob, 0.05);
        assert_eq!(ImpulseBurstPlant::SEVERE.burst_start_prob, 0.10);
        for p in [
            ImpulseBurstPlant::BATCH101,
            ImpulseBurstPlant::STORM,
            ImpulseBurstPlant::SEVERE,
        ] {
            assert_eq!(p.max_duration_symbols, 3);
            assert_eq!((p.amp_db_min, p.amp_db_max), (10.0, 30.0));
        }
        assert_eq!(
            TRIALS,
            [
                ("awgn", 200),
                ("base", 100),
                ("storm", 200),
                ("severe", 100)
            ]
        );
        let awgn = preregistered_grid("awgn").unwrap();
        assert_eq!(awgn.len(), 11);
        assert_eq!((awgn[0], awgn[10]), (-21.0, -16.0));
        for p in ["base", "storm", "severe"] {
            let g = preregistered_grid(p).unwrap();
            assert_eq!(g.len(), 19, "{p}");
            assert_eq!((g[0], g[18]), (-21.0, -12.0));
            assert!(g.windows(2).all(|w| w[1] - w[0] == 0.5));
        }
        assert_eq!(BAR_AWGN_DB, -0.05);
        assert_eq!(BAR_STORM_DB, 0.30);
        assert_eq!(BAR_OFFAIR_DTP, -2);
        assert_eq!(MAX_UNDEFINED_RESAMPLES, 50);
        assert_eq!(BOOT_N, 1000);
        assert_eq!(BOOT_SEED, 0xb007);
        assert_eq!(K_ORDER, [6.0, 3.0]);
        assert_eq!(
            CONFIGS,
            [("off", None), ("k3", Some(3.0)), ("k6", Some(6.0))]
        );
        assert_eq!((GRID_EXT_MIN_DB, GRID_EXT_MAX_DB), (-24.0, -6.0));
        assert_eq!(DECISION_PLANT, "storm");
        assert_eq!(seed_for(-18.0, 7), 0xB101_0000 + 12 * 1000 + 7);
        assert_eq!(seed_for(-24.0, 0), 0xB101_0000);
    }

    // ---- stimulus identity -----------------------------------------------

    /// Frozen copy of batch101's private plant (p = 0.02 hard-coded).
    fn batch101_add_impulse_bursts(rng: &mut StdRng, noise: &mut [f32], sigma_base: f32) {
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

    /// Frozen copy of batch101 Part A's inline generation at grid index `k`.
    fn batch101_window(impulsive: bool, k: usize, trial: usize, cleans: &[Vec<f32>]) -> Vec<f32> {
        let snr_db: f32 = -24.0 + 0.5 * k as f32;
        let si = k;
        let msg_idx = trial % MESSAGES.len();
        let clean = &cleans[msg_idx];
        let p_signal = signal_power(clean);
        let sigma = sigma_for_snr_db(p_signal, snr_db);
        let seed = 0xB101_0000u64 + (si as u64) * 1000 + trial as u64;
        let mut rng = StdRng::seed_from_u64(seed);
        let mut noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
        if impulsive {
            batch101_add_impulse_bursts(&mut rng, &mut noise, sigma);
        }
        clean.iter().zip(&noise).map(|(s, n)| s + n).collect()
    }

    #[test]
    fn stimulus_matches_batch101_bit_for_bit() {
        let cleans = clean_signals().unwrap();
        for (snr, k, trial) in [(-18.0f64, 12usize, 7usize), (-17.5, 13, 33)] {
            for (plant, impulsive) in [(None, false), (Some(&ImpulseBurstPlant::BATCH101), true)] {
                let ours = received_window(plant, snr, trial, &cleans);
                let oracle = batch101_window(impulsive, k, trial, &cleans);
                assert_eq!(ours.len(), WINDOW_SAMPLES);
                assert!(
                    ours.iter()
                        .zip(&oracle)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "stimulus differs at snr {snr} trial {trial} impulsive {impulsive}"
                );
            }
        }
        // The burst plant really adds energy over the paired base AWGN.
        let a = received_window(None, -18.0, 7, &cleans);
        let b = received_window(Some(&ImpulseBurstPlant::SEVERE), -18.0, 7, &cleans);
        assert!(signal_power(&b) > signal_power(&a));
    }

    // ---- decode determinism ----------------------------------------------

    #[test]
    fn decode_success_bits_parallel_equals_sequential() {
        let cleans = clean_signals().unwrap();
        let windows: Vec<(Vec<f32>, usize)> = (0..2)
            .map(|t| (received_window(None, -16.0, t, &cleans), t % MESSAGES.len()))
            .collect();
        let par = decode_success_bits(&windows, Some(6.0), true).unwrap();
        let seq = decode_success_bits(&windows, Some(6.0), false).unwrap();
        assert_eq!(par, seq);
        assert_eq!(par.len(), 2);
    }

    // ---- grid extension --------------------------------------------------

    #[test]
    fn extended_grid_rule() {
        let g = grid_points(-21.0, -19.0); // 5 points
        let crossed = vec![0.0, 0.2, 0.6, 0.9, 1.0];
        assert_eq!(extended_grid(&g, &[crossed.clone(), crossed.clone()]), g);
        // One config never crosses → one more point above, existing points untouched.
        let never = vec![0.0, 0.1, 0.2, 0.3, 0.4];
        let up = extended_grid(&g, &[crossed.clone(), never.clone()]);
        assert_eq!(up[..g.len()], g[..]);
        assert_eq!(up, grid_points(-21.0, -18.5));
        // A config already ≥ 50% at the bottom → one more point below.
        let high = vec![0.5, 0.7, 0.9, 1.0, 1.0];
        let down = extended_grid(&g, &[crossed.clone(), high.clone()]);
        assert_eq!(down[1..], g[..]);
        assert_eq!(down, grid_points(-21.5, -19.0));
        // Both ends at once.
        assert_eq!(
            extended_grid(&g, &[never.clone(), high]),
            grid_points(-21.5, -18.5)
        );
        // Limits: never above -6 or below -24.
        let top = grid_points(-8.0, -6.0);
        assert_eq!(extended_grid(&top, &[vec![0.0; 5]]), top);
        let bottom = grid_points(-24.0, -22.0);
        assert_eq!(extended_grid(&bottom, &[vec![1.0; 5]]), bottom);
    }

    #[test]
    fn check_grid_accepts_preregistered_and_justified_extension() {
        let pre = preregistered_grid("awgn").unwrap();
        let crossing: Vec<f64> = (0..pre.len()).map(|i| i as f64 / 10.0).collect();
        check_grid("awgn", &pre, std::slice::from_ref(&crossing)).unwrap();
        // Pending extension → error.
        assert!(check_grid("awgn", &pre, &[vec![0.0; pre.len()]]).is_err());
        // Justified one-point upward extension.
        let ext = grid_points(-21.0, -15.5);
        let mut never_then_cross = vec![0.0; pre.len()];
        never_then_cross.push(0.6);
        check_grid("awgn", &ext, &[never_then_cross]).unwrap();
        // Unjustified extension (already crossed below) → error.
        let mut c = crossing.clone();
        c.push(1.0);
        assert!(check_grid("awgn", &ext, &[c]).is_err());
        // Missing pre-registered points → error.
        assert!(check_grid("awgn", &pre[1..], &[crossing[1..].to_vec()]).is_err());
    }

    // ---- reference parser ------------------------------------------------

    #[test]
    fn parse_reference_txt_reads_vendored_format() {
        let txt = "110130  -6  0.7  683 ~  CQ TA6CQ KN70      AS Turkey\n\
                   110130 -16  1.0  989 ~  OH3NIV ZS6S -03\n\
                   000000  13  0.9  708 ~  CQ IK4LZH JN54\n\
                   no tilde here\n\
                   110130 -16  1.0  989 ~   \n\
                   \n\
                   110145  -2  0.3 1500 ~  K1ABC <...12345> RR73\n";
        let msgs = parse_reference_txt(txt);
        assert_eq!(
            msgs,
            [
                "CQ TA6CQ KN70",
                "OH3NIV ZS6S -03",
                "CQ IK4LZH JN54",
                "K1ABC <...12345> RR73"
            ]
        );
        // hash_normalize-consistent: a normalized parsed message is a fixed
        // point and equals the normalized decoder text.
        for m in &msgs {
            let n = hash_normalize_message(m);
            assert_eq!(hash_normalize_message(&n), n);
        }
        assert_eq!(
            hash_normalize_message(&msgs[3]),
            hash_normalize_message("K1ABC <...67890> RR73")
        );
        let truth: HashSet<String> = msgs.iter().map(|m| hash_normalize_message(m)).collect();
        let (tp, fp) = score_decodes(
            &truth,
            [
                "CQ TA6CQ KN70",
                "CQ TA6CQ KN70",
                "K1ABC <...999> RR73",
                "CQ N0NE AA00",
            ],
        );
        assert_eq!((tp, fp), (2, 1));
    }

    #[test]
    fn parse_reference_txt_on_vendored_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pancetta-ft8/vendor/ft8_lib/test/wav/191111_110130.txt");
        let Ok(txt) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {} not present", path.display());
            return;
        };
        let msgs = parse_reference_txt(&txt);
        assert_eq!(msgs.len(), txt.lines().filter(|l| l.contains('~')).count());
        assert!(msgs.iter().all(|m| !m.contains("  ") && m == m.trim()));
    }

    // ---- decision rule ---------------------------------------------------

    fn ci(point: Option<f64>, lo: Option<f64>, undefined: usize) -> ThresholdShiftCi {
        ThresholdShiftCi {
            point,
            ci_low: lo,
            ci_high: lo.map(|l| l + 0.5),
            significant: lo.is_some_and(|l| l > 0.0),
            n_bootstrap: BOOT_N,
            undefined_resamples: undefined,
        }
    }

    fn ev(k: f64, awgn: Option<f64>, storm: ThresholdShiftCi, dtp: i64) -> KneeEvidence {
        KneeEvidence {
            k,
            awgn_shift: awgn,
            storm,
            offair_dtp: dtp,
        }
    }

    fn pass_storm() -> ThresholdShiftCi {
        ci(Some(0.4), Some(0.1), 0)
    }
    fn fail_storm() -> ThresholdShiftCi {
        ci(Some(0.1), Some(-0.2), 0)
    }

    #[test]
    fn decide_every_branch() {
        // k6 preferred over k3 when both pass.
        let both = [
            ev(6.0, Some(0.0), pass_storm(), 0),
            ev(3.0, Some(0.0), pass_storm(), 0),
        ];
        assert_eq!(decide(&both).unwrap(), Verdict::CandidateK6);
        // Order of the input slice does not matter.
        let rev = [both[1], both[0]];
        assert_eq!(decide(&rev).unwrap(), Verdict::CandidateK6);
        // k6 fails (a); k3 passes all → candidate-k3.
        let k3 = [
            ev(6.0, Some(-0.06), pass_storm(), 0),
            ev(3.0, Some(-0.05), pass_storm(), 0),
        ];
        assert_eq!(decide(&k3).unwrap(), Verdict::CandidateK3);
        // Passed (b) but failed (a) → harm gate.
        let harm_a = [
            ev(6.0, Some(-0.2), pass_storm(), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&harm_a).unwrap(), Verdict::StaysOffHarmGate);
        // Undefined awgn shift fails (a).
        let harm_a2 = [
            ev(6.0, None, pass_storm(), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&harm_a2).unwrap(), Verdict::StaysOffHarmGate);
        // Passed (b) but failed (c′) → harm gate.
        let harm_c = [
            ev(6.0, Some(0.0), fail_storm(), 0),
            ev(3.0, Some(0.0), pass_storm(), -3),
        ];
        assert_eq!(decide(&harm_c).unwrap(), Verdict::StaysOffHarmGate);
        // No (b) pass; an undefined storm point → inconclusive.
        let inc = [
            ev(6.0, Some(0.0), ci(None, Some(0.1), 0), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&inc).unwrap(), Verdict::StaysOffInconclusive);
        // Everything else → below bar.
        let below = [
            ev(6.0, Some(0.0), fail_storm(), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&below).unwrap(), Verdict::StaysOffBelowBar);
        // Point passes but CI touches zero → (b) fails.
        let ci0 = [
            ev(6.0, Some(0.0), ci(Some(0.5), Some(0.0), 0), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&ci0).unwrap(), Verdict::StaysOffBelowBar);
        // Point just below the storm bar.
        let pt = [
            ev(6.0, Some(0.0), ci(Some(0.29), Some(0.1), 0), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&pt).unwrap(), Verdict::StaysOffBelowBar);
        // Missing knee → error.
        assert!(decide(&both[..1]).is_err());
        assert!(decide(&[both[0], both[0]]).is_err());
    }

    #[test]
    fn decide_undefined_resample_cap() {
        let at_cap = [
            ev(6.0, Some(0.0), ci(Some(0.4), Some(0.1), 50), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&at_cap).unwrap(), Verdict::CandidateK6);
        let over = [
            ev(6.0, Some(0.0), ci(Some(0.4), Some(0.1), 51), 0),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&over).unwrap(), Verdict::StaysOffInconclusive);
    }

    #[test]
    fn decide_offair_boundary() {
        let m2 = [
            ev(6.0, Some(0.0), pass_storm(), -2),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&m2).unwrap(), Verdict::CandidateK6);
        let m3 = [
            ev(6.0, Some(0.0), pass_storm(), -3),
            ev(3.0, Some(0.0), fail_storm(), 0),
        ];
        assert_eq!(decide(&m3).unwrap(), Verdict::StaysOffHarmGate);
    }

    #[test]
    fn verdict_serde_strings() {
        for (v, s) in [
            (Verdict::CandidateK6, "candidate-k6"),
            (Verdict::CandidateK3, "candidate-k3"),
            (Verdict::StaysOffHarmGate, "stays-off-harm-gate"),
            (Verdict::StaysOffInconclusive, "stays-off-inconclusive"),
            (Verdict::StaysOffBelowBar, "stays-off-below-bar"),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{s}\""));
            assert_eq!(
                serde_json::from_str::<Verdict>(&format!("\"{s}\"")).unwrap(),
                v
            );
            assert_eq!(v.as_str(), s);
        }
    }

    // ---- scorecards ------------------------------------------------------

    fn offair_fixture() -> OffairScorecard {
        let wavs = (0..6)
            .map(|i| OffairWav {
                path: format!("pancetta-ft8/vendor/ft8_lib/test/wav/{i}.wav"),
                sha256: format!("{:064x}", i),
                truth: 20 + i,
                tp_off: 15 + i,
                fp_off: 1,
                tp_k3: 14 + i,
                fp_k3: 1,
                tp_k6: 15 + i,
                fp_k6: 0,
            })
            .collect();
        OffairScorecard::build(
            "pan162-offair-test",
            "pancetta-ft8/vendor/ft8_lib/test/wav",
            wavs,
            0,
        )
    }

    /// Deterministic logistic-ish rows: `count(point)` successes per point.
    fn rows_for(grid: &[f64], trials: usize, center: f64) -> Vec<Vec<bool>> {
        grid.iter()
            .map(|&s| {
                let p = 1.0 / (1.0 + (-(s - center) * 2.5).exp());
                let c = (p * trials as f64).round() as usize;
                (0..trials)
                    .map(|t| (t * 37 + (s * 2.0) as usize) % trials < c)
                    .collect()
            })
            .collect()
    }

    fn storm_fixture() -> StormRemeasureScorecard {
        let plants = PLANTS
            .iter()
            .map(|(name, _)| {
                let grid = preregistered_grid(name).unwrap();
                let trials = trials_for(name).unwrap();
                let base = if *name == "awgn" { -18.7 } else { -18.0 };
                PlantInput {
                    name: name.to_string(),
                    grid: grid.clone(),
                    mean_impulsive_block_fraction: 0.05,
                    configs: CONFIGS
                        .iter()
                        .map(|(c, knee)| {
                            let gain = knee.map_or(0.0, |k| if k == 6.0 { 0.1 } else { 0.05 });
                            (c.to_string(), rows_for(&grid, trials, base - gain))
                        })
                        .collect(),
                }
            })
            .collect();
        build_storm_scorecard(
            "pan162-storm-test",
            plants,
            &offair_fixture(),
            "deadbeef",
            2,
        )
        .unwrap()
    }

    #[test]
    fn offair_scorecard_requires_reference_evidence() {
        let mut no_truth = offair_fixture().wavs;
        for w in &mut no_truth {
            w.truth = 0;
            w.tp_off = 0;
            w.tp_k3 = 0;
            w.tp_k6 = 0;
        }
        for (wavs, missing_truth) in [(vec![], 0), (vec![], 3), (no_truth, 0)] {
            let card = OffairScorecard::build("no-evidence", "manifest", wavs, missing_truth);
            let err = card.check_invariants().unwrap_err();
            assert!(err.to_string().contains("no WAVs with reference messages"));
            let err = build_storm_scorecard("test", vec![], &card, "deadbeef", 1).unwrap_err();
            assert!(err.to_string().contains("no WAVs with reference messages"));
        }
        // Missing truth for other WAVs does not invalidate usable evidence.
        let card = OffairScorecard::build("partial", "manifest", offair_fixture().wavs, 3);
        card.check_invariants().unwrap();
    }

    #[test]
    fn offair_scorecard_round_trip_and_invariants() {
        let card = offair_fixture();
        assert_eq!(card.delta_tp_k3, -6);
        assert_eq!(card.delta_tp_k6, 0);
        card.check_invariants().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("offair.json");
        card.save(&p).unwrap();
        let back = OffairScorecard::load(&p).unwrap();
        back.check_invariants().unwrap();
        assert_eq!(back.wavs, card.wavs);
        assert_eq!(back.delta_tp_k6, card.delta_tp_k6);

        let mut bad = card.clone();
        bad.delta_tp_k6 = 1;
        assert!(bad.check_invariants().is_err());
        let mut bad = card.clone();
        bad.wavs[0].tp_k6 += 1;
        assert!(bad.check_invariants().is_err());
        let mut bad = card.clone();
        bad.wavs[0].tp_off = bad.wavs[0].truth + 1;
        assert!(bad.check_invariants().is_err());
        let mut bad = card;
        bad.boot_seed = 1;
        assert!(bad.check_invariants().is_err());
    }

    #[test]
    fn storm_scorecard_round_trip_and_invariants() {
        let card = storm_fixture();
        card.check_invariants().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("storm.json");
        card.save(&p).unwrap();
        let back = StormRemeasureScorecard::load(&p).unwrap();
        back.check_invariants().unwrap();
        assert_eq!(back.verdict, card.verdict);
        assert_eq!(back.plants.len(), 4);
        let line = back.verdict_line();
        assert!(
            line.starts_with(&format!("PAN-162 verdict: {} — storm k=6 ", back.verdict)),
            "{line}"
        );
        assert!(line.contains("off-air ΔTP k=6 +0, k=3 -6"), "{line}");

        // Tampering with any derived value or constant is caught.
        let mut bad = back.clone();
        bad.plants[2].configs[0].rates[5] += 0.01;
        assert!(bad.check_invariants().is_err());
        let mut bad = back.clone();
        bad.verdict = if back.verdict == Verdict::CandidateK6 {
            Verdict::StaysOffBelowBar
        } else {
            Verdict::CandidateK6
        };
        assert!(bad.check_invariants().is_err());
        let mut bad = back.clone();
        bad.plants[2].configs[2].outcomes[3].pop();
        assert!(bad.check_invariants().is_err());
        let mut bad = back.clone();
        bad.plants[2].shifts[0].ci.undefined_resamples += 1;
        assert!(bad.check_invariants().is_err());
        let mut bad = back.clone();
        bad.bars.storm_db = 0.2;
        assert!(bad.check_invariants().is_err());
        let mut bad = back.clone();
        bad.plants[1].trials = 50;
        assert!(bad.check_invariants().is_err());
        let mut bad = back;
        bad.k_order = vec![3.0, 6.0];
        assert!(bad.check_invariants().is_err());
    }

    #[test]
    fn shard_round_trip_and_validation() {
        let grid = preregistered_grid("storm").unwrap();
        let rows = rows_for(&grid, 4, -18.0);
        let shard = Shard {
            schema_version: SCHEMA_VERSION,
            plant: "storm".into(),
            config: "k6".into(),
            knee: Some(6.0),
            trials: 4,
            smoke: true,
            grid: grid.clone(),
            outcomes: rows.iter().map(|r| bits_to_string(r)).collect(),
            ibf_by_point: vec![0.1; grid.len()],
            mean_impulsive_block_fraction: 0.1,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = Shard::path_in(dir.path(), "storm", "k6");
        shard.save(&p).unwrap();
        let back = Shard::load(&p).unwrap();
        assert_eq!(back, shard);
        assert_eq!(back.rows().unwrap(), rows);
        back.validate("storm", "k6", true).unwrap();
        assert!(
            back.validate("storm", "k6", false).is_err(),
            "smoke refused"
        );
        assert!(back.validate("storm", "k3", true).is_err());
    }
}
