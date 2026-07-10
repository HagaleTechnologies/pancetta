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
    compute_regression_flags, default_weights, populate_composite, saturation_aware_composite,
    RefreshOffsetRegistry,
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
    /// Task W3.5 [A/B]: combine each symbol's two TIME_OSR sub-steps in
    /// linear power instead of dB (separate call site from the flag above).
    linear_power_averaging: Option<bool>,
    /// hb-067: mBP offset value (subtract from |LLR| before OSD).
    bp_offset_subtract: Option<f32>,
    /// hb-063: enable layered (row-sequential) BP schedule.
    layered_bp: Option<bool>,
    /// F1 [A/B]: use the Padé atanh approximant in the BP check-node
    /// update instead of the exact ln form. See `Ft8Config::pade_atanh`.
    pade_atanh: Option<bool>,
    /// F5 [A/B]: disable the redundant half-symbol inner loop in the
    /// Costas sync kernel. See `Ft8Config::costas_half_loop_disabled`.
    costas_half_loop_disabled: Option<bool>,
    /// Decoder-speed-overhaul Task 10 [A/B]: master switch for the BP
    /// escalation ladder. See `Ft8Config::escalation_enabled`.
    escalation_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W1.3 [A/B]: rectangular (no) window
    /// on the fine-FFT fallback's symbol FFT instead of Hann. See
    /// `Ft8Config::fine_fft_rect_window`.
    fine_fft_rect_window: Option<bool>,
    /// Decoder-TP-sensitivity Task W3.3 [A/B]: master switch for the
    /// per-candidate fine-sync + matched-demod stage that replaces the
    /// legacy 21-trial fine-FFT fallback. See
    /// `Ft8Config::fine_sync_enabled`.
    fine_sync_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W3.4 [A/B]: nsym=2/3 noncoherent
    /// combining LLR variants layered on top of the W3.3 matched-demod
    /// stage (only takes effect when `fine_sync_enabled` is ALSO
    /// `true`). See `Ft8Config::nsym_combining_enabled`.
    nsym_combining_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W3.6 [A/B]: re-test of the
    /// per-candidate frequency tracker as a consumer of the W3.3
    /// matched-demod stage (only takes effect when `fine_sync_enabled`
    /// is ALSO `true`). See `Ft8Config::per_candidate_freq_tracker_enabled`.
    per_candidate_freq_tracker_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W4.2/W4.3 [A/B]: master switch for the
    /// per-block time-varying complex-amplitude GFSK subtraction path.
    /// Only has any observable effect when `max_decode_passes >= 2`. See
    /// `Ft8Config::time_varying_subtraction_enabled`.
    time_varying_subtraction_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W4.2/W4.3 [A/B]: subtract the
    /// time-varying fit at full scale (1.0) instead of the legacy 0.9
    /// hold-back. Only takes effect when `time_varying_subtraction_enabled`
    /// is ALSO `true`. See `Ft8Config::full_scale_subtraction_enabled`.
    full_scale_subtraction_enabled: Option<bool>,
    /// Decoder-TP-sensitivity Task W1.4 [A/B]: master switch for the
    /// divisive LLR whitening step, re-measured after the dB/linear
    /// unit-consistency fix. See `Ft8Config::llr_whitening_enabled`.
    llr_whitening: Option<bool>,
    /// Decoder-TP-sensitivity Task W2.5 [A/B]: master switch for the
    /// acceptance-metric-based post-CRC gate that replaces the blunt
    /// sync-score confidence floors. See `Ft8Config::acceptance_gating_enabled`.
    acceptance_gating: Option<bool>,
    /// Decoder-TP-sensitivity Task W2.6 [A/B]: master switch for
    /// `ApLevel::Cq`. See `Ft8Config::cq_ap_enabled`.
    cq_ap: Option<bool>,
    /// Decoder-TP-sensitivity Task W2.6 [A/B]: master switch for the AP4
    /// full message-content mask (RR73/RRR/73). See
    /// `Ft8Config::ap4_full_message_mask_enabled`.
    ap4_full_mask: Option<bool>,
    /// Decoder-TP-sensitivity Task W2.6 [A/B]: AP injection/normalization
    /// ordering. See `Ft8Config::ap_injection_post_normalization`.
    ap_post_normalize: Option<bool>,
    /// Decoder-speed-overhaul Task 9/10: shallow BP iteration count.
    /// See `Ft8Config::floor_iters`.
    floor_iters: Option<usize>,
    /// Decoder-speed-overhaul Task 10: deep BP iteration count. See
    /// `Ft8Config::deep_iters`.
    deep_iters: Option<usize>,
    /// Decoder-speed-overhaul Task 10: max unsatisfied parity checks
    /// tolerated before escalating. See `Ft8Config::escalation_parity_max`.
    escalation_parity_max: Option<usize>,
    /// Task W5.1 [A/B]: master switch for per-bin peak candidate
    /// selection (replaces the flat top-`max_sync_candidates` cap on the
    /// primary sweep with a per-`freq_bin` top-K cut). See
    /// `Ft8Config::per_bin_candidate_selection`.
    per_bin_candidate_selection: Option<bool>,
    /// Task W5.2 [A/B]: master switch for the percentile-normalized
    /// wide-lag two-baseline sync mechanism. See
    /// `Ft8Config::costas_two_baseline_enabled`.
    costas_two_baseline_enabled: Option<bool>,
    /// Task W5.4 [A/B]: master switch for the sync_bc partial-Costas
    /// (blocks B+C only) metric. See `Ft8Config::costas_partial_metric_enabled`.
    costas_partial_metric_enabled: Option<bool>,
    /// Task W5.4 [A/B]: half-width (Hz) of the relaxed-sync window around
    /// the QSO partner. See `Ft8Config::relaxed_sync_near_partner_hz_radius`.
    relaxed_sync_near_partner_hz_radius: Option<f64>,
    /// Task W5.4 [A/B]: signed delta applied to `min_sync_score` inside
    /// the near-partner window. See
    /// `Ft8Config::relaxed_sync_near_partner_score_delta`.
    relaxed_sync_near_partner_score_delta: Option<f64>,
    /// Task W5.4 [HARNESS]: forwards a fixed partner audio frequency (Hz)
    /// to every `decode_wav` call in the tier (simulating "we have an
    /// active QSO parked at this frequency" for the whole run). Only has
    /// an effect when `relaxed_sync_near_partner_hz_radius` is also set.
    partner_freq_hz: Option<f64>,
    /// hb-056: enable cross-cycle non-coherent symbol averaging.
    cross_cycle_averaging: Option<bool>,
    /// hb-074: coherent (phase-aligned complex sum) variant of cross-cycle averaging.
    cross_cycle_coherent: Option<bool>,
    /// hb-075: MRC-weighted variant of coherent cross-cycle averaging.
    cross_cycle_coherent_mrc: Option<bool>,
    /// Task W4.4 [A/B]: LLR-sign correlation threshold content guard for
    /// cross-cycle grouping (`None` = geometric-only, pre-W4.4 behavior).
    cross_cycle_content_guard: Option<f32>,
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
    /// hb-057 V1: enable per-callsign median-DT prior on the residual
    /// sync pass. When set, the eval harness threads a shared
    /// `InMemoryDtHistory` across all WAVs in a tier so the prior
    /// reflects cross-WAV (cross-session-proxy) history. Defaults to
    /// (floor_s=0.2, iqr_scale=3.0) per the spec.
    hb057_dt_history_enabled: Option<bool>,
    /// hb-057 V1: override the minimum prior-gate radius (seconds).
    hb057_dt_history_window_floor_s: Option<f64>,
    /// hb-057 V1: override the IQR scaling factor for the prior gate.
    hb057_dt_history_window_iqr_scale: Option<f64>,
    /// hb-057 V2 (Session 3): frequency window (Hz) for per-candidate
    /// callsign-keyed sync narrowing. Default 25.0 (≈ 4 freq_bins at
    /// 6.25 Hz/bin). Set to 0.0 to fall back to V1 union-of-pass-1
    /// callsigns behavior.
    hb057_dt_history_freq_window_hz: Option<f64>,
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
    /// Chronological-replay tier (2026-06-01): explicit opt-in to
    /// stateful cross-WAV semantics. Auto-set when the `chrono-replay`
    /// tier is in `tiers`. Exposing this as a flag lets ad-hoc dispatch
    /// (e.g. running hard-200 with a stateful decoder for diagnostic
    /// purposes) opt in without renaming the tier.
    chrono_replay_enabled: Option<bool>,
    /// Chronological-replay tier (2026-06-01): cap on the persistent
    /// callsign deque. `None` → unbounded (default for chrono-replay,
    /// where session length naturally bounds growth).
    chrono_replay_capacity: Option<usize>,
    /// Chronological-replay tier (2026-06-01): override the default
    /// manifest path (`research/corpus/curated/ft8/chrono_replay.manifest.json`).
    chrono_replay_manifest: Option<PathBuf>,
    /// Batch 19 (2026-06-02): cap on the number of *heavy* tier runs
    /// that may execute concurrently across all `eval` invocations on
    /// this host. None = unbounded (default; behaviour is unchanged).
    /// When `Some(n)`, each heavy tier acquires one of `n` slots in a
    /// shared file-lock pool (`/tmp/pancetta-eval-tier-slots/`, or
    /// `--max-concurrent-tiers-pool-dir`) before running. Releases on
    /// tier completion. Light tiers (fixtures, synth-*) run
    /// unconditionally — see `pancetta_research::tier_slots::is_heavy_tier`.
    max_concurrent_tiers: Option<usize>,
    /// Override the pool directory for `--max-concurrent-tiers`. Default
    /// is `pancetta_research::tier_slots::DEFAULT_POOL_DIR`.
    max_concurrent_tiers_pool_dir: Option<PathBuf>,
    /// Workstream 0 (2026-07-06): override the default noise-tier manifest
    /// path (`research/corpus/curated/noise/noise_1000.manifest.json`).
    /// Lets tests (and ad-hoc diagnostic runs) point `noise_1000` at a
    /// small generated manifest instead of the full 1000-file corpus.
    noise_manifest: Option<PathBuf>,
    /// Task W0.4 (2026-07-07): override the default `synth-ft4` tier
    /// manifest path (`research/corpus/synth/manifests/ft4_clean.manifest.json`).
    /// Mirrors `--noise-manifest` — lets `tests/ft4_tier_tests.rs` point
    /// the tier at a small generated FT4 manifest instead of the full
    /// 550-file production corpus.
    synth_ft4_manifest: Option<PathBuf>,
    /// Task W3.3b [HARNESS]: per-window wall-clock decode budget in
    /// milliseconds, derived from `--effort <eco|standard|deep|auto|unlimited>`
    /// via `effort_preset_budget_ms` below (mirrors production's
    /// `pancetta/src/coordinator/effort.rs::preset_budget_ms`). `None`
    /// (flag omitted, the default) reproduces `DecodeBudget::unlimited()`
    /// exactly — every existing eval invocation is unaffected.
    effort_budget_ms: Option<u64>,
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
        let mut max_parity_errors_for_osd: Option<usize> = None;
        let mut sync_time_interpolation: Option<bool> = None;
        let mut sync_time_interp_score_gate: Option<f64> = None;
        let mut sync_time_interp_delta_scale: Option<f64> = None;
        let mut sync_time_interp_max_delta_abs: Option<f64> = None;
        let mut sync_time_interp_linear_power: Option<bool> = None;
        let mut linear_power_averaging: Option<bool> = None;
        let mut bp_offset_subtract: Option<f32> = None;
        let mut layered_bp: Option<bool> = None;
        let mut pade_atanh: Option<bool> = None;
        let mut costas_half_loop_disabled: Option<bool> = None;
        let mut escalation_enabled: Option<bool> = None;
        let mut fine_fft_rect_window: Option<bool> = None;
        let mut fine_sync_enabled: Option<bool> = None;
        let mut nsym_combining_enabled: Option<bool> = None;
        let mut per_candidate_freq_tracker_enabled: Option<bool> = None;
        let mut time_varying_subtraction_enabled: Option<bool> = None;
        let mut full_scale_subtraction_enabled: Option<bool> = None;
        let mut llr_whitening: Option<bool> = None;
        let mut acceptance_gating: Option<bool> = None;
        let mut cq_ap: Option<bool> = None;
        let mut ap4_full_mask: Option<bool> = None;
        let mut ap_post_normalize: Option<bool> = None;
        let mut floor_iters: Option<usize> = None;
        let mut deep_iters: Option<usize> = None;
        let mut escalation_parity_max: Option<usize> = None;
        let mut per_bin_candidate_selection: Option<bool> = None;
        let mut costas_two_baseline_enabled: Option<bool> = None;
        let mut costas_partial_metric_enabled: Option<bool> = None;
        let mut relaxed_sync_near_partner_hz_radius: Option<f64> = None;
        let mut relaxed_sync_near_partner_score_delta: Option<f64> = None;
        let mut partner_freq_hz: Option<f64> = None;
        let mut cross_cycle_averaging: Option<bool> = None;
        let mut cross_cycle_coherent: Option<bool> = None;
        let mut cross_cycle_coherent_mrc: Option<bool> = None;
        let mut cross_cycle_content_guard: Option<f32> = None;
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
        let mut hb057_dt_history_enabled: Option<bool> = None;
        let mut hb057_dt_history_window_floor_s: Option<f64> = None;
        let mut hb057_dt_history_window_iqr_scale: Option<f64> = None;
        let mut hb057_dt_history_freq_window_hz: Option<f64> = None;
        let mut two_stage: Option<bool> = None;
        let mut ap_my_call: Option<String> = None;
        let mut ap_recent_calls: Option<Vec<String>> = None;
        let mut ap_rolling_window: Option<usize> = None;
        let mut fp_filter_baselines: Option<PathBuf> = None;
        let mut fp_filter_rolling: Option<usize> = None;
        let mut fp_filter_adif: Option<PathBuf> = None;
        let mut chrono_replay_enabled: Option<bool> = None;
        let mut chrono_replay_capacity: Option<usize> = None;
        let mut chrono_replay_manifest: Option<PathBuf> = None;
        let mut max_concurrent_tiers: Option<usize> = None;
        let mut max_concurrent_tiers_pool_dir: Option<PathBuf> = None;
        let mut noise_manifest: Option<PathBuf> = None;
        let mut synth_ft4_manifest: Option<PathBuf> = None;
        let mut effort_budget_ms: Option<u64> = None;
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
                "--linear-power-averaging" => {
                    // Task W3.5 [A/B]: turn on linear-power substep
                    // averaging. Unlike --sync-time-interp-linear-power,
                    // this does NOT imply sync_time_interpolation — the
                    // substep combine runs unconditionally on the
                    // always-active coarse-sync path.
                    linear_power_averaging = Some(true);
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
                "--pade-atanh" => {
                    pade_atanh = Some(true);
                }
                "--costas-half-loop-disabled" => {
                    costas_half_loop_disabled = Some(true);
                }
                "--escalation-enabled" => {
                    escalation_enabled = Some(true);
                }
                "--fine-fft-rect-window" => {
                    fine_fft_rect_window = Some(true);
                }
                "--fine-sync-enabled" => {
                    fine_sync_enabled = Some(true);
                }
                "--nsym-combining-enabled" => {
                    nsym_combining_enabled = Some(true);
                }
                "--per-candidate-freq-tracker-enabled" => {
                    per_candidate_freq_tracker_enabled = Some(true);
                }
                "--time-varying-subtraction-enabled" => {
                    time_varying_subtraction_enabled = Some(true);
                }
                "--full-scale-subtraction-enabled" => {
                    full_scale_subtraction_enabled = Some(true);
                }
                "--llr-whitening" => {
                    llr_whitening = Some(true);
                }
                "--no-llr-whitening" => {
                    llr_whitening = Some(false);
                }
                "--acceptance-gating" => {
                    acceptance_gating = Some(true);
                }
                "--no-acceptance-gating" => {
                    acceptance_gating = Some(false);
                }
                "--cq-ap" => {
                    cq_ap = Some(true);
                }
                "--no-cq-ap" => {
                    cq_ap = Some(false);
                }
                "--ap4-full-mask" => {
                    ap4_full_mask = Some(true);
                }
                "--no-ap4-full-mask" => {
                    ap4_full_mask = Some(false);
                }
                "--ap-post-normalize" => {
                    ap_post_normalize = Some(true);
                }
                "--no-ap-post-normalize" => {
                    ap_post_normalize = Some(false);
                }
                "--floor-iters" => {
                    floor_iters = Some(
                        iter.next()
                            .context("--floor-iters needs a value")?
                            .parse()?,
                    );
                }
                "--deep-iters" => {
                    deep_iters = Some(iter.next().context("--deep-iters needs a value")?.parse()?);
                }
                "--escalation-parity-max" => {
                    escalation_parity_max = Some(
                        iter.next()
                            .context("--escalation-parity-max needs a value")?
                            .parse()?,
                    );
                }
                "--per-bin-candidate-selection" => {
                    per_bin_candidate_selection = Some(true);
                }
                "--no-per-bin-candidate-selection" => {
                    per_bin_candidate_selection = Some(false);
                }
                "--costas-two-baseline-enabled" => {
                    costas_two_baseline_enabled = Some(true);
                }
                "--no-costas-two-baseline-enabled" => {
                    costas_two_baseline_enabled = Some(false);
                }
                "--costas-partial-metric-enabled" => {
                    costas_partial_metric_enabled = Some(true);
                }
                "--no-costas-partial-metric-enabled" => {
                    costas_partial_metric_enabled = Some(false);
                }
                "--relaxed-sync-near-partner-hz-radius" => {
                    relaxed_sync_near_partner_hz_radius = Some(
                        iter.next()
                            .context("--relaxed-sync-near-partner-hz-radius needs a value (Hz, e.g. 3.0)")?
                            .parse()?,
                    );
                }
                "--relaxed-sync-near-partner-score-delta" => {
                    relaxed_sync_near_partner_score_delta = Some(
                        iter.next()
                            .context("--relaxed-sync-near-partner-score-delta needs a value (signed dB delta, e.g. -1.5)")?
                            .parse()?,
                    );
                }
                "--partner-freq-hz" => {
                    partner_freq_hz = Some(
                        iter.next()
                            .context("--partner-freq-hz needs a value (Hz, e.g. 1500.0)")?
                            .parse()?,
                    );
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
                "--no-cross-cycle-coherent" => {
                    cross_cycle_coherent = Some(false);
                }
                "--cross-cycle-coherent-mrc" => {
                    cross_cycle_coherent = Some(true);
                    cross_cycle_coherent_mrc = Some(true);
                }
                "--cross-cycle-content-guard" => {
                    cross_cycle_content_guard = Some(
                        iter.next()
                            .context(
                                "--cross-cycle-content-guard needs a value \
                                 (LLR-sign correlation threshold in [-1,1])",
                            )?
                            .parse()?,
                    );
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
                "--hb057-dt-history-enabled" => {
                    hb057_dt_history_enabled = Some(true);
                }
                "--hb057-dt-history-window-floor-s" => {
                    hb057_dt_history_window_floor_s = Some(
                        iter.next()
                            .context("--hb057-dt-history-window-floor-s needs a value (seconds, e.g. 0.2)")?
                            .parse()?,
                    );
                }
                "--hb057-dt-history-window-iqr-scale" => {
                    hb057_dt_history_window_iqr_scale = Some(
                        iter.next()
                            .context(
                                "--hb057-dt-history-window-iqr-scale needs a value (e.g. 3.0)",
                            )?
                            .parse()?,
                    );
                }
                "--hb057-dt-history-freq-window-hz" => {
                    hb057_dt_history_freq_window_hz = Some(
                        iter.next()
                            .context(
                                "--hb057-dt-history-freq-window-hz needs a value (Hz, e.g. 25.0; 0.0 disables V2)",
                            )?
                            .parse()?,
                    );
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
                "--chrono-replay-enabled" => {
                    chrono_replay_enabled = Some(true);
                }
                "--no-chrono-replay" => {
                    chrono_replay_enabled = Some(false);
                }
                "--chrono-replay-capacity" => {
                    chrono_replay_capacity = Some(
                        iter.next()
                            .context("--chrono-replay-capacity needs a value (N; 0 = unbounded)")?
                            .parse()?,
                    );
                }
                "--chrono-replay-manifest" => {
                    chrono_replay_manifest = Some(
                        iter.next()
                            .context("--chrono-replay-manifest needs a path to a manifest JSON")?
                            .into(),
                    );
                }
                "--max-concurrent-tiers" => {
                    let raw = iter
                        .next()
                        .context("--max-concurrent-tiers needs a value (positive integer)")?;
                    let n: usize = raw.parse().with_context(|| {
                        format!("--max-concurrent-tiers value {raw:?} is not a positive integer")
                    })?;
                    anyhow::ensure!(
                        n >= 1,
                        "--max-concurrent-tiers must be >= 1 (got {n}); omit the flag to disable the guard entirely"
                    );
                    max_concurrent_tiers = Some(n);
                }
                "--max-concurrent-tiers-pool-dir" => {
                    max_concurrent_tiers_pool_dir = Some(
                        iter.next()
                            .context("--max-concurrent-tiers-pool-dir needs a path")?
                            .into(),
                    );
                }
                "--noise-manifest" => {
                    noise_manifest = Some(
                        iter.next()
                            .context("--noise-manifest needs a path to a manifest JSON")?
                            .into(),
                    );
                }
                "--synth-ft4-manifest" => {
                    synth_ft4_manifest = Some(
                        iter.next()
                            .context("--synth-ft4-manifest needs a path to a manifest JSON")?
                            .into(),
                    );
                }
                "--effort" => {
                    let v = iter
                        .next()
                        .context("--effort needs a value (eco|standard|deep|auto|unlimited)")?;
                    effort_budget_ms = effort_preset_budget_ms(&v)?;
                }
                "-h" | "--help" => {
                    eprintln!(
                        "usage: eval --tier <tiers,...> --mode <mode> --output <path> [--seed N] [--max-passes N] [--max-sync-candidates N] [--max-candidates N] [--osd-depth N|none] [--ldpc-iters N]"
                    );
                    eprintln!("  tiers: fixtures, synth-clean, synth-doppler, synth-ft4 (requires --mode ft4), synth-pair-200, curated-hard-200, curated-hard-1000, wild-50, wild-100, wild-doppler-50, hard-jt9-rich-200, lid-of-band, chrono-replay, noise_1000");
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
                    eprintln!("  --pade-atanh: F1 [A/B] — use the Padé rational approximant for atanh in the BP check-node update instead of the exact ln form (default off)");
                    eprintln!("  --costas-half-loop-disabled: F5 [A/B] — evaluate only half=0 in the Costas sync kernel's half-symbol inner loop instead of max(half=0, half=1) (default off)");
                    eprintln!("  --escalation-enabled: decoder-speed-overhaul Task 10 [A/B] — BP escalation ladder master switch (default off). See Ft8Config::escalation_enabled.");
                    eprintln!("  --fine-fft-rect-window: decoder-TP-sensitivity Task W1.3 [A/B] — rectangular (no) window on the fine-FFT fallback symbol FFT instead of Hann (default off). See Ft8Config::fine_fft_rect_window.");
                    eprintln!("  --fine-sync-enabled: decoder-TP-sensitivity Task W3.3 [A/B] — per-candidate fine-sync + matched-demod stage replacing the legacy 21-trial fine-FFT fallback (default off). See Ft8Config::fine_sync_enabled.");
                    eprintln!("  --nsym-combining-enabled: decoder-TP-sensitivity Task W3.4 [A/B] — nsym=2/3 noncoherent combining LLR variants on top of the W3.3 stage; requires --fine-sync-enabled to have any effect (default off). See Ft8Config::nsym_combining_enabled.");
                    eprintln!("  --per-candidate-freq-tracker-enabled: decoder-TP-sensitivity Task W3.6 [A/B] — re-test of the per-candidate frequency tracker as a consumer of the W3.3 matched-demod stage; requires --fine-sync-enabled to have any effect (default off). See Ft8Config::per_candidate_freq_tracker_enabled.");
                    eprintln!("  --time-varying-subtraction-enabled: decoder-TP-sensitivity Task W4.2/W4.3 [A/B] — per-block time-varying GFSK subtraction, replacing the legacy whole-signal fit; only observable when --max-passes >= 2 (default off). See Ft8Config::time_varying_subtraction_enabled.");
                    eprintln!("  --full-scale-subtraction-enabled: decoder-TP-sensitivity Task W4.2/W4.3 [A/B] — subtract the time-varying fit at full scale (1.0) instead of the legacy 0.9 hold-back; requires --time-varying-subtraction-enabled to have any effect (default off, UNTESTED on weak signals). See Ft8Config::full_scale_subtraction_enabled.");
                    eprintln!("  --linear-power-averaging: decoder-TP-sensitivity Task W3.5 [A/B] — combine each symbol's two TIME_OSR sub-steps in linear power instead of dB; runs unconditionally on the always-active coarse-sync path, independent of --sync-time-interp-linear-power (default off). See Ft8Config::linear_power_averaging.");
                    eprintln!("  --llr-whitening / --no-llr-whitening: decoder-TP-sensitivity Task W1.4 [A/B] — force the divisive LLR whitening step on/off (production default: off, flipped by the W1.4 A/B result). See Ft8Config::llr_whitening_enabled.");
                    eprintln!("  --acceptance-gating / --no-acceptance-gating: decoder-TP-sensitivity Task W2.5 [A/B] — replace the blunt post-CRC sync-score confidence floors with the W2.1 acceptance metric for decodes that cleanly pass it (production default: off). See Ft8Config::acceptance_gating_enabled.");
                    eprintln!("  --cq-ap / --no-cq-ap: decoder-TP-sensitivity Task W2.6 [A/B] — try ApLevel::Cq (assume a failed-AP0 candidate is a plain \"CQ\" call; no ApContext needed) on every candidate that reaches AP injection (production default: off). See Ft8Config::cq_ap_enabled.");
                    eprintln!("  --ap4-full-mask / --no-ap4-full-mask: decoder-TP-sensitivity Task W2.6 [A/B] — extend AP4 from a message-TYPE-only prior (i3=1) to a full message-CONTENT mask, trying the RR73/RRR/73 ir+igrid4 fields (production default: off). See Ft8Config::ap4_full_message_mask_enabled.");
                    eprintln!("  --ap-post-normalize / --no-ap-post-normalize: decoder-TP-sensitivity Task W2.6 [A/B] — normalize LLRs BEFORE injecting AP bits (not after) at every AP injection site, so the injected magnitude never distorts the channel-evidence scale (production default: off = inject-then-normalize). See Ft8Config::ap_injection_post_normalization.");
                    eprintln!("  --per-bin-candidate-selection / --no-per-bin-candidate-selection: decoder-TP-sensitivity Task W5.1 [A/B] — per-bin (freq_bin) top-K candidate thinning on the main Costas sweep, replacing the flat top-max_sync_candidates cap (production default: off, DECLINED — real regression on hard-200). See Ft8Config::per_bin_candidate_selection.");
                    eprintln!("  --costas-two-baseline-enabled / --no-costas-two-baseline-enabled: decoder-TP-sensitivity Task W5.2 [A/B] — percentile-normalized wide-lag two-baseline (tight+wide) sync candidate emission (production default: off). See Ft8Config::costas_two_baseline_enabled.");
                    eprintln!("  --costas-partial-metric-enabled / --no-costas-partial-metric-enabled: decoder-TP-sensitivity Task W5.4 [A/B] — sync_bc partial-Costas (blocks B+C only) parallel score, rescuing slot-edge negative-dt candidates whose block A falls outside the window (production default: off). See Ft8Config::costas_partial_metric_enabled.");
                    eprintln!("  --relaxed-sync-near-partner-hz-radius V / --relaxed-sync-near-partner-score-delta V: decoder-TP-sensitivity Task W5.4 [A/B] — JTDX-style relaxed Costas acceptance threshold inside ±V Hz of the QSO partner audio freq (production default: radius=None, delta=0.0, i.e. inert). See Ft8Config::relaxed_sync_near_partner_hz_radius / _score_delta.");
                    eprintln!("  --partner-freq-hz V: [HARNESS] forwards a fixed partner audio frequency (Hz) to every decode_wav call in the tier, simulating an active QSO parked at V for the whole run. Only has an effect when --relaxed-sync-near-partner-hz-radius is also set.");
                    eprintln!("  --floor-iters N: shallow BP iteration count for S1/S2 when --escalation-enabled (default 25).");
                    eprintln!("  --deep-iters N: deep BP iteration count a near-miss floor failure is escalated to (default 100).");
                    eprintln!("  --escalation-parity-max N: max unsatisfied parity checks at floor_iters tolerated before escalating (default 30).");
                    eprintln!("  --max-concurrent-tiers N: opt-in CPU-contention guard. Heavy tiers (hard-200/1000, chrono-replay, wild-*, hard-jt9-rich-200) acquire one of N file-lock slots in /tmp/pancetta-eval-tier-slots/ before running. Default unbounded (no guard).");
                    eprintln!("  --max-concurrent-tiers-pool-dir PATH: override the slot-pool directory (default /tmp/pancetta-eval-tier-slots).");
                    eprintln!("  --noise-manifest PATH: override the noise_1000 tier's manifest (default research/corpus/curated/noise/noise_1000.manifest.json).");
                    eprintln!("  --synth-ft4-manifest PATH: override the synth-ft4 tier's manifest (default research/corpus/synth/manifests/ft4_clean.manifest.json).");
                    eprintln!("  --effort <eco|standard|deep|auto|unlimited>: Task W3.3b [HARNESS] — construct the decoder's DecodeBudget from a real production effort preset (eco=1ms, standard=250ms, deep=1000ms, auto=1000ms [Fast-tier assumption; no live hardware probe here], unlimited=DecodeBudget::unlimited()) instead of always using an unlimited budget. Default (flag omitted): unlimited, byte-identical to every prior eval invocation. Mirrors pancetta/src/coordinator/effort.rs::preset_budget_ms.");
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
            max_parity_errors_for_osd,
            sync_time_interpolation,
            sync_time_interp_score_gate,
            sync_time_interp_delta_scale,
            sync_time_interp_max_delta_abs,
            sync_time_interp_linear_power,
            linear_power_averaging,
            bp_offset_subtract,
            layered_bp,
            pade_atanh,
            costas_half_loop_disabled,
            escalation_enabled,
            fine_fft_rect_window,
            fine_sync_enabled,
            nsym_combining_enabled,
            per_candidate_freq_tracker_enabled,
            time_varying_subtraction_enabled,
            full_scale_subtraction_enabled,
            llr_whitening,
            acceptance_gating,
            cq_ap,
            ap4_full_mask,
            ap_post_normalize,
            floor_iters,
            deep_iters,
            escalation_parity_max,
            per_bin_candidate_selection,
            costas_two_baseline_enabled,
            costas_partial_metric_enabled,
            relaxed_sync_near_partner_hz_radius,
            relaxed_sync_near_partner_score_delta,
            partner_freq_hz,
            cross_cycle_averaging,
            cross_cycle_coherent,
            cross_cycle_coherent_mrc,
            cross_cycle_content_guard,
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
            hb057_dt_history_enabled,
            hb057_dt_history_window_floor_s,
            hb057_dt_history_window_iqr_scale,
            hb057_dt_history_freq_window_hz,
            two_stage,
            ap_my_call,
            ap_recent_calls,
            ap_rolling_window,
            fp_filter_baselines,
            fp_filter_rolling,
            fp_filter_adif,
            chrono_replay_enabled,
            chrono_replay_capacity,
            chrono_replay_manifest,
            max_concurrent_tiers,
            max_concurrent_tiers_pool_dir,
            noise_manifest,
            synth_ft4_manifest,
            effort_budget_ms,
        })
    }
}

/// Task W3.3b [HARNESS]: effort-preset name → per-window wall-clock decode
/// budget in milliseconds, mirroring the production mapping in
/// `pancetta/src/coordinator/effort.rs::preset_budget_ms` (decoder-speed-
/// overhaul Task 14: `Eco`=1, `Standard`=250, `Deep`=1000, `Max`=unlimited).
///
/// This is a deliberate, documented LOCAL RE-DERIVATION rather than a
/// cross-crate call into that function: `preset_budget_ms` is `pub(crate)`
/// inside the `pancetta` binary crate (not `pub`, and its `coordinator`
/// submodule tree is private too), and reaching it would require adding
/// `pancetta` — which pulls in axum, tokio-tungstenite, cpal, the hamlib
/// FFI bindings, the TUI, etc. — as a dependency of `pancetta-research`
/// just to reach one 4-arm match statement. That is a materially larger
/// dependency-graph change than this harness-only CLI flag warrants (see
/// task W3.3b's brief: "a well-justified local re-derivation of just the
/// preset->ms numbers, cited from the production source, is an acceptable
/// fallback"). **KEEP IN SYNC** with `preset_budget_ms` if the production
/// constants ever change.
///
/// `auto` here assumes `HardwareTier::Fast` (mirrors the coordinator's own
/// "innocent until proven otherwise" startup assumption in
/// `coordinator/tier.rs`/`coordinator/mod.rs`'s pre-probe seeding) — the
/// eval harness has no live hardware-tier probe, and probing would make
/// scorecards host-dependent and non-reproducible. `unlimited` returns
/// `None`, which `Ft8Decoder::decode_budget()` maps to
/// `DecodeBudget::unlimited()` — the harness's pre-existing, and default,
/// behavior.
fn effort_preset_budget_ms(name: &str) -> anyhow::Result<Option<u64>> {
    Ok(match name {
        "unlimited" => None,
        "eco" => Some(1),
        "standard" => Some(250),
        "deep" => Some(1000),
        "auto" => Some(1000),
        other => anyhow::bail!(
            "--effort: unknown preset {other:?} (expected eco|standard|deep|auto|unlimited)"
        ),
    })
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

/// Task W0.2: SHA-256 of a file, used to key into the `baseline` binary's
/// jt9-decode cache at `research/baselines/ft8/<sha>.json`. Mirrors the
/// same tiny private helper already duplicated in `bin/baseline.rs`,
/// `gen_noise.rs`, `bin/curate.rs`, `bin/curate_chrono_replay.rs` — an
/// existing (if not ideal) convention in this crate, kept rather than
/// introduced fresh.
fn sha256_file(path: &std::path::Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Task W0.2: look up the jt9 oracle's cached decodes for `wav_path` (from
/// `cargo run -p pancetta-research --bin baseline -- --tier synth
/// --synth-manifest <manifest>`) and report whether any decode matches
/// `encoded_message`. Returns `None` if no cache exists for this WAV (jt9
/// oracle not yet run over this corpus) — the caller must distinguish
/// "no jt9 data" from "jt9 attempted and missed".
///
/// Task W0.4 (2026-07-07): takes `mode` to select the baseline cache
/// subdirectory (`research/baselines/<mode>/`, matching how `bin/baseline.rs`
/// keys its cache — see `cache_path`), so the new FT4 tier reads the FT4
/// jt9 oracle cache instead of FT8's.
fn jt9_recovered(
    workspace: &std::path::Path,
    wav_path: &std::path::Path,
    encoded_message: &str,
    mode: Mode,
) -> Option<bool> {
    let sha = sha256_file(wav_path).ok()?;
    let cache_path = workspace
        .join("research/baselines")
        .join(mode.as_str())
        .join(format!("{sha}.json"));
    if !cache_path.exists() {
        return None;
    }
    let s = std::fs::read_to_string(&cache_path).ok()?;
    let cache: serde_json::Value = serde_json::from_str(&s).ok()?;
    let decodes = cache.get("decodes")?.as_array()?;
    Some(decodes.iter().any(|d| {
        d.get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.contains(encoded_message))
            .unwrap_or(false)
    }))
}

fn run_synth_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
    mode: Mode,
) -> anyhow::Result<TierResult> {
    let entries = load_synth_corpus(workspace, manifest_path)?;
    // Group by snr_db bin.
    let mut bins: BTreeMap<i64, (u32, u32)> = BTreeMap::new(); // key = snr*10 to avoid float keys
                                                               // Task W0.2: parallel jt9-oracle bins, keyed identically, populated
                                                               // only when a jt9 baseline cache exists for that WAV (see
                                                               // `jt9_recovered`). If ANY entry in the tier is missing a cache, the
                                                               // jt9 curve is reported empty rather than silently partial — a
                                                               // partial curve with the SAME bin structure as the pancetta curve
                                                               // could be misread as "jt9 covers the whole corpus" when it doesn't.
    let mut jt9_bins: BTreeMap<i64, (u32, u32)> = BTreeMap::new();
    let mut jt9_baseline_missing = false;
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

        if !jt9_baseline_missing {
            let jt9_bin = jt9_bins.entry(bin_key).or_insert((0, 0));
            jt9_bin.0 += 1;
            match jt9_recovered(workspace, &e.wav_path, &e.encoded_message, mode) {
                Some(true) => jt9_bin.1 += 1,
                Some(false) => {}
                None => jt9_baseline_missing = true,
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
    // Find SNR @ 50% and 90% recovery (linear interpolation between the
    // straddling bins — Task W0.2, see `first_threshold_db`'s doc).
    let snr_at_50 = first_threshold_db(&by_snr, 0.50);
    let snr_at_90 = first_threshold_db(&by_snr, 0.90);
    let ttfd_distribution = TtfdDistribution::from_per_wav(per_wav_ttfd_s);

    let (jt9_snr_curve, jt9_snr_at_50pct_recovery_db) = if jt9_baseline_missing {
        eprintln!(
            "synth tier: jt9 baseline cache missing for at least one WAV — jt9_snr_curve left \
             empty. Run `cargo run --release -p pancetta-research --bin baseline -- --tier synth \
             --mode {mode} --synth-manifest {}` first to populate research/baselines/{}/.",
            manifest_path.display(),
            mode.as_str(),
        );
        (Vec::new(), None)
    } else {
        let mut curve: Vec<SnrBin> = jt9_bins
            .iter()
            .map(|(k, (attempts, decoded))| SnrBin {
                snr_db: (*k as f64) / 10.0,
                attempts: *attempts,
                decoded: *decoded,
                fp: 0,
            })
            .collect();
        curve.sort_by(|a, b| a.snr_db.partial_cmp(&b.snr_db).unwrap());
        let at_50 = first_threshold_db(&curve, 0.50);
        (curve, at_50)
    };

    Ok(TierResult {
        wavs_processed,
        by_snr_db: by_snr,
        snr_at_50pct_recovery_db: snr_at_50,
        snr_at_90pct_recovery_db: snr_at_90,
        ttfd_distribution,
        jt9_snr_curve,
        jt9_snr_at_50pct_recovery_db,
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

/// Chronological-replay tier (2026-06-01) — processes WAVs in
/// `slot_index` order from a [`pancetta_research::chrono_replay::ChronoReplayManifest`].
///
/// Stateful semantics: unlike `run_curated_tier`, the decoder's persistent
/// state (chrono_replay deque, dt_history, etc.) IS NOT reset between
/// WAVs. The caller MUST construct the `decoder` with stateful mode
/// enabled (auto-enabled when `--tier chrono-replay` is requested; see
/// the dispatch in `main`).
///
/// Per-WAV scoring is identical to the curated tier (recovered vs jt9
/// baseline + novels). The differences are:
/// - manifest ordering is the ground truth (we do NOT sort or reshuffle);
/// - between-WAV decoder state is preserved (rolling deque grows);
/// - a per-tier snapshot-growth diagnostic is logged.
fn run_chrono_replay_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
    novel_classifier: Option<&pancetta_research::FpFilter>,
) -> anyhow::Result<TierResult> {
    use pancetta_research::chrono_replay::load_chrono_replay_corpus;
    let entries = load_chrono_replay_corpus(manifest_path)?;
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
    // Task W0.3: see the identical counters in `run_curated_tier` — same
    // report-only semantics.
    let mut novels_verified = 0u32;
    let mut novels_unverified = 0u32;
    let mut wsjtx_total = 0u32;
    let mut per_wav_failures: Vec<PerWavFailure> = Vec::new();
    let mut per_wav_records: Vec<PerWavRecord> = Vec::new();
    let mut per_wav_ttfd_s: Vec<f64> = Vec::new();

    // Snapshot-growth diagnostic: read the decoder's chrono-replay
    // snapshot length before/after each WAV. Confirms statefulness —
    // a stateless tier returns the same length (0 or None) at every
    // observation; a stateful tier shows monotonic growth.
    let snapshot_at_start = decoder.chrono_replay_snapshot_len();
    let mut snapshot_growth_log: Vec<(usize, usize)> = Vec::new();
    eprintln!(
        "chrono-replay: snapshot at tier start = {:?}",
        snapshot_at_start
    );
    for (i, entry) in entries.iter().enumerate() {
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
            Vec::new()
        };
        wsjtx_total += baseline_decodes.len() as u32;
        truth_decodes_total += baseline_decodes.len() as u32;

        let mut our_decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        apply_fp_filter(fp_filter, &mut our_decodes);

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

        let mut novel_here = 0u32;
        for ours in &our_decodes {
            if !baseline_decodes
                .iter()
                .any(|t| t.trim() == ours.message.trim())
            {
                novel_decodes += 1;
                novel_here += 1;
                // Task W0.3: report-only classification, see run_curated_tier.
                if let Some(classifier) = novel_classifier {
                    if classifier.classify(&ours.message) {
                        novels_verified += 1;
                    } else {
                        novels_unverified += 1;
                    }
                }
            }
        }

        let gap = baseline_decodes.len() as i64 - recovered_here as i64;
        if gap > 0 {
            per_wav_failures.push(PerWavFailure {
                wav_hash: entry.wav_sha256.clone(),
                truth: baseline_decodes.len() as u32,
                recovered: recovered_here,
                wsjtx: baseline_decodes.len() as u32,
                jtdx: 0,
            });
        }

        per_wav_records.push(PerWavRecord {
            wav_hash: entry.wav_sha256.clone(),
            truth: baseline_decodes.len() as u32,
            recovered: recovered_here,
            novel: novel_here,
        });

        // Statefulness probe: record snapshot length after this WAV.
        if let Some(len) = decoder.chrono_replay_snapshot_len() {
            snapshot_growth_log.push((i, len));
        }

        if i < 5 || (i + 1) % 50 == 0 || i + 1 == entries.len() {
            let snap_info = decoder
                .chrono_replay_snapshot_len()
                .map(|n| format!(" snapshot={n}"))
                .unwrap_or_default();
            eprintln!(
                "chrono-replay: slot {}/{} ({}): {} truth, {} recovered, {} novel{}",
                i + 1,
                entries.len(),
                entry
                    .wav_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?"),
                baseline_decodes.len(),
                recovered_here,
                novel_here,
                snap_info,
            );
        }
    }

    // Statefulness summary: report snapshot growth across the tier.
    if !snapshot_growth_log.is_empty() {
        let (_, final_len) = snapshot_growth_log.last().copied().unwrap_or((0, 0));
        let monotonic = snapshot_growth_log.windows(2).all(|w| w[1].1 >= w[0].1);
        eprintln!(
            "chrono-replay: STATEFULNESS final snapshot={} callsigns, monotonic-growth={}, samples={}",
            final_len,
            monotonic,
            snapshot_growth_log.len(),
        );
    } else {
        eprintln!(
            "chrono-replay: STATEFULNESS no chrono_replay_snapshot_len reported by decoder \
             (stateless mode — verify --chrono-replay-enabled or chrono-replay tier is in --tiers)"
        );
    }

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
            "chrono-replay tier TTFD: n={} wavs, p50={:.3}s p90={:.3}s mean={:.3}s",
            ttfd.wavs_with_decode, ttfd.p50_seconds, ttfd.p90_seconds, ttfd.mean_seconds,
        );
    }

    Ok(TierResult {
        wavs_processed: total,
        truth_decodes_total: Some(truth_decodes_total),
        truth_decodes_recovered: Some(truth_recovered),
        decode_rate: Some(decode_rate),
        novel_decodes: Some(novel_decodes),
        novels_verified: novel_classifier.map(|_| novels_verified),
        novels_unverified: novel_classifier.map(|_| novels_unverified),
        wsjtx_decoded: Some(wsjtx_total),
        vs_wsjtx_pct: Some(vs_wsjtx_pct),
        per_wav_top_failures: per_wav_failures,
        per_wav_records,
        ttfd_distribution,
        ..Default::default()
    })
}

fn run_curated_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
    manifest_path: &std::path::Path,
    fp_filter: Option<&pancetta_research::FpFilter>,
    novel_classifier: Option<&pancetta_research::FpFilter>,
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
    // Task W0.3: report-only classification of novel decodes (does NOT
    // filter/drop anything — see `novel_classifier` doc at the call site
    // in `main`). `None` unless a classifier was built for this run.
    let mut novels_verified = 0u32;
    let mut novels_unverified = 0u32;
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
                // Task W0.3: classify (never filter) this novel via the
                // existing callsign-continuity logic. Purely additive —
                // `our_decodes`/`novel_decodes`/`recovered_here` etc. are
                // untouched by this branch.
                if let Some(classifier) = novel_classifier {
                    if classifier.classify(&ours.message) {
                        novels_verified += 1;
                    } else {
                        novels_unverified += 1;
                    }
                }
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
        novels_verified: novel_classifier.map(|_| novels_verified),
        novels_unverified: novel_classifier.map(|_| novels_unverified),
        wsjtx_decoded: Some(wsjtx_total),
        vs_wsjtx_pct: Some(vs_wsjtx_pct),
        per_wav_top_failures: per_wav_failures,
        per_wav_records,
        ttfd_distribution,
        ..Default::default()
    })
}

/// Workstream 0 (2026-07-06) — FP-on-noise tier. Every WAV in this corpus
/// is seeded white Gaussian noise (+ optional birdie interference) with
/// NO FT8 signal present. Any message the decoder under test returns is,
/// by construction, a false positive — this tier is the harness's first
/// guardrail against a hallucinating decoder scoring identically to a
/// correct one (design spec §2, decision D0(a)).
fn run_noise_tier(
    decoder: &dyn DecoderUnderTest,
    manifest_path: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let entries = pancetta_research::gen_noise::load_noise_corpus(manifest_path)?;
    let total = entries.len() as u32;
    if total == 0 {
        return Ok(TierResult {
            wavs_processed: 0,
            ..Default::default()
        });
    }
    let mut false_positives_total: u32 = 0;
    let mut noise_files_decoded: u32 = 0;
    for entry in &entries {
        let decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        if !decodes.is_empty() {
            noise_files_decoded += 1;
            false_positives_total += decodes.len() as u32;
        }
    }
    if false_positives_total > 0 {
        eprintln!(
            "noise_1000: {false_positives_total} FALSE POSITIVE decode(s) across {noise_files_decoded}/{total} noise-only WAVs \
             — the decoder is hallucinating on signal-free audio. This must be 0 for a healthy decoder."
        );
    } else {
        eprintln!("noise_1000: 0 false positives across {total} noise-only WAVs.");
    }
    Ok(TierResult {
        wavs_processed: total,
        false_positives_total: Some(false_positives_total),
        noise_files_decoded: Some(noise_files_decoded),
        ..Default::default()
    })
}

/// SNR (in dB) where recovery crosses `threshold`, via **linear
/// interpolation** between the two SNR bins straddling it (Task W0.2,
/// 2026-07-06) — not "first bin >= threshold", which quantizes the
/// reported number to the corpus's step size and can visibly move by a
/// full step for a one-file recall change near a bin boundary.
///
/// Bins must be sorted by `snr_db` ascending (as `run_synth_tier` already
/// sorts `by_snr`); bins with zero attempts are skipped entirely (neither
/// bound an interpolation nor count as "reached").
///
/// - If the very first bin with attempts already meets `threshold`,
///   there's no lower bin to interpolate from — returns that bin's
///   `snr_db` as-is (can't report a value below the corpus's own range).
/// - If no bin ever reaches `threshold`, returns `None`.
fn first_threshold_db(bins: &[SnrBin], threshold: f64) -> Option<f64> {
    let valid: Vec<(f64, f64)> = bins
        .iter()
        .filter(|b| b.attempts > 0)
        .map(|b| (b.snr_db, b.decoded as f64 / b.attempts as f64))
        .collect();
    let (first_snr, first_recall) = *valid.first()?;
    if first_recall >= threshold {
        return Some(first_snr);
    }
    for window in valid.windows(2) {
        let (snr_lo, recall_lo) = window[0];
        let (snr_hi, recall_hi) = window[1];
        if recall_lo < threshold && recall_hi >= threshold {
            if (recall_hi - recall_lo).abs() < f64::EPSILON {
                return Some(snr_hi);
            }
            let frac = (threshold - recall_lo) / (recall_hi - recall_lo);
            return Some(snr_lo + frac * (snr_hi - snr_lo));
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

/// Task W0.4 (2026-07-07): build the `Ft8Decoder` wrapper from CLI args for
/// a given protocol. Factored out of `main`'s old `match args.mode { Mode::Ft8
/// => { .. } }` arm so adding `Mode::Ft4` (FT4 evaluation tier) didn't
/// require duplicating this ~190-line CLI-knob wiring chain — every knob here
/// is a generic `Ft8Config` field, not FT8-specific, so FT8 and FT4 share it
/// verbatim; only the wrapped `Ft8Config::protocol` differs.
fn build_decoder_from_args(args: &Args, protocol: pancetta_ft8::Protocol) -> Ft8Decoder {
    let mut d = Ft8Decoder::with_default_config().with_protocol(protocol);
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
    if let Some(on) = args.linear_power_averaging {
        d = d.with_linear_power_averaging(on);
    }
    if let Some(v) = args.bp_offset_subtract {
        d = d.with_bp_offset_subtract(v);
    }
    if let Some(on) = args.layered_bp {
        d = d.with_layered_bp(on);
    }
    if let Some(on) = args.pade_atanh {
        d = d.with_pade_atanh(on);
    }
    if let Some(on) = args.costas_half_loop_disabled {
        d = d.with_costas_half_loop_disabled(on);
    }
    if let Some(on) = args.escalation_enabled {
        d = d.with_escalation_enabled(on);
    }
    if let Some(on) = args.fine_fft_rect_window {
        d = d.with_fine_fft_rect_window(on);
    }
    if let Some(on) = args.fine_sync_enabled {
        d = d.with_fine_sync_enabled(on);
    }
    if let Some(on) = args.nsym_combining_enabled {
        d = d.with_nsym_combining_enabled(on);
    }
    if let Some(on) = args.per_candidate_freq_tracker_enabled {
        d = d.with_per_candidate_freq_tracker_enabled(on);
    }
    if let Some(on) = args.time_varying_subtraction_enabled {
        d = d.with_time_varying_subtraction_enabled(on);
    }
    if let Some(on) = args.full_scale_subtraction_enabled {
        d = d.with_full_scale_subtraction_enabled(on);
    }
    if let Some(on) = args.llr_whitening {
        d = d.with_llr_whitening(on);
    }
    if let Some(on) = args.acceptance_gating {
        d = d.with_acceptance_gating_enabled(on);
    }
    if let Some(on) = args.cq_ap {
        d = d.with_cq_ap_enabled(on);
    }
    if let Some(on) = args.ap4_full_mask {
        d = d.with_ap4_full_message_mask_enabled(on);
    }
    if let Some(on) = args.ap_post_normalize {
        d = d.with_ap_injection_post_normalization(on);
    }
    if let Some(v) = args.floor_iters {
        d = d.with_floor_iters(v);
    }
    if let Some(v) = args.deep_iters {
        d = d.with_deep_iters(v);
    }
    if let Some(v) = args.escalation_parity_max {
        d = d.with_escalation_parity_max(v);
    }
    if let Some(on) = args.per_bin_candidate_selection {
        d = d.with_per_bin_candidate_selection(on);
    }
    if let Some(on) = args.costas_two_baseline_enabled {
        d = d.with_costas_two_baseline_enabled(on);
    }
    if let Some(on) = args.costas_partial_metric_enabled {
        d = d.with_costas_partial_metric_enabled(on);
    }
    if args.relaxed_sync_near_partner_hz_radius.is_some()
        || args.relaxed_sync_near_partner_score_delta.is_some()
    {
        let radius = args.relaxed_sync_near_partner_hz_radius.unwrap_or(3.0);
        let delta = args.relaxed_sync_near_partner_score_delta.unwrap_or(0.0);
        d = d.with_relaxed_sync_near_partner(radius, delta);
    }
    if let Some(hz) = args.partner_freq_hz {
        d = d.with_partner_freq_hz(hz);
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
    if args.cross_cycle_content_guard.is_some() {
        d = d.with_cross_cycle_content_guard(args.cross_cycle_content_guard);
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
    if matches!(args.hb057_dt_history_enabled, Some(true)) {
        let floor = args.hb057_dt_history_window_floor_s.unwrap_or(0.2);
        let scale = args.hb057_dt_history_window_iqr_scale.unwrap_or(3.0);
        d = d.with_dt_history(floor, scale);
        if let Some(hz) = args.hb057_dt_history_freq_window_hz {
            d = d.with_dt_history_freq_window_hz(hz);
        }
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
                            eprintln!("warning: --ap-recent-calls entry {c:?} did not encode");
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
    // Chronological-replay tier (2026-06-01): when the user
    // requests the `chrono-replay` tier, stateful mode is
    // mandatory (the tier's whole point). Auto-enable here so the
    // operator doesn't have to remember a redundant flag — but
    // also expose `--chrono-replay-enabled` for explicit
    // hard-200/hard-1000 dispatch where the tier name doesn't
    // imply statefulness (e.g. ad-hoc combinations).
    let chrono_tier_requested = args.tiers.iter().any(|t| t == "chrono-replay");
    if chrono_tier_requested || args.chrono_replay_enabled.unwrap_or(false) {
        let cap = args.chrono_replay_capacity.unwrap_or(0);
        let (d2, _state) = d.with_chrono_replay(cap);
        d = d2;
    }
    // Task W3.3b [HARNESS]: only touched when --effort is passed; omitting
    // the flag leaves `effort_budget_ms` at its parse-time default (None)
    // and this branch never runs, so `with_effort_budget_ms` is never
    // called and the decoder's `budget_ms` stays at its own default
    // (`None`) — byte-identical to every pre-existing eval invocation.
    if args.effort_budget_ms.is_some() {
        d = d.with_effort_budget_ms(args.effort_budget_ms);
    }

    d
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
    // Task W0.4 (2026-07-07): the decoder-construction knob chain lives in
    // build_decoder_from_args (shared verbatim between FT8 and FT4 --
    // only Ft8Config::protocol differs).
    let protocol = match args.mode {
        Mode::Ft8 => pancetta_ft8::Protocol::Ft8,
        Mode::Ft4 => pancetta_ft8::Protocol::Ft4,
    };
    let decoder: Box<dyn DecoderUnderTest> = Box::new(build_decoder_from_args(&args, protocol));

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

    // Task W0.3 (2026-07-06): report-only novel-decode classifier. This is
    // DELIBERATELY independent of `fp_filter` above (which is opt-in and
    // actually drops decodes via `apply_fp_filter`) — it always builds
    // when a jt9-truth tier is requested, from the SAME jt9-baseline
    // corpus (`research/baselines/ft8`) already used to look up truth for
    // every WAV in those tiers, and is used ONLY to classify (never
    // filter) pancetta-only ("novel") decodes as verified/unverified for
    // scorecard accounting (design spec §2, decision D0(c)). See
    // `run_curated_tier` / `run_chrono_replay_tier` for where it's
    // consulted, and `fp_filter.rs::FpFilter::classify` for the
    // report-only entry point.
    let novel_classifier_needed = args.tiers.iter().any(|t| {
        matches!(
            t.as_str(),
            "curated-hard-200"
                | "curated-hard-1000"
                | "wild-50"
                | "wild-100"
                | "lid-of-band"
                | "wild-doppler-50"
                | "hard-jt9-rich-200"
                | "chrono-replay"
        )
    });
    let novel_classifier: Option<pancetta_research::FpFilter> = if novel_classifier_needed {
        let dir = workspace.join("research/baselines/ft8");
        if dir.exists() {
            let mut f = pancetta_research::FpFilter::new();
            let n = f.extend_from_baselines(&dir).with_context(|| {
                format!("loading novel-classifier baselines from {}", dir.display())
            })?;
            eprintln!(
                "novel-classifier: loaded {n} jt9 baseline files from {}, {} unique callsigns \
                 (report-only — does not filter decodes)",
                dir.display(),
                f.reference_size()
            );
            Some(f)
        } else {
            eprintln!(
                "novel-classifier: baselines dir {} missing — novels_verified/novels_unverified \
                 will be omitted (None) for this run",
                dir.display()
            );
            None
        }
    } else {
        None
    };
    let novel_classifier_ref = novel_classifier.as_ref();

    // Batch 19 (2026-06-02): build a tier-slot pool when --max-concurrent-tiers
    // is set so heavy tier runs across N parallel `eval` invocations don't
    // co-saturate CPU. The pool is a no-op for light tiers (fixtures,
    // synth-*); see pancetta_research::tier_slots::is_heavy_tier.
    let tier_slot_pool: Option<pancetta_research::TierSlotPool> = if let Some(n) =
        args.max_concurrent_tiers
    {
        let dir = args
            .max_concurrent_tiers_pool_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(pancetta_research::DEFAULT_POOL_DIR));
        let pool = pancetta_research::TierSlotPool::new(&dir, n)
            .with_context(|| format!("creating tier-slot pool (size={n}) at {}", dir.display()))?;
        eprintln!(
            "tier-slot: pool active (size={}, dir={}); heavy tiers will acquire one slot before running",
            pool.size(),
            pool.dir().display(),
        );
        Some(pool)
    } else {
        None
    };

    let mut tiers = BTreeMap::new();
    for tier_name in &args.tiers {
        // Acquire a slot before any heavy tier dispatches. Light tiers
        // (fixtures, synth-*) skip the gate. The guard releases on drop
        // at the end of this loop iteration (after `tiers.insert`).
        let _tier_slot = match (
            tier_slot_pool.as_ref(),
            pancetta_research::is_heavy_tier(tier_name),
        ) {
            (Some(pool), true) => Some(
                pool.acquire(tier_name)
                    .with_context(|| format!("acquiring tier slot for heavy tier {tier_name}"))?,
            ),
            _ => None,
        };
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
                let result = run_synth_tier(
                    decoder.as_ref(),
                    &workspace,
                    &manifest,
                    fp_filter_ref,
                    Mode::Ft8,
                )?;
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
                let result = run_synth_tier(
                    decoder.as_ref(),
                    &workspace,
                    &manifest,
                    fp_filter_ref,
                    Mode::Ft8,
                )?;
                tiers.insert("synth-doppler".to_string(), result);
            }
            // Task W0.4 (2026-07-07): FT4 evaluation tier — the first-ever
            // measured FT4 decode sensitivity. Same synth-clean shape
            // (1 dB steps, n=50/step, 2500 Hz reference-bandwidth
            // convention) as `synth-clean`, generated at FT4's 7.5s slot
            // via `gen-synth --config ft4_clean.config.json` (the config's
            // `"mode": "ft4"` selects FT4 protocol params + slot geometry).
            // Requires `--mode ft4` so `build_decoder_from_args` wraps the
            // decoder with `Protocol::Ft4`, and the jt9 oracle cache under
            // `research/baselines/ft4/` (from `bin/baseline --mode ft4`).
            "synth-ft4" => {
                anyhow::ensure!(
                    matches!(args.mode, Mode::Ft4),
                    "synth-ft4 tier requires --mode ft4 (got {}) — the decoder must be \
                     constructed with Protocol::Ft4 to decode FT4 audio",
                    args.mode
                );
                let manifest = args.synth_ft4_manifest.clone().unwrap_or_else(|| {
                    workspace.join("research/corpus/synth/manifests/ft4_clean.manifest.json")
                });
                let manifest = if manifest.is_absolute() {
                    manifest
                } else {
                    workspace.join(&manifest)
                };
                anyhow::ensure!(
                    manifest.exists(),
                    "FT4 synth manifest missing at {}; run `cargo run --release -p pancetta-research --bin gen-synth -- --config research/corpus/synth/manifests/ft4_clean.config.json --output research/corpus/synth/manifests/ft4_clean.manifest.json`",
                    manifest.display()
                );
                let result = run_synth_tier(
                    decoder.as_ref(),
                    &workspace,
                    &manifest,
                    fp_filter_ref,
                    Mode::Ft4,
                )?;
                tiers.insert("synth-ft4".to_string(), result);
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
            "curated-hard-200" | "curated-hard-1000" | "wild-50" | "wild-100" | "lid-of-band" => {
                let label = match tier_name.as_str() {
                    "curated-hard-200" => "hard_200",
                    "curated-hard-1000" => "hard_1000",
                    "wild-50" => "wild_50",
                    "wild-100" => "wild_100",
                    // hb-156 (Batch 29): weak-signal-only subset filtered from
                    // hard_200 + wild_100 by per-truth jt9 SNR ≤ -19 dB. Each
                    // entry annotated with `min_truth_snr_db` and
                    // `n_truths_at_or_below_threshold`; tier evaluates pancetta
                    // on the operationally hardest slice of FT8 decoding.
                    "lid-of-band" => "lid_of_band",
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
                let result = run_curated_tier(
                    decoder.as_ref(),
                    &workspace,
                    &manifest,
                    fp_filter_ref,
                    novel_classifier_ref,
                )?;
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
                    let result = run_curated_tier(
                        decoder.as_ref(),
                        &workspace,
                        &manifest,
                        fp_filter_ref,
                        novel_classifier_ref,
                    )?;
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
                    let result = run_curated_tier(
                        decoder.as_ref(),
                        &workspace,
                        &manifest,
                        fp_filter_ref,
                        novel_classifier_ref,
                    )?;
                    tiers.insert("hard-jt9-rich-200".to_string(), result);
                }
            }
            // Chronological-replay tier (2026-06-01): stateful cross-WAV
            // semantics — the decoder's `chrono_replay_state` persists
            // across consecutive WAVs, so callsigns decoded in slot N
            // are available to slot N+1's AP path. Unblocks future
            // re-tests of hb-048 a7, hb-057 median-DT, and hb-173
            // within-QSO (all SHELVED on the per-WAV-isolated curated
            // tiers; root cause was empty cross-slot snapshot).
            "chrono-replay" => {
                let manifest = args.chrono_replay_manifest.clone().unwrap_or_else(|| {
                    workspace.join("research/corpus/curated/ft8/chrono_replay.manifest.json")
                });
                let manifest = if manifest.is_absolute() {
                    manifest
                } else {
                    workspace.join(&manifest)
                };
                if !manifest.exists() {
                    eprintln!(
                        "tier chrono-replay: manifest missing at {} — SKIPPING (run `cargo run --release -p pancetta-research --bin curate-chrono-replay` to generate)",
                        manifest.display()
                    );
                    tiers.insert(
                        "chrono-replay".to_string(),
                        TierResult {
                            wavs_processed: 0,
                            ..Default::default()
                        },
                    );
                } else {
                    let result = run_chrono_replay_tier(
                        decoder.as_ref(),
                        &workspace,
                        &manifest,
                        fp_filter_ref,
                        novel_classifier_ref,
                    )?;
                    tiers.insert("chrono-replay".to_string(), result);
                }
            }
            // Workstream 0 (2026-07-06) — FP-on-noise guardrail. Manifest
            // defaults to the real 1000-file corpus but can be overridden
            // (e.g. by tests) via --noise-manifest.
            "noise_1000" => {
                let manifest = args.noise_manifest.clone().unwrap_or_else(|| {
                    workspace.join("research/corpus/curated/noise/noise_1000.manifest.json")
                });
                let manifest = if manifest.is_absolute() {
                    manifest
                } else {
                    workspace.join(&manifest)
                };
                anyhow::ensure!(
                    manifest.exists(),
                    "noise tier manifest missing at {}. Run: cargo run --release -p pancetta-research --bin gen-noise -- --count 1000 --seed 20260706 --birdie-fraction 0.3 --output-dir ~/.pancetta/recordings/noise_1000 --manifest research/corpus/curated/noise/noise_1000.manifest.json",
                    manifest.display()
                );
                let result = run_noise_tier(decoder.as_ref(), &manifest)?;
                tiers.insert("noise_1000".to_string(), result);
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

    // Task W0.3 (2026-07-06): compute real `RegressionFlags` by
    // self-diffing this run against the checked-in
    // `research/scorecards/main.json` baseline (best-effort — if it's
    // missing, unreadable, or on a different schema version, this run's
    // flags stay `RegressionFlags::default()`; a missing baseline is not
    // itself a regression). This is deliberately read BEFORE `card` is
    // written to `args.output` below, so the common `--output
    // research/scorecards/main.json` refresh recipe compares against the
    // PRIOR main.json, not the one it's about to overwrite.
    let main_baseline_path = workspace.join("research/scorecards/main.json");
    match Scorecard::load(&main_baseline_path) {
        Ok(baseline) => {
            card.regressions = compute_regression_flags(&baseline, &card);
            eprintln!(
                "regression-flags: computed vs {} (fixture_regression={}, false_positive_introduced={}, snr_curve_regression_db={:+.2})",
                main_baseline_path.display(),
                card.regressions.fixture_regression,
                card.regressions.false_positive_introduced,
                card.regressions.snr_curve_regression_db,
            );
        }
        Err(e) => {
            eprintln!(
                "regression-flags: no usable baseline at {} ({e}); regressions left at default (false/false/0.0)",
                main_baseline_path.display(),
            );
        }
    }

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

#[cfg(test)]
mod snr_interpolation_tests {
    use super::*;

    fn bin(snr_db: f64, attempts: u32, decoded: u32) -> SnrBin {
        SnrBin {
            snr_db,
            attempts,
            decoded,
            fp: 0,
        }
    }

    /// Task W0.2: SNR@50% must be genuine LINEAR INTERPOLATION between the
    /// two bins straddling the threshold, not "first bin >= threshold".
    /// -20dB: 2/10=0.20, -19dB: 6/10=0.60. Threshold=0.50 sits 75% of the
    /// way from 0.20 to 0.60, so the interpolated crossing is
    /// -20 + 0.75*(-19 - -20) = -19.25 dB — NOT -19 dB (what
    /// "first bin >= threshold" would return).
    #[test]
    fn snr_at_50pct_interpolates_between_straddling_bins() {
        let bins = vec![
            bin(-22.0, 10, 0),
            bin(-21.0, 10, 1),
            bin(-20.0, 10, 2),
            bin(-19.0, 10, 6),
            bin(-18.0, 10, 9),
        ];
        let got = first_threshold_db(&bins, 0.50).expect("must cross 50%");
        assert!(
            (got - (-19.25)).abs() < 1e-9,
            "expected -19.25 (linear interpolation), got {got}"
        );
    }

    #[test]
    fn snr_at_90pct_interpolates_between_straddling_bins() {
        // -18dB: 9/10=0.90 exactly -> should return -18.0 with no
        // interpolation needed (exact hit).
        let bins = vec![bin(-19.0, 10, 6), bin(-18.0, 10, 9), bin(-17.0, 10, 10)];
        let got = first_threshold_db(&bins, 0.90).expect("must cross 90%");
        assert!(
            (got - (-18.0)).abs() < 1e-9,
            "expected exact -18.0, got {got}"
        );
    }

    #[test]
    fn returns_none_when_threshold_never_reached() {
        let bins = vec![bin(-22.0, 10, 0), bin(-21.0, 10, 1)];
        assert_eq!(first_threshold_db(&bins, 0.90), None);
    }

    #[test]
    fn returns_edge_bin_when_first_bin_already_above_threshold() {
        // No lower bin to interpolate from -- report the edge, don't
        // fabricate a value below the corpus's actual range.
        let bins = vec![bin(-22.0, 10, 10), bin(-21.0, 10, 10)];
        assert_eq!(first_threshold_db(&bins, 0.50), Some(-22.0));
    }

    #[test]
    fn skips_zero_attempt_bins() {
        let bins = vec![bin(-22.0, 0, 0), bin(-21.0, 10, 2), bin(-20.0, 10, 8)];
        let got = first_threshold_db(&bins, 0.50).expect("must cross 50%");
        // interpolate between (-21, 0.2) and (-20, 0.8): frac=(0.5-0.2)/0.6=0.5
        assert!((got - (-20.5)).abs() < 1e-9, "got {got}");
    }
}

/// Task W0.3 (2026-07-06) — novel-decode classification tests. These call
/// `run_curated_tier` / `run_chrono_replay_tier` directly against a stub
/// `DecoderUnderTest` and a temp workspace (no real audio, no subprocess),
/// proving:
///
/// 1. the classification pass is genuinely report-only — every field that
///    feeds recall/composite is byte-identical whether or not a classifier
///    is supplied;
/// 2. verified vs unverified counts come out right given a known reference
///    set (built the same way `main` builds it, from
///    `research/baselines/ft8`).
#[cfg(test)]
mod novel_classification_tests {
    use super::*;
    use pancetta_research::chrono_replay::{ChronoReplayEntry, ChronoReplayManifest};
    use pancetta_research::curated::{CuratedEntry, CuratedManifest, ScoreBreakdown};
    use pancetta_research::decoder::Decode;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// A decoder stub that returns canned decodes keyed by the WAV's file
    /// name, ignoring actual file content — no real audio needed.
    struct StubDecoder {
        responses: HashMap<String, Vec<Decode>>,
    }

    impl DecoderUnderTest for StubDecoder {
        fn mode(&self) -> Mode {
            Mode::Ft8
        }
        fn identity(&self) -> String {
            "stub".to_string()
        }
        fn decode_wav(&self, path: &std::path::Path) -> anyhow::Result<Vec<Decode>> {
            let key = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            Ok(self.responses.get(key).cloned().unwrap_or_default())
        }
        fn config_snapshot(&self) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    fn decode(message: &str) -> Decode {
        Decode {
            message: message.to_string(),
            freq_hz: 1500.0,
            dt_s: 0.0,
            snr_db: -10.0,
            crc_valid: true,
            decode_time_into_window_s: None,
            soft_distance: None,
            hard_errors: None,
            coherence: None,
        }
    }

    /// Writes `wav_sha.json` (jt9's truth for the tier's one WAV) plus an
    /// unrelated `extra_reference_wav` baseline file into
    /// `workspace/research/baselines/ft8/` (seeding the novel-classifier's
    /// reference set with extra callsigns, exactly like the real corpus
    /// where the reference set spans every baseline file, not just the
    /// WAVs in the current tier). Returns the curated-manifest path.
    fn setup_curated(
        workspace: &std::path::Path,
        wav_sha: &str,
        jt9_decodes: &[&str],
        extra_reference_wav: Option<(&str, &[&str])>,
    ) -> PathBuf {
        let wav_dir = workspace.join("wavs");
        std::fs::create_dir_all(&wav_dir).unwrap();
        let wav_path = wav_dir.join(format!("{wav_sha}.wav"));
        std::fs::write(&wav_path, b"not real audio").unwrap();

        let baselines_dir = workspace.join("research/baselines/ft8");
        std::fs::create_dir_all(&baselines_dir).unwrap();
        write_baseline_cache(&baselines_dir, wav_sha, jt9_decodes);
        if let Some((extra_sha, extra_decodes)) = extra_reference_wav {
            write_baseline_cache(&baselines_dir, extra_sha, extra_decodes);
        }

        let manifest = CuratedManifest {
            schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
            label: "test".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),
            scoring_decoder: "stub".to_string(),
            entries: vec![CuratedEntry {
                wav_path,
                wav_sha256: wav_sha.to_string(),
                interest_score: 0.0,
                score_breakdown: ScoreBreakdown::default(),
            }],
        };
        let manifest_path = workspace.join("manifest.json");
        manifest.save(&manifest_path).unwrap();
        manifest_path
    }

    fn write_baseline_cache(baselines_dir: &std::path::Path, sha: &str, decodes: &[&str]) {
        let cache = serde_json::json!({
            "decodes": decodes
                .iter()
                .map(|m| serde_json::json!({"message": m}))
                .collect::<Vec<_>>(),
        });
        std::fs::write(
            baselines_dir.join(format!("{sha}.json")),
            serde_json::to_string(&cache).unwrap(),
        )
        .unwrap();
    }

    fn build_classifier(workspace: &std::path::Path) -> pancetta_research::FpFilter {
        let mut f = pancetta_research::FpFilter::new();
        f.extend_from_baselines(&workspace.join("research/baselines/ft8"))
            .unwrap();
        f
    }

    #[test]
    fn curated_tier_classification_is_report_only_and_correct() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let sha = "aaaa1111";
        // jt9 found K1ABC's CQ; pancetta finds it too PLUS two novels.
        let manifest_path = setup_curated(
            workspace,
            sha,
            &["CQ K1ABC FN42"],
            Some(("bbbb2222", &["W1AW K5ARH FN20"])), // seeds K5ARH into the reference set
        );

        let mut responses = HashMap::new();
        responses.insert(
            format!("{sha}.wav"),
            vec![
                decode("CQ K1ABC FN42"),     // matches jt9 -> recovered, not novel
                decode("K5ARH W9XYZ EM10"),  // novel; K5ARH IS in reference -> verified
                decode("ZZ0ZZZ AA0AA AA00"), // novel; nothing in reference -> unverified
            ],
        );
        let decoder = StubDecoder { responses };

        let without = run_curated_tier(&decoder, workspace, &manifest_path, None, None).unwrap();
        let classifier = build_classifier(workspace);
        let with =
            run_curated_tier(&decoder, workspace, &manifest_path, None, Some(&classifier)).unwrap();

        // Report-only invariant: every recall/composite-relevant field is
        // IDENTICAL whether or not the classifier ran.
        assert_eq!(without.truth_decodes_total, with.truth_decodes_total);
        assert_eq!(
            without.truth_decodes_recovered,
            with.truth_decodes_recovered
        );
        assert_eq!(without.decode_rate, with.decode_rate);
        assert_eq!(without.novel_decodes, with.novel_decodes);
        assert_eq!(without.wsjtx_decoded, with.wsjtx_decoded);
        assert_eq!(without.vs_wsjtx_pct, with.vs_wsjtx_pct);
        assert_eq!(without.per_wav_records.len(), with.per_wav_records.len());
        for (a, b) in without
            .per_wav_records
            .iter()
            .zip(with.per_wav_records.iter())
        {
            assert_eq!(a.truth, b.truth);
            assert_eq!(a.recovered, b.recovered);
            assert_eq!(a.novel, b.novel);
        }

        // Without a classifier: fields are None (not computed/omitted).
        assert_eq!(without.novels_verified, None);
        assert_eq!(without.novels_unverified, None);

        // With a classifier: 1 verified (K5ARH known), 1 unverified (ZZ0ZZZ unknown).
        assert_eq!(with.truth_decodes_recovered, Some(1));
        assert_eq!(with.novel_decodes, Some(2));
        assert_eq!(with.novels_verified, Some(1));
        assert_eq!(with.novels_unverified, Some(1));
    }

    #[test]
    fn chrono_replay_tier_classification_is_report_only_and_correct() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let sha = "cccc3333";

        let wav_dir = workspace.join("wavs");
        std::fs::create_dir_all(&wav_dir).unwrap();
        let wav_path = wav_dir.join(format!("{sha}.wav"));
        std::fs::write(&wav_path, b"not real audio").unwrap();

        let baselines_dir = workspace.join("research/baselines/ft8");
        std::fs::create_dir_all(&baselines_dir).unwrap();
        write_baseline_cache(&baselines_dir, sha, &["CQ K1ABC FN42"]);
        write_baseline_cache(&baselines_dir, "dddd4444", &["W1AW K5ARH FN20"]);

        let manifest = ChronoReplayManifest {
            schema_version: ChronoReplayManifest::CURRENT_SCHEMA_VERSION,
            label: "test".to_string(),
            generated_at: "2026-07-07T00:00:00Z".to_string(),
            source_session_label: "test_*".to_string(),
            first_wav_timestamp: "2026-07-07T00:00:00Z".to_string(),
            last_wav_timestamp: "2026-07-07T00:00:00Z".to_string(),
            span_seconds: 0.0,
            entries: vec![ChronoReplayEntry {
                wav_path,
                wav_sha256: sha.to_string(),
                slot_index: 0,
                wav_timestamp: "2026-07-07T00:00:00Z".to_string(),
            }],
        };
        let manifest_path = workspace.join("chrono.manifest.json");
        manifest.save(&manifest_path).unwrap();

        let mut responses = HashMap::new();
        responses.insert(
            format!("{sha}.wav"),
            vec![
                decode("CQ K1ABC FN42"),
                decode("K5ARH W9XYZ EM10"),
                decode("ZZ0ZZZ AA0AA AA00"),
            ],
        );
        let decoder = StubDecoder { responses };

        let without =
            run_chrono_replay_tier(&decoder, workspace, &manifest_path, None, None).unwrap();
        let classifier = build_classifier(workspace);
        let with =
            run_chrono_replay_tier(&decoder, workspace, &manifest_path, None, Some(&classifier))
                .unwrap();

        assert_eq!(without.novel_decodes, with.novel_decodes);
        assert_eq!(
            without.truth_decodes_recovered,
            with.truth_decodes_recovered
        );
        assert_eq!(without.novels_verified, None);
        assert_eq!(without.novels_unverified, None);
        assert_eq!(with.novel_decodes, Some(2));
        assert_eq!(with.novels_verified, Some(1));
        assert_eq!(with.novels_unverified, Some(1));
    }
}
