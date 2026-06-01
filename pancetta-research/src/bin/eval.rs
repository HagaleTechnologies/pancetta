//! eval — runs a DecoderUnderTest against requested corpus tiers and emits a
//! scorecard. Tiers: fixtures (truth-validated), synth-clean (sensitivity
//! curve), curated-hard-200 / curated-hard-1000 / wild-50 (real-world WAVs
//! vs cached jt9 baseline).

use anyhow::Context;
use chrono::Utc;
use pancetta_research::corpus::{load_ft8_fixtures, load_synth_corpus, load_synth_pair_corpus};
use pancetta_research::curated::{load_curated_corpus, CuratedEntry};
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::metrics::{
    default_weights, populate_composite, saturation_aware_composite, RefreshOffsetRegistry,
};
use pancetta_research::scorecard::{
    BuildInfo, ConfigInfo, GitInfo, HarnessInfo, PerWavFailure, PerWavRecord, RegressionFlags,
    Scorecard, SnrBin, TierResult, TtfdDistribution,
};
use pancetta_research::truth::{FixtureCategory, FixtureTruth};
use pancetta_research::Mode;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug)]
struct Args {
    tiers: Vec<String>,
    mode: Mode,
    output: PathBuf,
    seed: u64,
    max_passes: Option<usize>,
    max_sync_candidates: Option<usize>,
    max_candidates: Option<usize>,
    /// `Some(None)` means "explicitly disable OSD". `Some(Some(d))` means
    /// "set depth to d". `None` means "no override; use the production
    /// default."
    osd_depth: Option<Option<u8>>,
    ldpc_iterations: Option<usize>,
    llr_target_variance: Option<f32>,
    nms_enabled: Option<bool>,
    nms_time_radius: Option<usize>,
    nms_freq_radius: Option<usize>,
    nms_score_delta_db: Option<f64>,
    min_sync_score: Option<f64>,
    adaptive_ldpc_iters: Option<bool>,
    time_range: Option<f64>,
    max_parity_errors_for_osd: Option<usize>,
    /// hb-044: enable Costas time-axis parabolic refinement.
    sync_time_interpolation: Option<bool>,
    /// hb-068 variant (a) — score gate; only refine when score > gate.
    sync_time_interp_score_gate: Option<f64>,
    /// hb-068 variant (b) — scale parabolic delta by this factor.
    sync_time_interp_delta_scale: Option<f64>,
    /// hb-068 variant (c) — reject |delta| > threshold (fall back to integer).
    sync_time_interp_max_delta_abs: Option<f64>,
    /// hb-069: interpolate spectrogram lookups in linear power instead of dB.
    sync_time_interp_linear_power: Option<bool>,
    /// hb-067: mBP offset value (subtract from |LLR| before OSD).
    bp_offset_subtract: Option<f32>,
    /// hb-063: enable layered (row-sequential) BP schedule.
    layered_bp: Option<bool>,
    /// hb-056: enable cross-cycle non-coherent symbol averaging.
    cross_cycle_averaging: Option<bool>,
    /// hb-074: coherent (phase-aligned complex sum) variant of cross-cycle averaging.
    cross_cycle_coherent: Option<bool>,
    /// hb-075: MRC-weighted variant of coherent cross-cycle averaging.
    cross_cycle_coherent_mrc: Option<bool>,
    /// hb-079 + hb-080: number of coherent subtract+repass rounds.
    coherent_multipass_iterations: Option<u8>,
    /// hb-081: MRC subtract scaling threshold (0 disables).
    coherent_subtract_mrc_threshold: Option<f64>,
    /// hb-082: residual sync_score threshold (None reuses production).
    residual_min_sync_score: Option<f64>,
    /// hb-086 V1: force-retry failed original candidates on residual.
    joint_pair_retry: Option<bool>,
    /// hb-086 V3: dB relaxation on the bin-targeted residual sync pass
    /// (0.0 = disabled, negative = lower min_sync_score by that much
    /// only at freq_bins within ±window of subtracted positions).
    joint_residual_sync_relax_db: Option<f64>,
    /// hb-086 V3: half-width in freq_bins of the bin-targeting window
    /// for the V3 localized residual sync pass.
    joint_residual_sync_window_bins: Option<usize>,
    /// hb-016: residual energy early-stop margin in dB (None disables).
    residual_energy_stop_db: Option<f64>,
    /// hb-093: per-position residual SNR pre-decode gate (dB, WAV-relative).
    /// None disables; Some(db) skips LDPC at residual joint_pair_retry
    /// candidates with SNR < db.
    residual_snr_gate_db: Option<f64>,
    /// hb-048 Session 3: enable a7 template cross-correlation pass.
    a7_enabled: Option<bool>,
    /// hb-048: snr7 acceptance threshold (default 6.0 per WSJT-X).
    a7_snr7_threshold: Option<f64>,
    /// hb-048: snr7b acceptance threshold (default 1.8 per WSJT-X).
    a7_snr7b_threshold: Option<f64>,
    /// hb-048: freq-window in Hz around each expected call (default 6.25).
    a7_freq_window_hz: Option<f64>,
    /// hb-046: enable two-stage decoding (cheap pass + standard pass, unioned).
    two_stage: Option<bool>,
    /// hb-004: when Some, an ApContext is built and passed to
    /// `decode_window_with_ap`. Empty `None` means default behavior
    /// (decode_window with default-empty context → AP never fires).
    ap_my_call: Option<String>,
    ap_recent_calls: Option<Vec<String>>,
    /// hb-050: enable rolling-callsign-window mode with capacity N.
    ap_rolling_window: Option<usize>,
    /// hb-052 FP filter: build the reference set from corpus baselines
    /// at this dir (one .json per WAV). When set, every decode is
    /// passed through the filter post-decode; rejected decodes don't
    /// count toward the scorecard.
    fp_filter_baselines: Option<PathBuf>,
    /// hb-052: enable a rolling-window callsign source for the FP
    /// filter (capacity N). Combined with `fp_filter_baselines` via
    /// OR-of-membership.
    fp_filter_rolling: Option<usize>,
    /// hb-052: build the reference set from an ADIF file's CALL
    /// fields. Used for production-style validation (operator log
    /// is the natural source). Can combine with baselines via OR.
    fp_filter_adif: Option<PathBuf>,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut tiers: Option<Vec<String>> = None;
        let mut mode: Option<Mode> = None;
        let mut output: Option<PathBuf> = None;
        let mut seed: u64 = 42;
        let mut max_passes: Option<usize> = None;
        let mut max_sync_candidates: Option<usize> = None;
        let mut max_candidates: Option<usize> = None;
        let mut osd_depth: Option<Option<u8>> = None;
        let mut ldpc_iterations: Option<usize> = None;
        let mut llr_target_variance: Option<f32> = None;
        let mut nms_enabled: Option<bool> = None;
        let mut nms_time_radius: Option<usize> = None;
        let mut nms_freq_radius: Option<usize> = None;
        let mut nms_score_delta_db: Option<f64> = None;
        let mut min_sync_score: Option<f64> = None;
        let mut adaptive_ldpc_iters: Option<bool> = None;
        let mut time_range: Option<f64> = None;
        let mut max_parity_errors_for_osd: Option<usize> = None;
        let mut sync_time_interpolation: Option<bool> = None;
        let mut sync_time_interp_score_gate: Option<f64> = None;
        let mut sync_time_interp_delta_scale: Option<f64> = None;
        let mut sync_time_interp_max_delta_abs: Option<f64> = None;
        let mut sync_time_interp_linear_power: Option<bool> = None;
        let mut bp_offset_subtract: Option<f32> = None;
        let mut layered_bp: Option<bool> = None;
        let mut cross_cycle_averaging: Option<bool> = None;
        let mut cross_cycle_coherent: Option<bool> = None;
        let mut cross_cycle_coherent_mrc: Option<bool> = None;
        let mut coherent_multipass_iterations: Option<u8> = None;
        let mut coherent_subtract_mrc_threshold: Option<f64> = None;
        let mut residual_min_sync_score: Option<f64> = None;
        let mut joint_pair_retry: Option<bool> = None;
        let mut joint_residual_sync_relax_db: Option<f64> = None;
        let mut joint_residual_sync_window_bins: Option<usize> = None;
        let mut residual_energy_stop_db: Option<f64> = None;
        let mut residual_snr_gate_db: Option<f64> = None;
        let mut a7_enabled: Option<bool> = None;
        let mut a7_snr7_threshold: Option<f64> = None;
        let mut a7_snr7b_threshold: Option<f64> = None;
        let mut a7_freq_window_hz: Option<f64> = None;
        let mut two_stage: Option<bool> = None;
        let mut ap_my_call: Option<String> = None;
        let mut ap_recent_calls: Option<Vec<String>> = None;
        let mut ap_rolling_window: Option<usize> = None;
        let mut fp_filter_baselines: Option<PathBuf> = None;
        let mut fp_filter_rolling: Option<usize> = None;
        let mut fp_filter_adif: Option<PathBuf> = None;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--tier" | "--tiers" => {
                    tiers = Some(
                        iter.next()
                            .context("--tier needs a value")?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect(),
                    );
                }
                "--mode" => {
                    mode = Some(
                        iter.next()
                            .context("--mode needs a value")?
                            .parse::<Mode>()
                            .map_err(|e| anyhow::anyhow!("{e}"))?,
                    );
                }
                "--output" => {
                    output = Some(iter.next().context("--output needs a value")?.into());
                }
                "--seed" => {
                    seed = iter.next().context("--seed needs a value")?.parse()?;
                }
                "--max-passes" => {
                    max_passes = Some(iter.next().context("--max-passes needs a value")?.parse()?);
                }
                "--max-sync-candidates" => {
                    max_sync_candidates = Some(
                        iter.next()
                            .context("--max-sync-candidates needs a value")?
                            .parse()?,
                    );
                }
                "--max-candidates" => {
                    max_candidates = Some(
                        iter.next()
                            .context("--max-candidates needs a value")?
                            .parse()?,
                    );
                }
                "--osd-depth" => {
                    let s = iter.next().context("--osd-depth needs a value")?;
                    osd_depth = Some(if s == "none" || s == "off" {
                        None
                    } else {
                        Some(s.parse()?)
                    });
                }
                "--ldpc-iters" => {
                    ldpc_iterations =
                        Some(iter.next().context("--ldpc-iters needs a value")?.parse()?);
                }
                "--llr-target-variance" => {
                    llr_target_variance = Some(
                        iter.next()
                            .context("--llr-target-variance needs a value")?
                            .parse()?,
                    );
                }
                "--no-nms" => {
                    nms_enabled = Some(false);
                }
                "--nms-on" => {
                    nms_enabled = Some(true);
                }
                "--nms-time-radius" => {
                    nms_time_radius = Some(
                        iter.next()
                            .context("--nms-time-radius needs a value")?
                            .parse()?,
                    );
                    // Setting a radius implicitly opts back into NMS
                    // unless --no-nms is also passed; respect explicit flag.
                    nms_enabled.get_or_insert(true);
                }
                "--nms-freq-radius" => {
                    nms_freq_radius = Some(
                        iter.next()
                            .context("--nms-freq-radius needs a value")?
                            .parse()?,
                    );
                    nms_enabled.get_or_insert(true);
                }
                "--nms-score-delta-db" => {
                    nms_score_delta_db = Some(
                        iter.next()
                            .context("--nms-score-delta-db needs a value")?
                            .parse()?,
                    );
                    // hb-036: a non-zero score-delta implies NMS is active
                    // (the gate only fires when nms_enabled = true).
                    nms_enabled.get_or_insert(true);
                }
                "--min-sync-score" => {
                    min_sync_score = Some(
                        iter.next()
                            .context("--min-sync-score needs a value")?
                            .parse()?,
                    );
                }
                "--adaptive-ldpc-iters" => {
                    adaptive_ldpc_iters = Some(true);
                }
                "--time-range" => {
                    time_range = Some(iter.next().context("--time-range needs a value")?.parse()?);
                }
                "--max-parity-errors-for-osd" => {
                    max_parity_errors_for_osd = Some(
                        iter.next()
                            .context("--max-parity-errors-for-osd needs a value")?
                            .parse()?,
                    );
                }
                "--sync-time-interpolation" => {
                    sync_time_interpolation = Some(true);
                }
                "--sync-time-interp-score-gate" => {
                    sync_time_interp_score_gate = Some(
                        iter.next()
                            .context("--sync-time-interp-score-gate needs a value")?
                            .parse()?,
                    );
                    // Setting a variant knob implicitly enables refinement.
                    sync_time_interpolation.get_or_insert(true);
                }
                "--sync-time-interp-delta-scale" => {
                    sync_time_interp_delta_scale = Some(
                        iter.next()
                            .context("--sync-time-interp-delta-scale needs a value")?
                            .parse()?,
                    );
                    sync_time_interpolation.get_or_insert(true);
                }
                "--sync-time-interp-max-delta-abs" => {
                    sync_time_interp_max_delta_abs = Some(
                        iter.next()
                            .context("--sync-time-interp-max-delta-abs needs a value")?
                            .parse()?,
                    );
                    sync_time_interpolation.get_or_insert(true);
                }
                "--sync-time-interp-linear-power" => {
                    // hb-069: turn on linear-power interpolation. Implies
                    // sync_time_interpolation is also on (the flag is a no-op
                    // when the parabolic refinement isn't running).
                    sync_time_interp_linear_power = Some(true);
                    sync_time_interpolation.get_or_insert(true);
                }
                "--bp-offset-subtract" => {
                    bp_offset_subtract = Some(
                        iter.next()
                            .context("--bp-offset-subtract needs a value")?
                            .parse()?,
                    );
                }
                "--layered-bp" => {
                    layered_bp = Some(true);
                }
                "--cross-cycle-averaging" => {
                    cross_cycle_averaging = Some(true);
                }
                "--no-cross-cycle-averaging" => {
                    cross_cycle_averaging = Some(false);
                }
                "--cross-cycle-coherent" => {
                    cross_cycle_coherent = Some(true);
                }
                "--cross-cycle-coherent-mrc" => {
                    cross_cycle_coherent = Some(true);
                    cross_cycle_coherent_mrc = Some(true);
                }
                "--coherent-multipass" => {
                    coherent_multipass_iterations = Some(1);
                }
                "--no-coherent-multipass" => {
                    coherent_multipass_iterations = Some(0);
                }
                "--coherent-multipass-iters" => {
                    coherent_multipass_iterations = Some(
                        iter.next()
                            .context("--coherent-multipass-iters needs a value")?
                            .parse()?,
                    );
                }
                "--coherent-mrc-threshold" => {
                    coherent_subtract_mrc_threshold = Some(
                        iter.next()
                            .context("--coherent-mrc-threshold needs a value")?
                            .parse()?,
                    );
                }
                "--residual-min-sync-score" => {
                    residual_min_sync_score = Some(
                        iter.next()
                            .context("--residual-min-sync-score needs a value")?
                            .parse()?,
                    );
                }
                "--joint-pair-retry" => {
                    joint_pair_retry = Some(true);
                }
                "--no-joint-pair-retry" => {
                    joint_pair_retry = Some(false);
                }
                "--joint-residual-sync-relax-db" => {
                    joint_residual_sync_relax_db = Some(
                        iter.next()
                            .context("--joint-residual-sync-relax-db needs a value (negative dB; 0 disables)")?
                            .parse()?,
                    );
                }
                "--joint-residual-sync-window-bins" => {
                    joint_residual_sync_window_bins = Some(
                        iter.next()
                            .context("--joint-residual-sync-window-bins needs a value (half-width in freq_bins)")?
                            .parse()?,
                    );
                }
                "--hb016-residual-energy-stop-db" => {
                    residual_energy_stop_db = Some(
                        iter.next()
                            .context("--hb016-residual-energy-stop-db needs a value (dB margin)")?
                            .parse()?,
                    );
                }
                "--residual-snr-gate-db" => {
                    residual_snr_gate_db = Some(
                        iter.next()
                            .context("--residual-snr-gate-db needs a value (dB, WAV-relative; e.g. -5.0)")?
                            .parse()?,
                    );
                }
                "--a7-enabled" => {
                    a7_enabled = Some(true);
                }
                "--no-a7" => {
                    a7_enabled = Some(false);
                }
                "--a7-snr7-threshold" => {
                    a7_snr7_threshold = Some(
                        iter.next()
                            .context("--a7-snr7-threshold needs a value (default 6.0 per WSJT-X)")?
                            .parse()?,
                    );
                    a7_enabled.get_or_insert(true);
                }
                "--a7-snr7b-threshold" => {
                    a7_snr7b_threshold = Some(
                        iter.next()
                            .context("--a7-snr7b-threshold needs a value (default 1.8 per WSJT-X)")?
                            .parse()?,
                    );
                    a7_enabled.get_or_insert(true);
                }
                "--a7-freq-window-hz" => {
                    a7_freq_window_hz = Some(
                        iter.next()
                            .context("--a7-freq-window-hz needs a value (default 6.25)")?
                            .parse()?,
                    );
                    a7_enabled.get_or_insert(true);
                }
                "--two-stage" => {
                    two_stage = Some(true);
                }
                "--ap-my-call" => {
                    ap_my_call = Some(iter.next().context("--ap-my-call needs a value")?);
                }
                "--ap-recent-calls" => {
                    ap_recent_calls = Some(
                        iter.next()
                            .context("--ap-recent-calls needs a value (comma-separated)")?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
                "--ap-rolling-window" => {
                    ap_rolling_window = Some(
                        iter.next()
                            .context("--ap-rolling-window needs a value (N)")?
                            .parse()?,
                    );
                }
                "--fp-filter-baselines" => {
                    fp_filter_baselines = Some(
                        iter.next()
                            .context("--fp-filter-baselines needs a directory path")?
                            .into(),
                    );
                }
                "--fp-filter-rolling" => {
                    fp_filter_rolling = Some(
                        iter.next()
                            .context("--fp-filter-rolling needs a value (N)")?
                            .parse()?,
                    );
                }
                "--fp-filter-adif" => {
                    fp_filter_adif = Some(
                        iter.next()
                            .context("--fp-filter-adif needs a path to an ADIF file")?
                            .into(),
                    );
                }
                "-h" | "--help" => {
                    eprintln!(
                        "usage: eval --tier <tiers,...> --mode <mode> --output <path> [--seed N] [--max-passes N] [--max-sync-candidates N] [--max-candidates N] [--osd-depth N|none] [--ldpc-iters N]"
                    );
                    eprintln!("  tiers: fixtures, synth-clean, synth-doppler, synth-pair-200, curated-hard-200, curated-hard-1000, wild-50, wild-100, wild-doppler-50, hard-jt9-rich-200");
                    eprintln!("  --max-passes: override Ft8Config::max_decode_passes (default 3)");
                    eprintln!("  --max-sync-candidates: override Ft8Config::max_sync_candidates (default 200)");
                    eprintln!(
                        "  --max-candidates: override Ft8Config::max_candidates (default 100)"
                    );
                    eprintln!("  --osd-depth: override Ft8Config::osd_depth — N is 0..3 or 'none' to disable (default 2)");
                    eprintln!("  --ldpc-iters: override Ft8Config::ldpc_iterations (default 50)");
                    eprintln!("  --llr-target-variance: override Ft8Config::llr_target_variance (default 32.0)");
                    eprintln!(
                        "  --no-nms: disable non-maximum suppression of Costas sync candidates"
                    );
                    eprintln!("  --nms-on: explicitly re-enable NMS (production default is off)");
                    eprintln!("  --nms-time-radius N: override Ft8Config::nms_time_radius (default 8); implies --nms-on");
                    eprintln!("  --nms-freq-radius N: override Ft8Config::nms_freq_radius (default 2); implies --nms-on");
                    eprintln!("  --nms-score-delta-db V: hb-036 score-relative NMS suppression delta (default 0.0 = pure TF-distance); implies --nms-on");
                    eprintln!(
                        "  --min-sync-score V: override Ft8Config::min_sync_score (default 3.0)"
                    );
                    eprintln!("  --adaptive-ldpc-iters: enable hb-022 SNR-adaptive per-candidate LDPC iterations");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            tiers: tiers.context("--tier required")?,
            mode: mode.context("--mode required")?,
            output: output.context("--output required")?,
            seed,
            max_passes,
            max_sync_candidates,
            max_candidates,
            osd_depth,
            ldpc_iterations,
            llr_target_variance,
            nms_enabled,
            nms_time_radius,
            nms_freq_radius,
            nms_score_delta_db,
            min_sync_score,
            adaptive_ldpc_iters,
            time_range,
            max_parity_errors_for_osd,
            sync_time_interpolation,
            sync_time_interp_score_gate,
            sync_time_interp_delta_scale,
            sync_time_interp_max_delta_abs,
            sync_time_interp_linear_power,
            bp_offset_subtract,
            layered_bp,
            cross_cycle_averaging,
            cross_cycle_coherent,
            cross_cycle_coherent_mrc,
            coherent_multipass_iterations,
            coherent_subtract_mrc_threshold,
            residual_min_sync_score,
            joint_pair_retry,
            joint_residual_sync_relax_db,
            joint_residual_sync_window_bins,
            residual_energy_stop_db,
            residual_snr_gate_db,
            a7_enabled,
            a7_snr7_threshold,
            a7_snr7b_threshold,
            a7_freq_window_hz,
            two_stage,
            ap_my_call,
            ap_recent_calls,
            ap_rolling_window,
            fp_filter_baselines,
            fp_filter_rolling,
            fp_filter_adif,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

/// Apply the FP filter to a decode vector in place, dropping rejected
/// decodes. Updates the rolling window via the `update_rolling=true`
/// path so the filter learns within the eval run.
fn apply_fp_filter(
    filter: Option<&pancetta_research::FpFilter>,
    decodes: &mut Vec<pancetta_research::Decode>,
) {
    if let Some(f) = filter {
        decodes.retain(|d| f.accept(&d.message, true));
    }
}

fn run_fixtures_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
) -> anyhow::Result<TierResult> {
    // Fixtures tier is a decoder regression test — it does NOT apply the
    // FP filter. The eval-side FpFilter is strict-membership against jt9
    // baselines; fixture WAVs (e.g. basicft8/170923_082015.wav from 2017)
    // contain callsigns absent from those baselines and would be falsely
    // dropped. Production CallsignContinuityFilter has cold-start lenient
    // mode that prevents this in a real station. Filter behavior is
    // validated separately by cross_validate_novels.rs and the hard-corpus
    // tiers.
    let truth_path = workspace.join("research/corpus/fixtures/ft8/truth.json");
    let truth = FixtureTruth::load(&truth_path)?;
    let fixtures = load_ft8_fixtures(workspace)?;
    let total = fixtures.len() as u32;
    let mut passed = 0u32;
    let mut skipped = 0u32;
    let mut failures = Vec::new();
    for f in &fixtures {
        let entry = truth.get(&f.display_name);
        let decodes_result = decoder.decode_wav(&f.wav_path);
        match (decodes_result, entry) {
            (Ok(decodes), Some(entry)) => match entry.category {
                FixtureCategory::Exact => {
                    let all_present = entry
                        .expect
                        .iter()
                        .all(|expected| decodes.iter().any(|d| d.message.contains(expected)));
                    if all_present {
                        passed += 1;
                    } else {
                        failures.push(pancetta_research::scorecard::FixtureFailure {
                            wav: f.display_name.clone(),
                            expected: entry.expect.clone(),
                            got: decodes.iter().map(|d| d.message.clone()).collect(),
                        });
                    }
                }
                FixtureCategory::AnyDecode => {
                    if !decodes.is_empty() {
                        passed += 1;
                    } else {
                        failures.push(pancetta_research::scorecard::FixtureFailure {
                            wav: f.display_name.clone(),
                            expected: vec!["any-decode".into()],
                            got: vec![],
                        });
                    }
                }
                FixtureCategory::Skip => {
                    // Skipped fixtures are excluded from the pass_rate denominator.
                    // Promoting a Skip → AnyDecode or Exact will widen the denominator
                    // and produce a real metric movement.
                    skipped += 1;
                }
            },
            (Ok(decodes), None) => {
                // Fixture exists on disk but not in truth.json — informational only.
                failures.push(pancetta_research::scorecard::FixtureFailure {
                    wav: f.display_name.clone(),
                    expected: vec![format!(
                        "no truth.json entry for {} — add one before counting as pass/fail",
                        f.display_name
                    )],
                    got: decodes.iter().map(|d| d.message.clone()).collect(),
                });
            }
            (Err(e), entry) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: entry.map(|e| e.expect.clone()).unwrap_or_default(),
                got: vec![format!("error: {e}")],
            }),
        }
    }
    let failed = total - passed - skipped;
    let gated = total - skipped;
    let pass_rate = if gated == 0 {
        0.0
    } else {
        passed as f64 / gated as f64
    };
    Ok(TierResult {
        wavs_processed: total,
        fixtures_total: Some(total),
        fixtures_passed: Some(passed),
        fixtures_failed: Some(failed),
        fixtures_skipped: Some(skipped),
        failures,
        pass_rate: Some(pass_rate),
        ..Default::default()
    })
}

fn run_synth_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
) -> anyhow::Result<TierResult> {
    let entries = load_synth_corpus(workspace, manifest_path)?;
    // Group by snr_db bin.
    let mut bins: BTreeMap<i64, (u32, u32)> = BTreeMap::new(); // key = snr*10 to avoid float keys
    let mut wavs_processed = 0u32;
    // hb-129: per-WAV TTFD collection for the synth-clean tier.
    let mut per_wav_ttfd_s: Vec<f64> = Vec::new();
    for e in &entries {
        wavs_processed += 1;
        let bin_key = (e.snr_db * 10.0).round() as i64;
        let bin = bins.entry(bin_key).or_insert((0, 0));
        bin.0 += 1; // attempts
        match decoder.decode_wav(&e.wav_path) {
            Ok(mut decodes) => {
                apply_fp_filter(fp_filter, &mut decodes);
                if let Some(min_ttfd) = decodes
                    .iter()
                    .filter_map(|d| d.decode_time_into_window_s)
                    .fold(None::<f64>, |acc, t| match acc {
                        None => Some(t),
                        Some(cur) => Some(cur.min(t)),
                    })
                {
                    per_wav_ttfd_s.push(min_ttfd);
                }
                if decodes
                    .iter()
                    .any(|d| d.message.contains(&e.encoded_message))
                {
                    bin.1 += 1; // decoded
                }
            }
            Err(_) => {
                // Decode error — counts as failed attempt.
            }
        }
    }
    let mut by_snr: Vec<SnrBin> = bins
        .iter()
        .map(|(k, (attempts, decoded))| SnrBin {
            snr_db: (*k as f64) / 10.0,
            attempts: *attempts,
            decoded: *decoded,
            fp: 0,
        })
        .collect();
    by_snr.sort_by(|a, b| a.snr_db.partial_cmp(&b.snr_db).unwrap());
    // Find SNR @ 50% and 90% recovery (first bin where decoded/attempts >= threshold).
    let snr_at_50 = first_threshold_db(&by_snr, 0.50);
    let snr_at_90 = first_threshold_db(&by_snr, 0.90);
    let ttfd_distribution = TtfdDistribution::from_per_wav(per_wav_ttfd_s);
    Ok(TierResult {
        wavs_processed,
        by_snr_db: by_snr,
        snr_at_50pct_recovery_db: snr_at_50,
        snr_at_90pct_recovery_db: snr_at_90,
        ttfd_distribution,
        ..Default::default()
    })
}

/// hb-146 — synth-pair adversarial mutual-masking pair tier. Each WAV
/// contains two FT8 signals at controlled (ΔSNR, Δf, Δt). Reports
/// per-bucket recovery (strong vs weak) so the regime where pancetta
/// drops the weak signal is visible and V2/V3 hypotheses can target it.
fn run_synth_pair_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
) -> anyhow::Result<TierResult> {
    let entries = load_synth_pair_corpus(workspace, manifest_path)?;
    let total = entries.len() as u32;
    if total == 0 {
        return Ok(TierResult {
            wavs_processed: 0,
            ..Default::default()
        });
    }

    // Per-bucket counters keyed by (delta_snr*10, delta_freq*10, delta_time*100)
    // — integer keys avoid float-ordering ambiguity. Each bucket tracks
    // (strong_recovered, weak_recovered, attempts).
    type Bucket = (u32, u32, u32);
    let mut buckets: BTreeMap<(i64, i64, i64), Bucket> = BTreeMap::new();
    let mut strong_total = 0u32;
    let mut weak_total = 0u32;

    for entry in &entries {
        let key = (
            (entry.delta_snr_db * 10.0).round() as i64,
            (entry.delta_freq_hz * 10.0).round() as i64,
            (entry.delta_time_s * 100.0).round() as i64,
        );
        let bucket = buckets.entry(key).or_insert((0, 0, 0));
        bucket.2 += 1;

        let mut decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        apply_fp_filter(fp_filter, &mut decodes);

        let got_strong = decodes
            .iter()
            .any(|d| d.message.contains(&entry.message_strong));
        let got_weak = decodes
            .iter()
            .any(|d| d.message.contains(&entry.message_weak));
        if got_strong {
            bucket.0 += 1;
            strong_total += 1;
        }
        if got_weak {
            bucket.1 += 1;
            weak_total += 1;
        }
    }

    // Print per-bucket regime map to stderr. The scorecard JSON keeps the
    // aggregate (decode_rate over 2*total truths); the regime breakdown
    // is operator-readable.
    eprintln!(
        "synth-pair-200 regime map ({} WAVs, 2 truths per WAV):",
        total
    );
    eprintln!(
        "  {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>8} {:>8}",
        "dSNR", "dF_Hz", "dT_s", "n", "strong", "weak", "rec_s%", "rec_w%"
    );
    for ((dsnr_k, df_k, dt_k), (strong, weak, n)) in &buckets {
        let dsnr = (*dsnr_k as f64) / 10.0;
        let df = (*df_k as f64) / 10.0;
        let dt = (*dt_k as f64) / 100.0;
        let rec_s = if *n > 0 {
            100.0 * *strong as f64 / *n as f64
        } else {
            0.0
        };
        let rec_w = if *n > 0 {
            100.0 * *weak as f64 / *n as f64
        } else {
            0.0
        };
        eprintln!(
            "  {:>8.1} {:>8.1} {:>8.2} {:>6} {:>6} {:>6} {:>7.1}% {:>7.1}%",
            dsnr, df, dt, n, strong, weak, rec_s, rec_w,
        );
    }
    eprintln!(
        "synth-pair-200 totals: strong_recovered={}/{} ({:.1}%), weak_recovered={}/{} ({:.1}%)",
        strong_total,
        total,
        100.0 * strong_total as f64 / total as f64,
        weak_total,
        total,
        100.0 * weak_total as f64 / total as f64,
    );

    let truth_total = total * 2; // strong + weak per WAV
    let recovered = strong_total + weak_total;
    let decode_rate = recovered as f64 / truth_total as f64;
    Ok(TierResult {
        wavs_processed: total,
        truth_decodes_total: Some(truth_total),
        truth_decodes_recovered: Some(recovered),
        decode_rate: Some(decode_rate),
        ..Default::default()
    })
}

fn run_curated_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
) -> anyhow::Result<TierResult> {
    let entries: Vec<CuratedEntry> = load_curated_corpus(manifest_path)?;
    let total = entries.len() as u32;
    if total == 0 {
        return Ok(TierResult {
            wavs_processed: 0,
            ..Default::default()
        });
    }
    let mut truth_decodes_total = 0u32;
    let mut truth_recovered = 0u32;
    let mut novel_decodes = 0u32;
    let mut wsjtx_total = 0u32;
    let mut per_wav_failures: Vec<PerWavFailure> = Vec::new();
    // Phase B (2026-06-01): full per-WAV (truth, recovered, novel) records
    // for bootstrap-CI input. Unlike per_wav_failures (truncated to top-20),
    // this is one entry per WAV in the tier.
    let mut per_wav_records: Vec<PerWavRecord> = Vec::new();
    // hb-129: per-WAV TTFD collection.
    let mut per_wav_ttfd_s: Vec<f64> = Vec::new();

    for entry in &entries {
        // Look up the jt9 baseline cache for this WAV's SHA.
        let baseline_path = workspace
            .join("research/baselines/ft8")
            .join(format!("{}.json", entry.wav_sha256));
        let baseline_decodes: Vec<String> = if baseline_path.exists() {
            let s = std::fs::read_to_string(&baseline_path)?;
            let cache: serde_json::Value = serde_json::from_str(&s)?;
            cache
                .get("decodes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            // No baseline cached — treat as 0 truth decodes for this WAV.
            Vec::new()
        };
        wsjtx_total += baseline_decodes.len() as u32;
        truth_decodes_total += baseline_decodes.len() as u32;

        let mut our_decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        apply_fp_filter(fp_filter, &mut our_decodes);
        // hb-129: per-WAV TTFD — min decode_time_into_window_s over decodes.
        // WAVs with zero stamped decodes don't contribute to the distribution.
        if let Some(min_ttfd) = our_decodes
            .iter()
            .filter_map(|d| d.decode_time_into_window_s)
            .fold(None::<f64>, |acc, t| match acc {
                None => Some(t),
                Some(cur) => Some(cur.min(t)),
            })
        {
            per_wav_ttfd_s.push(min_ttfd);
        }
        // Match: a baseline decode is "recovered" if we produced a message
        // containing the same callsign tokens. Conservative substring check.
        let mut recovered_here = 0u32;
        for truth_msg in &baseline_decodes {
            if our_decodes
                .iter()
                .any(|d| d.message.trim() == truth_msg.trim())
            {
                recovered_here += 1;
            }
        }
        truth_recovered += recovered_here;

        // "Novel" decodes: ones in our output that aren't in baseline.
        let mut novel_here = 0u32;
        for ours in &our_decodes {
            if !baseline_decodes
                .iter()
                .any(|t| t.trim() == ours.message.trim())
            {
                novel_decodes += 1;
                novel_here += 1;
            }
        }

        // Per-WAV failure tracking for the top 20 worst gaps.
        let gap = baseline_decodes.len() as i64 - recovered_here as i64;
        if gap > 0 {
            per_wav_failures.push(PerWavFailure {
                wav_hash: entry.wav_sha256.clone(),
                truth: baseline_decodes.len() as u32,
                recovered: recovered_here,
                wsjtx: baseline_decodes.len() as u32,
                jtdx: 0, // Plan 3 doesn't wire JTDX; field stays 0.
            });
        }

        // Phase B: full per-WAV record for unbiased bootstrap-CI input.
        // Recorded for every WAV in the tier (not just failures).
        per_wav_records.push(PerWavRecord {
            wav_hash: entry.wav_sha256.clone(),
            truth: baseline_decodes.len() as u32,
            recovered: recovered_here,
            novel: novel_here,
        });
    }

    // Keep top-20 worst gaps for the per_wav_top_failures field.
    per_wav_failures.sort_by(|a, b| (b.truth - b.recovered).cmp(&(a.truth - a.recovered)));
    per_wav_failures.truncate(20);

    let decode_rate = if truth_decodes_total == 0 {
        0.0
    } else {
        truth_recovered as f64 / truth_decodes_total as f64
    };
    let vs_wsjtx_pct = if wsjtx_total == 0 {
        0.0
    } else {
        100.0 * truth_recovered as f64 / wsjtx_total as f64
    };

    let ttfd_distribution = TtfdDistribution::from_per_wav(per_wav_ttfd_s);
    if let Some(ttfd) = &ttfd_distribution {
        eprintln!(
            "curated tier TTFD: n={} wavs, p50={:.3}s p90={:.3}s mean={:.3}s",
            ttfd.wavs_with_decode, ttfd.p50_seconds, ttfd.p90_seconds, ttfd.mean_seconds,
        );
    }

    Ok(TierResult {
        wavs_processed: total,
        truth_decodes_total: Some(truth_decodes_total),
        truth_decodes_recovered: Some(truth_recovered),
        decode_rate: Some(decode_rate),
        novel_decodes: Some(novel_decodes),
        wsjtx_decoded: Some(wsjtx_total),
        vs_wsjtx_pct: Some(vs_wsjtx_pct),
        per_wav_top_failures: per_wav_failures,
        per_wav_records,
        ttfd_distribution,
        ..Default::default()
    })
}

/// Lowest SNR (in dB) where recovery >= threshold. Bins must be sorted by SNR asc.
fn first_threshold_db(bins: &[SnrBin], threshold: f64) -> Option<f64> {
    for bin in bins {
        if bin.attempts > 0 && (bin.decoded as f64) / (bin.attempts as f64) >= threshold {
            return Some(bin.snr_db);
        }
    }
    None
}

fn git_info(workspace: &std::path::Path) -> GitInfo {
    let run = |args: &[&str]| -> String {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = run(&["rev-parse", "HEAD"]);
    let merge_base = run(&["merge-base", "main", "HEAD"]);
    let dirty = !run(&["status", "--porcelain"]).is_empty();
    GitInfo {
        branch,
        head_sha: sha,
        main_merge_base: merge_base,
        dirty,
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() -> anyhow::Result<()> {
    // Preflight gate. If --preflight refuses, the binary refuses too.
    let preflight = std::process::Command::new("./scripts/research-env.sh")
        .arg("--preflight")
        .current_dir(workspace_root()?)
        .status();
    match preflight {
        Ok(status) if !status.success() => {
            anyhow::bail!("preflight failed; aborting eval");
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "warn: preflight script not found or not executable ({e}); skipping disk check",
            );
        }
    }

    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let started = Instant::now();
    let decoder: Box<dyn DecoderUnderTest> = match args.mode {
        Mode::Ft8 => {
            let mut d = Ft8Decoder::with_default_config();
            if let Some(n) = args.max_passes {
                d = d.with_max_passes(n);
            }
            if let Some(n) = args.max_sync_candidates {
                d = d.with_max_sync_candidates(n);
            }
            if let Some(n) = args.max_candidates {
                d = d.with_max_candidates(n);
            }
            if let Some(depth) = args.osd_depth {
                d = d.with_osd_depth(depth);
            }
            if let Some(n) = args.ldpc_iterations {
                d = d.with_ldpc_iterations(n);
            }
            if let Some(v) = args.llr_target_variance {
                d = d.with_llr_target_variance(v);
            }
            if let Some(b) = args.nms_enabled {
                d = d.with_nms_enabled(b);
            }
            if let Some(n) = args.nms_time_radius {
                d = d.with_nms_time_radius(n);
            }
            if let Some(n) = args.nms_freq_radius {
                d = d.with_nms_freq_radius(n);
            }
            if let Some(v) = args.nms_score_delta_db {
                d = d.with_nms_score_delta_db(v);
            }
            if let Some(v) = args.min_sync_score {
                d = d.with_min_sync_score(v);
            }
            if let Some(on) = args.adaptive_ldpc_iters {
                d = d.with_adaptive_ldpc_iters(on);
            }
            if let Some(v) = args.time_range {
                d = d.with_time_range(v);
            }
            if let Some(n) = args.max_parity_errors_for_osd {
                d = d.with_max_parity_errors_for_osd(n);
            }
            if let Some(on) = args.sync_time_interpolation {
                d = d.with_sync_time_interpolation(on);
            }
            if let Some(v) = args.sync_time_interp_score_gate {
                d = d.with_sync_time_interp_score_gate(v);
            }
            if let Some(v) = args.sync_time_interp_delta_scale {
                d = d.with_sync_time_interp_delta_scale(v);
            }
            if args.sync_time_interp_max_delta_abs.is_some() {
                d = d.with_sync_time_interp_max_delta_abs(args.sync_time_interp_max_delta_abs);
            }
            if let Some(on) = args.sync_time_interp_linear_power {
                d = d.with_sync_time_interp_linear_power(on);
            }
            if let Some(v) = args.bp_offset_subtract {
                d = d.with_bp_offset_subtract(v);
            }
            if let Some(on) = args.layered_bp {
                d = d.with_layered_bp(on);
            }
            if let Some(on) = args.cross_cycle_averaging {
                d = d.with_cross_cycle_averaging(on);
            }
            if let Some(on) = args.cross_cycle_coherent {
                d = d.with_cross_cycle_coherent(on);
            }
            if let Some(on) = args.cross_cycle_coherent_mrc {
                d = d.with_cross_cycle_coherent_mrc(on);
            }
            if let Some(n) = args.coherent_multipass_iterations {
                d = d.with_coherent_multipass_iterations(n);
            }
            if let Some(t) = args.coherent_subtract_mrc_threshold {
                d = d.with_coherent_subtract_mrc_threshold(t);
            }
            if args.residual_min_sync_score.is_some() {
                d = d.with_residual_min_sync_score(args.residual_min_sync_score);
            }
            if let Some(on) = args.joint_pair_retry {
                d = d.with_joint_pair_retry(on);
            }
            if let Some(db) = args.joint_residual_sync_relax_db {
                d = d.with_joint_residual_sync_relax_db(db);
            }
            if let Some(n) = args.joint_residual_sync_window_bins {
                d = d.with_joint_residual_sync_window_bins(n);
            }
            if args.residual_energy_stop_db.is_some() {
                d = d.with_residual_energy_stop_db(args.residual_energy_stop_db);
            }
            if args.residual_snr_gate_db.is_some() {
                d = d.with_residual_snr_gate_db(args.residual_snr_gate_db);
            }
            if let Some(on) = args.a7_enabled {
                d = d.with_a7_enabled(on);
            }
            if let Some(t) = args.a7_snr7_threshold {
                d = d.with_a7_snr7_threshold(t);
            }
            if let Some(t) = args.a7_snr7b_threshold {
                d = d.with_a7_snr7b_threshold(t);
            }
            if let Some(hz) = args.a7_freq_window_hz {
                d = d.with_a7_freq_window_hz(hz);
            }
            if let Some(on) = args.two_stage {
                d = d.with_two_stage(on);
            }
            // hb-004: build an ApContext from CLI flags if any AP knob set.
            if args.ap_my_call.is_some() || args.ap_recent_calls.is_some() {
                use pancetta_ft8::ap::{ApContext, MyCallAp, RecentCallAp};
                let my_call = args.ap_my_call.as_ref().and_then(|c| {
                    let r = MyCallAp::new(c);
                    if r.is_none() {
                        eprintln!("warning: --ap-my-call {c:?} did not encode (returned None)");
                    }
                    r
                });
                let recent_calls = args
                    .ap_recent_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .filter_map(|c| {
                                let r = RecentCallAp::new(c, 0.0);
                                if r.is_none() {
                                    eprintln!(
                                        "warning: --ap-recent-calls entry {c:?} did not encode"
                                    );
                                }
                                r
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let ctx = ApContext {
                    my_call,
                    recent_calls,
                    active_qso: None,
                };
                d = d.with_ap_context(ctx);
            }
            // hb-050: rolling-window mode overrides per-call ApContext.
            if let Some(n) = args.ap_rolling_window {
                d = d.with_rolling_window(n);
            }
            Box::new(d)
        }
    };

    // hb-052: build FP filter from configured sources, if any.
    let fp_filter: Option<pancetta_research::FpFilter> = if args.fp_filter_baselines.is_some()
        || args.fp_filter_rolling.is_some()
        || args.fp_filter_adif.is_some()
    {
        let mut f = pancetta_research::FpFilter::new();
        if let Some(ref dir) = args.fp_filter_baselines {
            let resolved = if dir.is_absolute() {
                dir.clone()
            } else {
                workspace.join(dir)
            };
            let n = f.extend_from_baselines(&resolved).with_context(|| {
                format!("loading fp-filter baselines from {}", resolved.display())
            })?;
            eprintln!(
                "fp-filter: loaded {n} baselines from {}, {} unique callsigns so far",
                resolved.display(),
                f.reference_size()
            );
        }
        if let Some(ref adif) = args.fp_filter_adif {
            let resolved = if adif.is_absolute() {
                adif.clone()
            } else {
                workspace.join(adif)
            };
            let n = f
                .extend_from_adif(&resolved)
                .with_context(|| format!("loading fp-filter ADIF from {}", resolved.display()))?;
            eprintln!(
                "fp-filter: loaded {n} callsigns from ADIF {}, {} unique total",
                resolved.display(),
                f.reference_size()
            );
        }
        if let Some(n) = args.fp_filter_rolling {
            f = f.with_rolling_window(n);
            eprintln!("fp-filter: rolling window of {n}");
        }
        Some(f)
    } else {
        None
    };
    let fp_filter_ref = fp_filter.as_ref();

    let mut tiers = BTreeMap::new();
    for tier_name in &args.tiers {
        match tier_name.as_str() {
            "fixtures" => {
                let result = run_fixtures_tier(decoder.as_ref(), &workspace)?;
                tiers.insert("fixtures".to_string(), result);
            }
            "synth-clean" => {
                let manifest =
                    workspace.join("research/corpus/synth/manifests/clean.manifest.json");
                anyhow::ensure!(
                    manifest.exists(),
                    "synth manifest missing at {}; run `cargo run -p pancetta-research --bin gen-synth -- --config research/corpus/synth/manifests/clean.config.json --output research/corpus/synth/manifests/clean.manifest.json`",
                    manifest.display()
                );
                let result =
                    run_synth_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                tiers.insert("synth-clean".to_string(), result);
            }
            "synth-doppler" => {
                let manifest =
                    workspace.join("research/corpus/synth/manifests/doppler.manifest.json");
                anyhow::ensure!(
                    manifest.exists(),
                    "doppler synth manifest missing at {}; run `cargo run --release -p pancetta-research --bin gen-synth -- --config research/corpus/synth/manifests/doppler.config.json --output research/corpus/synth/manifests/doppler.manifest.json`",
                    manifest.display()
                );
                let result =
                    run_synth_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                tiers.insert("synth-doppler".to_string(), result);
            }
            // hb-146 — synthetic adversarial mutual-masking pair tier.
            // Each WAV contains two FT8 signals at controlled (ΔSNR, Δf,
            // Δt). Diagnostic tier (NEVER primary): targets shelved
            // hb-086 V2 (soft cancellation) + V3 (subtract-aware sync
            // relaxation) by building the marginal-SNR pair regime they
            // were designed for on demand.
            "synth-pair-200" => {
                let manifest =
                    workspace.join("research/corpus/synth/manifests/synth_pair_200.manifest.json");
                anyhow::ensure!(
                    manifest.exists(),
                    "synth-pair-200 manifest missing at {}; run `cargo run --release -p pancetta-research --bin gen-synth-pair -- --config research/corpus/synth/manifests/synth_pair_200.config.json --output research/corpus/synth/manifests/synth_pair_200.manifest.json`",
                    manifest.display()
                );
                let result =
                    run_synth_pair_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                tiers.insert("synth-pair-200".to_string(), result);
            }
            "curated-hard-200" | "curated-hard-1000" | "wild-50" | "wild-100" => {
                let label = match tier_name.as_str() {
                    "curated-hard-200" => "hard_200",
                    "curated-hard-1000" => "hard_1000",
                    "wild-50" => "wild_50",
                    "wild-100" => "wild_100",
                    _ => unreachable!(),
                };
                let manifest = workspace
                    .join("research/corpus/curated/ft8")
                    .join(format!("{label}.manifest.json"));
                anyhow::ensure!(
                    manifest.exists(),
                    "curated manifest missing at {}. Run: cargo run --release -p pancetta-research --bin curate -- --source-dir ~/.pancetta/recordings --output-prefix research/corpus/curated/ft8",
                    manifest.display()
                );
                let result =
                    run_curated_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                tiers.insert(tier_name.to_string(), result);
            }
            // hb-073 — real-Doppler eval tier sourced from KiwiSDR auroral/TEP
            // captures. Manifest is curated by the operator after capturing
            // 30-60 slot-aligned 12 kHz WAVs per
            // docs/operations/2026-05-31-hb-073-kiwisdr-capture-procedure.md.
            // Until then, treat a missing manifest as a SKIP so existing eval
            // runs that include this tier do not break.
            "wild-doppler-50" => {
                let manifest =
                    workspace.join("research/corpus/curated/ft8/wild_doppler_50.manifest.json");
                if !manifest.exists() {
                    eprintln!(
                        "tier wild-doppler-50: manifest missing at {} — SKIPPING (operator capture pending; see docs/operations/2026-05-31-hb-073-kiwisdr-capture-procedure.md)",
                        manifest.display()
                    );
                    tiers.insert(
                        "wild-doppler-50".to_string(),
                        TierResult {
                            wavs_processed: 0,
                            ..Default::default()
                        },
                    );
                } else {
                    let result =
                        run_curated_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                    tiers.insert("wild-doppler-50".to_string(), result);
                }
            }
            // hb-150 — high-jt9-novel-density tier curated from existing
            // baselines: WAVs where jt9 finds meaningfully more decodes
            // than pancetta. Stresses recall gaps vs jt9 and unblocks
            // sync-related hypotheses (hb-015 family) + bias-detection
            // work. Manifest is produced by the `curate-jt9-rich` binary;
            // treat missing manifest as a SKIP (matches wild-doppler-50
            // pattern).
            "hard-jt9-rich-200" => {
                let manifest =
                    workspace.join("research/corpus/curated/ft8/hard_jt9_rich_200.manifest.json");
                if !manifest.exists() {
                    eprintln!(
                        "tier hard-jt9-rich-200: manifest missing at {} — SKIPPING (run `cargo run --release -p pancetta-research --bin curate-jt9-rich` to generate)",
                        manifest.display()
                    );
                    tiers.insert(
                        "hard-jt9-rich-200".to_string(),
                        TierResult {
                            wavs_processed: 0,
                            ..Default::default()
                        },
                    );
                } else {
                    let result =
                        run_curated_tier(decoder.as_ref(), &workspace, &manifest, fp_filter_ref)?;
                    tiers.insert("hard-jt9-rich-200".to_string(), result);
                }
            }
            other => anyhow::bail!("unknown tier '{other}'"),
        }
    }

    let mut card = Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: Utc::now(),
        mode: args.mode,
        git: git_info(&workspace),
        build: BuildInfo {
            rustc_version: rustc_version(),
            release: cfg!(not(debug_assertions)),
            features: vec!["research-eval".into()],
        },
        harness: HarnessInfo {
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            host: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            cores_used: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            elapsed_seconds: 0.0,
        },
        config: ConfigInfo {
            decoder: decoder.config_snapshot(),
            seed: args.seed,
            tiers_run: args.tiers.clone(),
            fp_filter_active: fp_filter.is_some(),
        },
        tiers,
        composite: pancetta_research::scorecard::CompositeInfo {
            weights: default_weights(),
            score: 0.0,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: format!("Decoder under test: {}", decoder.identity()),
    };
    populate_composite(&mut card, default_weights());
    card.harness.elapsed_seconds = started.elapsed().as_secs_f64();

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    card.save(&args.output)?;

    // hb-133: load corpus-refresh offsets (if any) and report both raw
    // and saturation-aware composite. The saturation-aware number is
    // comparable across corpus rotations (e.g. 2026-05-30 hard-200 mix
    // update). The raw scorecard on disk is unmodified — offsets are
    // applied at read-time only.
    let offsets_path = workspace.join("research/scorecards/refresh_offsets.json");
    let registry = match RefreshOffsetRegistry::load_or_default(&offsets_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "warn: failed to load corpus-refresh offsets from {} ({e}); reporting raw composite only",
                offsets_path.display(),
            );
            RefreshOffsetRegistry::default()
        }
    };
    let raw = card.composite.score;
    let sat = saturation_aware_composite(raw, &registry);
    let n_offsets = registry.offsets.len();
    println!(
        "wrote scorecard: {} (composite raw {:.4}, saturation-aware {:.4} [{} refresh-offset(s) totaling {:+.4}], {} tier(s), {:.1}s)",
        args.output.display(),
        raw,
        sat,
        n_offsets,
        registry.total_offset(),
        args.tiers.len(),
        card.harness.elapsed_seconds,
    );
    Ok(())
}
